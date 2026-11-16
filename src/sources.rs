use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use log::info;
use serde::Deserialize;
use serde_json::Value;

use crate::catalunya;
use crate::datex;
use crate::http::Http;
use crate::model::{Coord, Incident, Kind, Shape};
use crate::sachsen;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    Datex,
    /// Fintraffic DATEX II for attributes plus its GeoJSON API for geometry.
    DigitrafficFi,
    Autobahn,
    /// Saxony's road works information system, a zip of GeoJSON.
    SachsenBis,
    /// The Servei Català de Trànsit's GML incidents.
    CatalunyaSct,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub id: String,
    pub country: String,
    pub format: Format,
    pub url: String,
    pub poll_secs: u64,
    #[serde(default = "default_stale")]
    pub stale_secs: u64,
    pub licence: String,
    pub attribution: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn default_stale() -> u64 {
    3600
}

fn default_true() -> bool {
    true
}

/// Fetches and parses one source. Expired incidents and ones without usable geometry are dropped.
pub fn fetch(src: &Source, http: &Http, now: DateTime<Utc>) -> Result<Vec<Incident>> {
    let mut incidents = match src.format {
        Format::Datex => datex::parse(&http.get_text(&src.url, &src.headers)?)?
            .into_iter()
            .map(|r| r.incident)
            .collect(),
        Format::DigitrafficFi => digitraffic(src, http)?,
        Format::Autobahn => autobahn(src, http)?,
        Format::SachsenBis => sachsen::parse(&http.get(&src.url, &src.headers)?)?,
        Format::CatalunyaSct => catalunya::parse(&http.get_text(&src.url, &src.headers)?)?,
    };
    let total = incidents.len();
    incidents.retain(|i| !i.expired(now) && !i.shape.is_empty());
    for inc in &mut incidents {
        inc.id = format!("{}:{}", src.id, inc.id);
        inc.source = src.id.clone();
        inc.country = src.country.clone();
    }
    info!("{}: {} records, {} kept", src.id, total, incidents.len());
    Ok(incidents)
}

fn digitraffic(src: &Source, http: &Http) -> Result<Vec<Incident>> {
    let mut out = Vec::new();
    for feed in ["roadworks", "traffic-announcements"] {
        let base = format!("{}/{feed}", src.url.trim_end_matches('/'));
        let geojson: Value = serde_json::from_slice(&http.get(&base, &src.headers)?)?;
        let shapes: HashMap<&str, Shape> = geojson["features"]
            .as_array()
            .context("missing features")?
            .iter()
            .filter_map(|f| Some((f["properties"]["situationId"].as_str()?, geojson_shape(&f["geometry"])?)))
            .collect();
        for rec in datex::parse(&http.get_text(&format!("{base}/datex2-3.7.xml"), &src.headers)?)? {
            let shape = rec.situation_id.as_deref().and_then(|id| shapes.get(id));
            if let Some(shape) = shape {
                let mut inc = rec.incident;
                inc.shape = shape.clone();
                out.push(inc);
            }
        }
    }
    Ok(out)
}

fn autobahn(src: &Source, http: &Http) -> Result<Vec<Incident>> {
    let base = src.url.trim_end_matches('/');
    let roads: Value = serde_json::from_slice(&http.get(&format!("{base}/"), &src.headers)?)?;
    let Some(roads) = roads["roads"].as_array() else {
        bail!("missing roads list")
    };
    let mut out = Vec::new();
    for road in roads.iter().filter_map(Value::as_str) {
        for (service, key) in [("roadworks", "roadworks"), ("closure", "closure")] {
            let url = format!("{base}/{}/services/{service}", road.replace(' ', "%20"));
            let body: Value = serde_json::from_slice(&http.get(&url, &src.headers)?)?;
            for item in body[key].as_array().into_iter().flatten() {
                if item["future"].as_bool() == Some(true) {
                    continue;
                }
                if let Some(inc) = autobahn_item(item, road) {
                    out.push(inc);
                }
            }
        }
    }
    Ok(out)
}

fn autobahn_item(item: &Value, road: &str) -> Option<Incident> {
    let display = item["display_type"].as_str().unwrap_or_default();
    let closure = display.starts_with("CLOSURE") || item["isBlocked"].as_str() == Some("true");
    let shape = geojson_shape(&item["geometry"]).or_else(|| {
        let c = &item["coordinate"];
        Some(Shape::Points(vec![[c["long"].as_f64()?, c["lat"].as_f64()?]]))
    })?;
    let kind = if display.contains("ROADWORKS") {
        Kind::Roadworks
    } else {
        Kind::Restriction
    };
    let mut inc = Incident::new(item["identifier"].as_str()?.to_string(), kind, shape);
    inc.road_closed = closure;
    inc.road = Some(road.to_string());
    inc.lang = Some("de".into());
    let lines: Vec<&str> = item["description"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    inc.description = Some(lines.join("\n").trim().to_string()).filter(|d| !d.is_empty());
    inc.valid_from = item["startTimestamp"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.to_utc());
    Some(inc)
}

fn geojson_shape(g: &Value) -> Option<Shape> {
    let line = |v: &Value| -> Option<Vec<Coord>> { v.as_array()?.iter().map(position).collect() };
    let c = &g["coordinates"];
    match g["type"].as_str()? {
        "LineString" => Some(Shape::Lines(vec![line(c)?])),
        "MultiLineString" => Some(Shape::Lines(c.as_array()?.iter().map(line).collect::<Option<_>>()?)),
        "Point" => Some(Shape::Points(vec![position(c)?])),
        "MultiPoint" => Some(Shape::Points(line(c)?)),
        _ => None,
    }
}

fn position(v: &Value) -> Option<Coord> {
    Some([v.get(0)?.as_f64()?, v.get(1)?.as_f64()?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn autobahn_closure() {
        let item = json!({
            "identifier": "2024-054366--vi-fbm.de16",
            "isBlocked": "false",
            "future": false,
            "display_type": "CLOSURE",
            "coordinate": {"lat": 53.505679, "long": 10.081479},
            "description": ["A1: Geesthacht -> Hamburg", "", "Länge: 0.2 km"],
            "geometry": {"type": "LineString", "coordinates": [[10.081479, 53.505679], [10.08099, 53.506987]]},
            "startTimestamp": "2026-11-14T03:00:00+01:00"
        });
        let inc = autobahn_item(&item, "A1").unwrap();
        assert!(inc.road_closed);
        assert_eq!(inc.kind, Kind::Restriction);
        assert_eq!(
            inc.description.as_deref(),
            Some("A1: Geesthacht -> Hamburg\n\nLänge: 0.2 km")
        );
        assert_eq!(inc.valid_from.unwrap().to_rfc3339(), "2026-11-14T02:00:00+00:00");
        assert_eq!(
            inc.shape,
            Shape::Lines(vec![vec![[10.081479, 53.505679], [10.08099, 53.506987]]])
        );
    }

    #[test]
    fn geojson_shapes() {
        let multi = json!({"type": "MultiLineString", "coordinates": [[[1.0, 2.0], [3.0, 4.0]]]});
        assert_eq!(
            geojson_shape(&multi),
            Some(Shape::Lines(vec![vec![[1.0, 2.0], [3.0, 4.0]]]))
        );
        let polygon = json!({"type": "MultiPolygon", "coordinates": []});
        assert_eq!(geojson_shape(&polygon), None);
    }
}
