mod datex;
mod geo;
mod http;
mod matcher;
mod model;
mod publish;
mod sachsen;
mod sources;
mod traffic;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use log::{error, info, warn};
use serde::Deserialize;

use http::Http;
use matcher::Matcher;
use model::MatchStatus;
use publish::SourceState;
use sources::Source;
use traffic::LiveTraffic;

#[derive(Deserialize)]
struct Config {
    valhalla_url: String,
    /// Valhalla's live traffic extract, where active closures and slowdowns are written.
    traffic_extract: Option<PathBuf>,
    user_agent: String,
    out_dir: PathBuf,
    state_dir: PathBuf,
    #[serde(rename = "source")]
    sources: Vec<Source>,
}

const USAGE: &str = "usage: open-road-incidents <run|once|check SOURCE_ID> (config from $ORI_CONFIG or ./config.toml)";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let path = env::var("ORI_CONFIG").unwrap_or_else(|_| "config.toml".into());
    let config: Config = toml::from_str(&fs::read_to_string(&path).with_context(|| format!("reading {path}"))?)?;
    let args: Vec<String> = env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["run"] => run(&config, false),
        ["once"] => run(&config, true),
        ["check", id] => check(&config, id),
        _ => bail!(USAGE),
    }
}

fn run(config: &Config, once: bool) -> Result<()> {
    let http = Http::new(&config.user_agent);
    let mut matcher = Matcher::open(&config.valhalla_url, config.state_dir.join("match-cache.json"))?;
    let mut traffic = config
        .traffic_extract
        .as_deref()
        .map(|path| LiveTraffic::open(path, config.state_dir.join("traffic.json"), matcher.tileset()))
        .transpose()?;
    let enabled: Vec<&Source> = config.sources.iter().filter(|s| s.enabled).collect();
    let mut states: BTreeMap<String, SourceState> = BTreeMap::new();
    let mut attempts: BTreeMap<String, Instant> = BTreeMap::new();
    loop {
        let now = Utc::now();
        let mut changed = false;
        for src in &enabled {
            if attempts
                .get(&src.id)
                .is_some_and(|t| t.elapsed() < Duration::from_secs(src.poll_secs))
            {
                continue;
            }
            attempts.insert(src.id.clone(), Instant::now());
            let state = states.entry(src.id.clone()).or_default();
            match sources::fetch(src, &http, now) {
                Ok(mut incidents) => {
                    for inc in &mut incidents {
                        if let Err(e) = matcher.match_incident(inc) {
                            warn!("{}: {e:#}", inc.id);
                        }
                    }
                    log_matches(&src.id, &incidents);
                    state.incidents = incidents;
                    state.last_success = Some(now);
                    changed = true;
                }
                Err(e) => {
                    error!("{}: {e:#}", src.id);
                    http.forget(&src.url);
                    let stale = state
                        .last_success
                        .is_none_or(|t| (now - t).num_seconds() as u64 > src.stale_secs);
                    if stale && !state.incidents.is_empty() {
                        warn!("{}: data older than {}s, dropping it", src.id, src.stale_secs);
                        state.incidents.clear();
                        changed = true;
                    }
                }
            }
        }
        if changed {
            matcher.save()?;
            if let Some(traffic) = &mut traffic {
                let (closed, slowed) = traffic.apply(states.values().flat_map(|s| &s.incidents), now)?;
                info!("{closed} edges closed, {slowed} slowed");
            }
            publish::publish(&config.out_dir, &enabled, &states, now)?;
            info!("published to {}", config.out_dir.display());
        }
        if once {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(10));
    }
}

/// Fetches and matches one source, enabled or not, and prints a summary without publishing.
fn check(config: &Config, id: &str) -> Result<()> {
    let src = config
        .sources
        .iter()
        .find(|s| s.id == id)
        .with_context(|| format!("no source {id}"))?;
    let mut incidents = sources::fetch(src, &Http::new(&config.user_agent), Utc::now())?;
    let mut matcher = Matcher::open(&config.valhalla_url, config.state_dir.join("match-cache.json"))?;
    for inc in &mut incidents {
        matcher.match_incident(inc)?;
    }
    log_matches(id, &incidents);
    let closed = incidents.iter().filter(|i| i.road_closed).count();
    println!("{} incidents, {closed} closures", incidents.len());
    if let Some(first) = incidents.first() {
        println!("{}", serde_json::to_string_pretty(first)?);
    }
    Ok(())
}

fn log_matches(id: &str, incidents: &[model::Incident]) {
    let count = |s: MatchStatus| incidents.iter().filter(|i| i.match_status == s).count();
    info!(
        "{id}: {} matched, {} failed, {} pending",
        count(MatchStatus::Matched),
        count(MatchStatus::Failed),
        count(MatchStatus::Pending)
    );
}
