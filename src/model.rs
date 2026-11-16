use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `[lon, lat]` in WGS84.
pub type Coord = [f64; 2];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Roadworks,
    Accident,
    Obstruction,
    Conditions,
    Congestion,
    Restriction,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// Exact road geometry from the source.
    Lines(Vec<Vec<Coord>>),
    /// Only start and end points are known, the road between them is not.
    Spans(Vec<[Coord; 2]>),
    Points(Vec<Coord>),
}

impl Shape {
    pub fn is_empty(&self) -> bool {
        match self {
            Shape::Lines(v) => v.is_empty(),
            Shape::Spans(v) => v.is_empty(),
            Shape::Points(v) => v.is_empty(),
        }
    }
}

/// A Valhalla directed edge and its usual speed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: u64,
    pub kph: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    Pending,
    Matched,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    pub id: String,
    pub source: String,
    pub country: String,
    pub kind: Kind,
    pub road_closed: bool,
    pub lanes_restricted: bool,
    pub speed_limit: Option<u32>,
    pub severity: Option<String>,
    pub road: Option<String>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub lang: Option<String>,
    pub shape: Shape,
    /// Affects both directions of the road although its line runs along one.
    #[serde(default)]
    pub both_directions: bool,
    /// Shown on the map but left out of routing.
    #[serde(default)]
    pub map_only: bool,
    pub osm_way_ids: Vec<i64>,
    /// Valhalla directed edges, only valid for the tile set they were matched on.
    #[serde(skip)]
    pub edges: Vec<Edge>,
    pub match_status: MatchStatus,
}

impl Incident {
    pub fn new(id: String, kind: Kind, shape: Shape) -> Self {
        Incident {
            id,
            source: String::new(),
            country: String::new(),
            kind,
            road_closed: false,
            lanes_restricted: false,
            speed_limit: None,
            severity: None,
            road: None,
            valid_from: None,
            valid_until: None,
            description: None,
            lang: None,
            shape,
            both_directions: false,
            map_only: false,
            osm_way_ids: Vec::new(),
            edges: Vec::new(),
            match_status: MatchStatus::Pending,
        }
    }

    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        self.valid_until.is_some_and(|t| t < now)
    }
}
