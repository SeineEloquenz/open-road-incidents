use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use log::warn;
use serde::{Deserialize, Serialize};

use crate::model::Incident;

const HEADER_BYTES: u64 = 32;
/// A Valhalla `TrafficSpeed` with breakpoint1 = 255 and all speeds 0, which Valhalla reads as closed.
const CLOSED: u64 = 255 << 28;
const UNKNOWN: u64 = 0;

struct Tile {
    offset: u64,
    edges: u32,
}

/// Closes Valhalla edges through its live traffic extract.
pub struct Closures {
    file: File,
    tiles: HashMap<u64, Tile>,
    state_path: PathBuf,
    state: State,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    tileset: String,
    closed: BTreeSet<u64>,
}

impl Closures {
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
                closed: BTreeSet::new(),
            });
        Ok(Closures {
            file,
            tiles,
            state_path,
            state,
        })
    }

    /// Closes the edges of every closure active at `now` and reopens edges no longer closed.
    pub fn apply<'a>(
        &mut self,
        incidents: impl IntoIterator<Item = &'a Incident>,
        now: DateTime<Utc>,
    ) -> Result<usize> {
        let closed: BTreeSet<u64> = incidents
            .into_iter()
            .filter(|i| i.road_closed && active(i, now))
            .flat_map(|i| i.edge_ids.iter().copied())
            .collect();
        for &edge in self.state.closed.difference(&closed) {
            self.write(edge, UNKNOWN)?;
        }
        for &edge in closed.difference(&self.state.closed) {
            self.write(edge, CLOSED)?;
        }
        let count = closed.len();
        self.state.closed = closed;
        crate::publish::write_atomic(&self.state_path, &serde_json::to_vec(&self.state)?)?;
        Ok(count)
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

fn active(i: &Incident, now: DateTime<Utc>) -> bool {
    i.valid_from.is_none_or(|t| t <= now) && !i.expired(now)
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
    use crate::model::{Kind, Shape};
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

    fn speed(path: &Path, edge: u64) -> u64 {
        let file = File::open(path).unwrap();
        let tile = &index(&file).unwrap()[&(edge & ((1 << 25) - 1))];
        let mut b = [0u8; 8];
        file.read_exact_at(&mut b, tile.offset + HEADER_BYTES + (edge >> 25) * 8)
            .unwrap();
        u64::from_le_bytes(b)
    }

    fn closure(edges: Vec<u64>, from: Option<DateTime<Utc>>, until: Option<DateTime<Utc>>) -> Incident {
        let mut i = Incident::new("x".into(), Kind::Roadworks, Shape::Points(vec![[0.0, 0.0]]));
        i.road_closed = true;
        i.valid_from = from;
        i.valid_until = until;
        i.edge_ids = edges;
        i
    }

    #[test]
    fn closes_active_edges_and_reopens_ended_ones() {
        let dir = std::env::temp_dir().join(format!("ori-closures-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tar = traffic_tar(&dir, 4);
        let tile = tile_id("2/000/807/143.gph").unwrap();
        let edge = |i: u64| tile | (i << 25);
        let now = Utc::now();
        let mut closures = Closures::open(&tar, dir.join("state.json"), "t1").unwrap();

        let current = closure(vec![edge(1)], Some(now - Duration::hours(1)), None);
        let planned = closure(vec![edge(2)], Some(now + Duration::hours(1)), None);
        let outside = closure(vec![edge(9)], None, None);
        assert_eq!(closures.apply([&current, &planned, &outside], now).unwrap(), 2);
        assert_eq!(speed(&tar, edge(1)), CLOSED);
        assert_eq!(speed(&tar, edge(2)), UNKNOWN);

        let mut reopened = Closures::open(&tar, dir.join("state.json"), "t1").unwrap();
        reopened.apply([&planned], now + Duration::hours(2)).unwrap();
        assert_eq!(speed(&tar, edge(1)), UNKNOWN);
        assert_eq!(speed(&tar, edge(2)), CLOSED);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_tile_ids_from_paths() {
        assert_eq!(tile_id("2/000/807/143.gph"), Some(2 | (807143 << 3)));
        assert_eq!(tile_id("0/003/106.gph"), Some(3106 << 3));
        assert_eq!(tile_id("index.bin"), None);
    }
}
