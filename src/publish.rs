use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use log::warn;
use serde_json::{Value, json};

use crate::model::{Incident, Shape};
use crate::sources::Source;

#[derive(Default)]
pub struct SourceState {
    pub incidents: Vec<Incident>,
    pub last_success: Option<DateTime<Utc>>,
}

/// Writes per-country GeoJSON, avoidance files, source metadata and vector tiles under `out/v1`.
pub fn publish(
    out: &Path,
    sources: &[&Source],
    states: &BTreeMap<String, SourceState>,
    now: DateTime<Utc>,
) -> Result<()> {
    let root = out.join("v1");
    fs::create_dir_all(root.join("avoid"))?;
    let mut by_country: BTreeMap<String, Vec<(&Incident, &Source)>> = BTreeMap::new();
    for src in sources {
        by_country.entry(src.country.to_lowercase()).or_default();
        for inc in states.get(&src.id).map(|s| &s.incidents[..]).unwrap_or_default() {
            if !inc.expired(now) {
                by_country
                    .get_mut(&src.country.to_lowercase())
                    .unwrap()
                    .push((inc, src));
            }
        }
    }

    let mut all = Vec::new();
    for (cc, items) in &by_country {
        let features: Vec<Value> = items.iter().map(|(i, s)| feature(i, s)).collect();
        write_json(
            &root.join(format!("{cc}.geojson")),
            &json!({"type": "FeatureCollection", "features": features}),
        )?;
        all.extend(features);

        let closures: Vec<Value> = items
            .iter()
            .filter(|(i, _)| i.road_closed && !i.osm_way_ids.is_empty())
            .map(|(i, _)| {
                json!({"id": i.id, "road": i.road, "valid_from": i.valid_from, "valid_until": i.valid_until, "osm_way_ids": i.osm_way_ids})
            })
            .collect();
        write_json(
            &root.join("avoid").join(format!("{cc}.json")),
            &json!({"generated_at": now, "closures": closures}),
        )?;
    }

    let meta: Vec<Value> = sources
        .iter()
        .map(|s| {
            let st = states.get(&s.id);
            json!({
                "id": s.id,
                "country": s.country,
                "licence": s.licence,
                "attribution": s.attribution,
                "last_success": st.and_then(|st| st.last_success),
                "incidents": st.map_or(0, |st| st.incidents.len()),
            })
        })
        .collect();
    write_json(
        &root.join("sources.json"),
        &json!({"generated_at": now, "sources": meta}),
    )?;

    if let Err(e) = tiles(&root, &json!({"type": "FeatureCollection", "features": all})) {
        warn!("vector tiles not updated: {e:#}");
    }
    Ok(())
}

fn feature(i: &Incident, src: &Source) -> Value {
    let geometry = match &i.shape {
        Shape::Lines(lines) => json!({"type": "MultiLineString", "coordinates": lines}),
        Shape::Spans(spans) => json!({"type": "MultiLineString", "coordinates": spans}),
        Shape::Points(points) => json!({"type": "MultiPoint", "coordinates": points}),
    };
    json!({
        "type": "Feature",
        "geometry": geometry,
        "properties": {
            "id": i.id,
            "source": i.source,
            "kind": i.kind,
            "road_closed": i.road_closed,
            "lanes_restricted": i.lanes_restricted,
            "speed_limit": i.speed_limit,
            "severity": i.severity,
            "road": i.road,
            "valid_from": i.valid_from,
            "valid_until": i.valid_until,
            "description": i.description,
            "lang": i.lang,
            "geometry_exact": !matches!(i.shape, Shape::Spans(_)),
            "osm_way_ids": i.osm_way_ids,
            "match_status": i.match_status,
            "licence": src.licence,
            "attribution": src.attribution,
        }
    })
}

fn tiles(root: &Path, collection: &Value) -> Result<()> {
    let input = root.join("tiles-input.geojson");
    let fresh = root.join("tiles.new");
    let current = root.join("tiles");
    let old = root.join("tiles.old");
    write_json(&input, collection)?;
    let _ = fs::remove_dir_all(&fresh);
    let status = Command::new("tippecanoe")
        .args([
            "--quiet",
            "--force",
            "--no-tile-compression",
            "-l",
            "incidents",
            "-Z6",
            "-z12",
            "-r1",
            "-e",
        ])
        .arg(&fresh)
        .arg(&input)
        .status()
        .context("running tippecanoe")?;
    fs::remove_file(&input)?;
    anyhow::ensure!(status.success(), "tippecanoe exited with {status}");
    let _ = fs::remove_dir_all(&old);
    if current.exists() {
        fs::rename(&current, &old)?;
    }
    fs::rename(&fresh, &current)?;
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    write_atomic(path, &serde_json::to_vec(value)?)
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}
