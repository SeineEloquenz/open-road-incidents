use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::geo;
use crate::model::{Coord, Incident, MatchStatus, Shape};

const MAX_MEAN_OFFSET_M: f64 = 30.0;
const MAX_UNMATCHED_SHARE: f64 = 0.2;
const MAX_POINT_OFFSET_M: f64 = 50.0;
const MAX_DETOUR_FACTOR: f64 = 3.0;

#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    way_ids: Option<Vec<i64>>,
}

/// Matches incident geometry to OSM ways through a Valhalla server, caching results per tileset.
pub struct Matcher {
    agent: ureq::Agent,
    base: String,
    tileset: String,
    cache_path: PathBuf,
    cache: HashMap<u64, Cached>,
    used: HashSet<u64>,
}

impl Matcher {
    pub fn open(base: &str, cache_path: PathBuf) -> Result<Self> {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).build();
        let base = base.trim_end_matches('/').to_string();
        let status: Value = agent
            .get(&format!("{base}/status"))
            .call()
            .context("Valhalla unreachable")?
            .into_json()?;
        let tileset = status["tileset_last_modified"].to_string();
        let cache = fs::read(&cache_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Matcher {
            agent,
            base,
            tileset,
            cache_path,
            cache,
            used: HashSet::new(),
        })
    }

    /// Fills `osm_way_ids` and `match_status`. Errors mean Valhalla itself failed and the incident stays pending.
    pub fn match_incident(&mut self, inc: &mut Incident) -> Result<()> {
        let key = geo::stable_hash(&serde_json::to_vec(&(&inc.shape, &self.tileset))?);
        self.used.insert(key);
        let cached = match self.cache.get(&key) {
            Some(c) => c.clone(),
            None => {
                let c = Cached {
                    way_ids: self.match_shape(&inc.shape)?,
                };
                self.cache.insert(key, c.clone());
                c
            }
        };
        match cached.way_ids {
            Some(ids) => {
                inc.osm_way_ids = ids;
                inc.match_status = MatchStatus::Matched;
            }
            None => inc.match_status = MatchStatus::Failed,
        }
        Ok(())
    }

    /// Persists the cache, dropping entries no current incident used.
    pub fn save(&mut self) -> Result<()> {
        self.cache.retain(|k, _| self.used.contains(k));
        self.used.clear();
        crate::publish::write_atomic(&self.cache_path, &serde_json::to_vec(&self.cache)?)
    }

    fn match_shape(&self, shape: &Shape) -> Result<Option<Vec<i64>>> {
        let mut ids = Vec::new();
        let mut any = false;
        let parts: Vec<Option<Vec<i64>>> = match shape {
            Shape::Lines(lines) => lines
                .iter()
                .filter(|l| l.len() >= 2)
                .map(|l| self.trace(l, "map_snap"))
                .collect::<Result<_>>()?,
            Shape::Spans(spans) => spans.iter().map(|s| self.span(*s)).collect::<Result<_>>()?,
            Shape::Points(points) => points.iter().map(|p| self.locate(*p)).collect::<Result<_>>()?,
        };
        for part in parts.into_iter().flatten() {
            any = true;
            for id in part {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        Ok(any.then_some(ids))
    }

    fn trace(&self, line: &[Coord], mode: &str) -> Result<Option<Vec<i64>>> {
        let body = json!({
            "shape": line.iter().map(|c| json!({"lat": c[1], "lon": c[0]})).collect::<Vec<_>>(),
            "costing": "auto",
            "shape_match": mode,
            "filters": {"attributes": ["edge.way_id", "matched.type", "matched.distance_from_trace_point"], "action": "include"},
        });
        let Some(resp) = self.post("trace_attributes", body)? else {
            return Ok(None);
        };
        if let Some(points) = resp["matched_points"].as_array().filter(|p| !p.is_empty()) {
            let matched: Vec<f64> = points
                .iter()
                .filter(|p| p["type"] != "unmatched")
                .filter_map(|p| p["distance_from_trace_point"].as_f64())
                .collect();
            let unmatched = 1.0 - matched.len() as f64 / points.len() as f64;
            let mean = matched.iter().sum::<f64>() / matched.len().max(1) as f64;
            if unmatched > MAX_UNMATCHED_SHARE || mean > MAX_MEAN_OFFSET_M {
                return Ok(None);
            }
        }
        let ids: Vec<i64> = resp["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["way_id"].as_i64())
            .collect();
        Ok(Some(ids).filter(|ids| !ids.is_empty()))
    }

    fn span(&self, [from, to]: [Coord; 2]) -> Result<Option<Vec<i64>>> {
        let body = json!({
            "locations": [{"lat": from[1], "lon": from[0]}, {"lat": to[1], "lon": to[0]}],
            "costing": "auto",
            "directions_type": "none",
        });
        let Some(resp) = self.post("route", body)? else {
            return Ok(None);
        };
        let leg = &resp["trip"]["legs"][0];
        let route_m = leg["summary"]["length"].as_f64().unwrap_or(f64::MAX) * 1000.0;
        if route_m > geo::distance(from, to).max(100.0) * MAX_DETOUR_FACTOR {
            return Ok(None);
        }
        let path = geo::decode_polyline(leg["shape"].as_str().unwrap_or_default(), 1e6);
        if path.len() < 2 {
            return Ok(None);
        }
        self.trace(&path, "edge_walk")
    }

    fn locate(&self, p: Coord) -> Result<Option<Vec<i64>>> {
        let body = json!({"locations": [{"lat": p[1], "lon": p[0]}], "costing": "auto"});
        let Some(resp) = self.post("locate", body)? else {
            return Ok(None);
        };
        let nearest = resp[0]["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let d = geo::distance(p, [e["correlated_lon"].as_f64()?, e["correlated_lat"].as_f64()?]);
                Some((d, e["way_id"].as_i64()?))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0));
        Ok(nearest
            .filter(|(d, _)| *d <= MAX_POINT_OFFSET_M)
            .map(|(_, id)| vec![id]))
    }

    /// Returns `None` when Valhalla rejects the request, which means the shape cannot be matched.
    fn post(&self, action: &str, body: Value) -> Result<Option<Value>> {
        match self.agent.post(&format!("{}/{action}", self.base)).send_json(body) {
            Ok(resp) => Ok(Some(resp.into_json()?)),
            Err(ureq::Error::Status(400, _)) => Ok(None),
            Err(e) => bail!("Valhalla {action}: {e}"),
        }
    }
}
