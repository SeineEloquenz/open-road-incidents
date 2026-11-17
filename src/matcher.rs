use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::geo;
use crate::model::{Coord, Edge, Incident, MatchStatus, Shape};

const MAX_MEAN_OFFSET_M: f64 = 30.0;
const MAX_UNMATCHED_SHARE: f64 = 0.2;
const MAX_POINT_OFFSET_M: f64 = 50.0;
const MAX_NAMED_POINT_OFFSET_M: f64 = 200.0;
const MAX_DETOUR_FACTOR: f64 = 3.0;

const CACHE_VERSION: u32 = 4;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Matched {
    way_ids: Vec<i64>,
    edges: Vec<Edge>,
}

impl Matched {
    fn extend(&mut self, other: Matched) {
        for id in other.way_ids {
            if !self.way_ids.contains(&id) {
                self.way_ids.push(id);
            }
        }
        for edge in other.edges {
            if !self.edges.iter().any(|e| e.id == edge.id) {
                self.edges.push(edge);
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    matched: Option<Matched>,
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

    /// Fills `osm_way_ids`, `edges` and `match_status`. Errors mean Valhalla itself failed and the incident stays pending.
    pub fn match_incident(&mut self, inc: &mut Incident) -> Result<()> {
        let key = geo::stable_hash(&serde_json::to_vec(&(
            &inc.shape,
            inc.both_directions,
            &inc.road,
            &self.tileset,
            CACHE_VERSION,
        ))?);
        self.used.insert(key);
        let cached = match self.cache.get(&key) {
            Some(c) => c.clone(),
            None => {
                let c = Cached {
                    matched: self.match_shape(&inc.shape, inc.both_directions, inc.road.as_deref())?,
                };
                self.cache.insert(key, c.clone());
                c
            }
        };
        match cached.matched {
            Some(m) => {
                inc.osm_way_ids = m.way_ids;
                inc.edges = m.edges;
                inc.match_status = MatchStatus::Matched;
            }
            None => inc.match_status = MatchStatus::Failed,
        }
        Ok(())
    }

    pub fn tileset(&self) -> &str {
        &self.tileset
    }

    /// Persists the cache, dropping entries no current incident used.
    pub fn save(&mut self) -> Result<()> {
        self.cache.retain(|k, _| self.used.contains(k));
        self.used.clear();
        crate::publish::write_atomic(&self.cache_path, &serde_json::to_vec(&self.cache)?)
    }

    fn match_shape(&self, shape: &Shape, both_directions: bool, road: Option<&str>) -> Result<Option<Matched>> {
        let mut all = Matched::default();
        let mut any = false;
        let parts: Vec<Option<Matched>> = match shape {
            Shape::Lines(lines) => lines
                .iter()
                .filter(|l| l.len() >= 2)
                .flat_map(|l| {
                    let reversed = both_directions.then(|| l.iter().rev().copied().collect::<Vec<_>>());
                    [Some(l.clone()), reversed].into_iter().flatten()
                })
                .map(|l| self.trace(&l, "map_snap"))
                .collect::<Result<_>>()?,
            Shape::Spans(spans) => spans.iter().map(|s| self.span(*s)).collect::<Result<_>>()?,
            Shape::Points(points) => points.iter().map(|p| self.locate(*p, road)).collect::<Result<_>>()?,
        };
        for part in parts.into_iter().flatten() {
            any = true;
            all.extend(part);
        }
        Ok(any.then_some(all))
    }

    fn trace(&self, line: &[Coord], mode: &str) -> Result<Option<Matched>> {
        let body = json!({
            "shape": line.iter().map(|c| json!({"lat": c[1], "lon": c[0]})).collect::<Vec<_>>(),
            "costing": "auto",
            "shape_match": mode,
            "filters": {"attributes": ["edge.way_id", "edge.id", "edge.speed", "matched.type", "matched.distance_from_trace_point"], "action": "include"},
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
        let mut matched = Matched::default();
        for e in resp["edges"].as_array().into_iter().flatten() {
            matched.extend(Matched {
                way_ids: e["way_id"].as_i64().into_iter().collect(),
                edges: edge(e["id"].as_u64(), e["speed"].as_u64()).into_iter().collect(),
            });
        }
        Ok(Some(matched).filter(|m| !m.way_ids.is_empty()))
    }

    fn span(&self, [from, to]: [Coord; 2]) -> Result<Option<Matched>> {
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

    /// Matches the nearest road segment in both directions, since a point does not say which one is affected.
    /// A segment of the named `road` may lie further off, as some sources place points beside the carriageway.
    fn locate(&self, p: Coord, road: Option<&str>) -> Result<Option<Matched>> {
        let body = json!({"locations": [{"lat": p[1], "lon": p[0]}], "costing": "auto", "verbose": true});
        let Some(resp) = self.post("locate", body)? else {
            return Ok(None);
        };
        let candidates: Vec<(f64, i64, Option<Edge>, bool)> = resp[0]["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let d = geo::distance(p, [e["correlated_lon"].as_f64()?, e["correlated_lat"].as_f64()?]);
                let way = e["edge_info"]["way_id"].as_i64()?;
                let named = road.is_some_and(|r| {
                    e["edge_info"]["names"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .any(|n| same_road(n, r))
                });
                Some((
                    d,
                    way,
                    edge(e["edge_id"]["value"].as_u64(), e["edge"]["speeds"]["default"].as_u64()),
                    named,
                ))
            })
            .collect();
        let nearest = candidates.iter().min_by(|a, b| a.0.total_cmp(&b.0));
        let Some(&(d, way, _, _)) = nearest.filter(|c| c.0 <= MAX_POINT_OFFSET_M).or_else(|| {
            candidates
                .iter()
                .filter(|c| c.3 && c.0 <= MAX_NAMED_POINT_OFFSET_M)
                .min_by(|a, b| a.0.total_cmp(&b.0))
        }) else {
            return Ok(None);
        };
        Ok(Some(Matched {
            way_ids: vec![way],
            edges: candidates
                .iter()
                .filter(|c| c.1 == way && (c.0 - d).abs() < 1.0)
                .filter_map(|c| c.2)
                .collect(),
        }))
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

/// Compares road numbers ignoring case, spaces and dashes, so "AP-7" matches "AP 7".
fn same_road(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && a == b
}

fn edge(id: Option<u64>, kph: Option<u64>) -> Option<Edge> {
    Some(Edge {
        id: id?,
        kph: u32::try_from(kph?).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn road_numbers_match_regardless_of_separators() {
        assert!(same_road("AP-7", "AP 7"));
        assert!(same_road("c-25", "C-25"));
        assert!(!same_road("C-25", "C-251"));
        assert!(!same_road("", " - "));
    }
}
