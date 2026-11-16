use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use log::warn;
use serde::{Deserialize, Serialize};

use crate::model::{Edge, Incident, Kind};

const HEADER_BYTES: u64 = 32;
const UNKNOWN: u64 = 0;
/// Speed on lanes left open next to roadworks, as a share of the usual speed.
const RESTRICTED_LANES_SHARE: f64 = 0.6;
/// Speed past roadworks that report no lane or speed details, as a share of the usual speed.
const ROADWORKS_SHARE: f64 = 0.85;

struct Tile {
    offset: u64,
    edges: u32,
}

/// Writes closures and slowdowns into Valhalla's live traffic extract.
pub struct LiveTraffic {
    file: File,
    tiles: HashMap<u64, Tile>,
    state_path: PathBuf,
    state: State,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    tileset: String,
    /// Edge id to the value written for it.
    written: BTreeMap<u64, u64>,
}

impl LiveTraffic {
    pub fn open(traffic: &Path, state_path: PathBuf, tileset: &str) -> Result<Self> {
        // The file is changed in place. Valhalla maps it into memory, so replacing it would leave the
        // server reading the old copy.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(traffic)
            .with_context(|| format!("opening {}", traffic.display()))?;
        let tiles = index(&file)?;
        let state = std::fs::read(&state_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<State>(&b).ok())
            .filter(|s| s.tileset == tileset)
            .unwrap_or_else(|| State {
                tileset: tileset.to_string(),
                written: BTreeMap::new(),
            });
        Ok(LiveTraffic {
            file,
            tiles,
            state_path,
            state,
        })
    }

    /// Applies every incident active at `now` and resets edges no longer affected. Returns how many
    /// edges are closed and how many slowed down.
    pub fn apply<'a>(
        &mut self,
        incidents: impl IntoIterator<Item = &'a Incident>,
        now: DateTime<Utc>,
    ) -> Result<(usize, usize)> {
        let mut speeds: BTreeMap<u64, u32> = BTreeMap::new();
        for incident in incidents.into_iter().filter(|i| active(i, now)) {
            for edge in &incident.edges {
                if let Some(kph) = speed(incident, edge) {
                    speeds.entry(edge.id).and_modify(|s| *s = (*s).min(kph)).or_insert(kph);
                }
            }
        }
        let wanted: BTreeMap<u64, u64> = speeds.iter().map(|(&id, &kph)| (id, encode(kph))).collect();
        for &edge in self.state.written.keys().filter(|e| !wanted.contains_key(e)) {
            self.write(edge, UNKNOWN)?;
        }
        for (&edge, &value) in wanted.iter().filter(|(e, v)| self.state.written.get(e) != Some(v)) {
            self.write(edge, value)?;
        }
        let closed = speeds.values().filter(|&&kph| kph == 0).count();
        let slowed = speeds.len() - closed;
        self.state.written = wanted;
        crate::publish::write_atomic(&self.state_path, &serde_json::to_vec(&self.state)?)?;
        Ok((closed, slowed))
    }

    fn write(&self, edge: u64, value: u64) -> Result<()> {
        let tile_id = edge & ((1 << 25) - 1);
        let index = edge >> 25;
        let Some(tile) = self.tiles.get(&tile_id).filter(|t| index < u64::from(t.edges)) else {
            warn!("edge {edge} is not in the traffic extract");
            return Ok(());
        };
        self.file
            .write_all_at(&value.to_le_bytes(), tile.offset + HEADER_BYTES + index * 8)
            .context("writing traffic extract")
    }
}

/// The speed `incident` leaves on `edge` in km/h, 0 for closed, or `None` when it does not slow traffic.
fn speed(incident: &Incident, edge: &Edge) -> Option<u32> {
    if incident.road_closed {
        return Some(0);
    }
    let usual = f64::from(edge.kph);
    let mut kph = usual;
    if let Some(limit) = incident.speed_limit {
        kph = kph.min(f64::from(limit));
    }
    if incident.lanes_restricted {
        kph = kph.min(usual * RESTRICTED_LANES_SHARE);
    } else if incident.kind == Kind::Roadworks && incident.speed_limit.is_none() {
        kph = kph.min(usual * ROADWORKS_SHARE);
    }
    (kph < usual).then(|| (kph.round() as u32).max(2))
}

/// A Valhalla `TrafficSpeed` for the whole edge. Speeds are stored in 2 km/h steps, and breakpoint1 = 255
/// marks one speed for the full length. All speeds 0 means closed.
fn encode(kph: u32) -> u64 {
    let step = u64::from((kph / 2).min(126));
    step | step << 7 | 255 << 28
}

fn active(i: &Incident, now: DateTime<Utc>) -> bool {
    !i.map_only && i.valid_from.is_none_or(|t| t <= now) && !i.expired(now)
}

fn index(file: &File) -> Result<HashMap<u64, Tile>> {
    let mut archive = tar::Archive::new(file);
    let mut tiles = HashMap::new();
    for entry in archive.entries_with_seek()? {
        let entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let Some(id) = tile_id(&path) else {
            continue;
        };
        let offset = entry.raw_file_position();
        let mut count = [0u8; 4];
        file.read_exact_at(&mut count, offset + 16)?;
        tiles.insert(
            id,
            Tile {
                offset,
                edges: u32::from_le_bytes(count),
            },
        );
    }
    Ok(tiles)
}

/// Valhalla's numeric tile id for a path like `2/000/807/143.gph`.
fn tile_id(path: &str) -> Option<u64> {
    let mut parts = path.strip_suffix(".gph")?.split('/');
    let level: u64 = parts.next()?.parse().ok()?;
    let index: u64 = parts.collect::<String>().parse().ok()?;
    Some(level | (index << 3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Shape;
    use chrono::Duration;

    fn traffic_tar(dir: &Path, edges: u32) -> PathBuf {
        let path = dir.join("traffic.tar");
        let mut builder = tar::Builder::new(File::create(&path).unwrap());
        let mut data = Vec::new();
        data.extend_from_slice(&tile_id("2/000/807/143.gph").unwrap().to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&edges.to_le_bytes());
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(&[0; 8]);
        data.resize(data.len() + edges as usize * 8, 0);
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "2/000/807/143.gph", &data[..])
            .unwrap();
        builder.finish().unwrap();
        path
    }

    fn stored(path: &Path, edge: u64) -> u64 {
        let file = File::open(path).unwrap();
        let tile = &index(&file).unwrap()[&(edge & ((1 << 25) - 1))];
        let mut b = [0u8; 8];
        file.read_exact_at(&mut b, tile.offset + HEADER_BYTES + (edge >> 25) * 8)
            .unwrap();
        u64::from_le_bytes(b)
    }

    fn edge(i: u64) -> Edge {
        Edge {
            id: tile_id("2/000/807/143.gph").unwrap() | (i << 25),
            kph: 100,
        }
    }

    fn incident(kind: Kind, edges: Vec<Edge>, from: Option<DateTime<Utc>>) -> Incident {
        let mut i = Incident::new("x".into(), kind, Shape::Points(vec![[0.0, 0.0]]));
        i.valid_from = from;
        i.edges = edges;
        i
    }

    fn closure(edges: Vec<Edge>, from: Option<DateTime<Utc>>) -> Incident {
        let mut i = incident(Kind::Roadworks, edges, from);
        i.road_closed = true;
        i
    }

    #[test]
    fn map_only_incidents_leave_routing_alone() {
        let dir = std::env::temp_dir().join(format!("ori-traffic-map-only-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tar = traffic_tar(&dir, 4);
        let now = Utc::now();
        let mut traffic = LiveTraffic::open(&tar, dir.join("state.json"), "t1").unwrap();
        let mut shown = closure(vec![edge(1)], Some(now - Duration::hours(1)));
        shown.map_only = true;

        traffic.apply([&shown], now).unwrap();

        assert_eq!(stored(&tar, edge(1).id), UNKNOWN);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn closes_active_edges_and_reopens_ended_ones() {
        let dir = std::env::temp_dir().join(format!("ori-traffic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tar = traffic_tar(&dir, 4);
        let now = Utc::now();
        let mut traffic = LiveTraffic::open(&tar, dir.join("state.json"), "t1").unwrap();

        let current = closure(vec![edge(1)], Some(now - Duration::hours(1)));
        let planned = closure(vec![edge(2)], Some(now + Duration::hours(1)));
        let outside = closure(vec![edge(9)], None);
        assert_eq!(traffic.apply([&current, &planned, &outside], now).unwrap(), (2, 0));
        assert_eq!(stored(&tar, edge(1).id), encode(0));
        assert_eq!(stored(&tar, edge(2).id), UNKNOWN);

        let mut reopened = LiveTraffic::open(&tar, dir.join("state.json"), "t1").unwrap();
        reopened.apply([&planned], now + Duration::hours(2)).unwrap();
        assert_eq!(stored(&tar, edge(1).id), UNKNOWN);
        assert_eq!(stored(&tar, edge(2).id), encode(0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn slows_roadworks_and_keeps_the_strongest_effect() {
        let mut lanes = incident(Kind::Roadworks, vec![edge(1)], None);
        lanes.lanes_restricted = true;
        let mut limit = incident(Kind::Restriction, vec![edge(1)], None);
        limit.speed_limit = Some(80);
        let plain = incident(Kind::Roadworks, vec![edge(1)], None);
        let congestion = incident(Kind::Congestion, vec![edge(1)], None);
        assert_eq!(speed(&lanes, &edge(1)), Some(60));
        assert_eq!(speed(&limit, &edge(1)), Some(80));
        assert_eq!(speed(&plain, &edge(1)), Some(85));
        assert_eq!(speed(&congestion, &edge(1)), None);

        let dir = std::env::temp_dir().join(format!("ori-slowdown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tar = traffic_tar(&dir, 4);
        let mut traffic = LiveTraffic::open(&tar, dir.join("state.json"), "t1").unwrap();
        assert_eq!(traffic.apply([&limit, &lanes, &plain], Utc::now()).unwrap(), (0, 1));
        assert_eq!(stored(&tar, edge(1).id), encode(60));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn encodes_speeds_like_valhalla() {
        assert_eq!(encode(0), 255 << 28);
        assert_eq!(encode(60), 30 | 30 << 7 | 255 << 28);
        assert_eq!(encode(400) & 0x7f, 126);
    }

    #[test]
    fn reads_tile_ids_from_paths() {
        assert_eq!(tile_id("2/000/807/143.gph"), Some(2 | (807143 << 3)));
        assert_eq!(tile_id("0/003/106.gph"), Some(3106 << 3));
        assert_eq!(tile_id("index.bin"), None);
    }
}
