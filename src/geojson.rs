use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::geo;
use crate::model::{Coord, Incident, Kind, Shape};

/// How the features of a plain GeoJSON source become incidents. Field names may be dotted paths into nested objects.
#[derive(Debug, Clone, Deserialize)]
pub struct GeoJsonConfig {
    /// More endpoints whose features join those of the source's `url`.
    #[serde(default)]
    pub extra_urls: Vec<String>,
    #[serde(default = "wgs84")]
    pub crs: u32,
    /// Where the list of records sits in a response, empty for a response that is the list.
    #[serde(default = "features")]
    pub features_path: String,
    /// Where a record keeps its fields, empty for fields on the record itself.
    #[serde(default = "properties")]
    pub properties_path: String,
    #[serde(default = "geometry")]
    pub geometry_path: String,
    /// A field holding a line as `lat lon lat lon …`.
    pub polyline_field: Option<String>,
    /// Joined to identify a feature. Without any, the feature's `id` is used.
    #[serde(default)]
    pub id_fields: Vec<String>,
    pub type_field: Option<String>,
    /// Field values to one of `roadworks`, `closure`, `restriction`, `accident`, `obstruction`, `conditions`,
    /// `congestion`, `other` or `skip`.
    #[serde(default)]
    pub type_map: BTreeMap<String, String>,
    #[serde(default = "roadworks")]
    pub default_type: String,
    pub severity_field: Option<String>,
    #[serde(default)]
    pub closed_values: Vec<String>,
    #[serde(default)]
    pub restricted_values: Vec<String>,
    /// Case-insensitive patterns matched against `severity_field`, for sources that describe severity in free text.
    pub closed_pattern: Option<String>,
    /// Keeps a `closed_pattern` match from counting, such as a closure of the footway only.
    pub closed_exception_pattern: Option<String>,
    pub restricted_pattern: Option<String>,
    pub headline_field: Option<String>,
    pub description_field: Option<String>,
    pub road_field: Option<String>,
    pub valid_from_field: Option<String>,
    pub valid_to_field: Option<String>,
    /// One field holding both dates, such as `01.03.2026 - 31.12.2026`.
    pub validity_range_field: Option<String>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Values that stand for an empty field.
    #[serde(default)]
    pub null_values: Vec<String>,
    pub lon_field: Option<String>,
    pub lat_field: Option<String>,
    pub start_lon_field: Option<String>,
    pub start_lat_field: Option<String>,
    pub end_lon_field: Option<String>,
    pub end_lat_field: Option<String>,
    /// False when the source's lines run in the affected direction only.
    #[serde(default = "yes")]
    pub both_directions: bool,
    #[serde(default)]
    pub lang: Option<String>,
}

/// Drops features by one field: when its value is excluded or matches `exclude_pattern`, or when `include` is set
/// and lacks it.
#[derive(Debug, Clone, Deserialize)]
pub struct Filter {
    pub field: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    pub exclude_pattern: Option<String>,
}

fn wgs84() -> u32 {
    4326
}

fn features() -> String {
    "features".into()
}

fn properties() -> String {
    "properties".into()
}

fn geometry() -> String {
    "geometry".into()
}

/// The items of a GeoRSS feed as records under `items`, each with its text fields and `lat` and `lon` from
/// `georss:point`, so a geojson table can describe them.
pub fn rss_items(xml: &str) -> Result<Value> {
    let doc = roxmltree::Document::parse(xml.trim_start_matches('\u{feff}')).context("invalid XML")?;
    let items = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "item")
        .map(|item| {
            let mut record = serde_json::Map::new();
            for child in item.children().filter(|c| c.is_element()) {
                let text = child.text().unwrap_or_default().trim();
                if child.tag_name().name() == "point" {
                    if let Some((lat, lon)) = text.split_once(' ') {
                        record.insert("lat".into(), Value::String(lat.into()));
                        record.insert("lon".into(), Value::String(lon.trim().into()));
                    }
                } else {
                    record.insert(child.tag_name().name().into(), Value::String(text.into()));
                }
            }
            Value::Object(record)
        })
        .collect();
    Ok(serde_json::json!({ "items": Value::Array(items) }))
}

fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(v);
    }
    path.split('.').try_fold(v, |cur, key| cur.get(key))
}

fn roadworks() -> String {
    "roadworks".into()
}

fn yes() -> bool {
    true
}

struct Patterns {
    closed: Option<Regex>,
    closed_exception: Option<Regex>,
    restricted: Option<Regex>,
    excluded: Vec<Option<Regex>>,
}

/// The incidents in GeoJSON feature collections described by `config`.
pub fn parse(bodies: &[Value], config: &GeoJsonConfig) -> Result<Vec<Incident>> {
    let pattern = |p: &Option<String>| -> Result<Option<Regex>> {
        p.as_deref()
            .map(|p| Regex::new(&format!("(?i){p}")).with_context(|| format!("invalid pattern {p}")))
            .transpose()
    };
    let patterns = Patterns {
        closed: pattern(&config.closed_pattern)?,
        closed_exception: pattern(&config.closed_exception_pattern)?,
        restricted: pattern(&config.restricted_pattern)?,
        excluded: config
            .filters
            .iter()
            .map(|f| pattern(&f.exclude_pattern))
            .collect::<Result<_>>()?,
    };
    let mut out = Vec::new();
    for body in bodies {
        let Some(features) = at(body, &config.features_path).and_then(Value::as_array) else {
            bail!("no list of records at {:?}", config.features_path)
        };
        out.extend(features.iter().filter_map(|f| feature(f, config, &patterns)));
    }
    Ok(out)
}

fn feature(f: &Value, config: &GeoJsonConfig, patterns: &Patterns) -> Option<Incident> {
    let props = at(f, &config.properties_path)?;
    let field = |name: &Option<String>| name.as_deref().and_then(|n| value(props, n, &config.null_values));
    for (filter, excluded) in config.filters.iter().zip(&patterns.excluded) {
        let v = value(props, &filter.field, &config.null_values);
        let dropped = v.as_ref().is_some_and(|v| filter.exclude.contains(v))
            || (!filter.include.is_empty() && !v.as_ref().is_some_and(|v| filter.include.contains(v)))
            || excluded
                .as_ref()
                .zip(v.as_deref())
                .is_some_and(|(re, v)| re.is_match(v));
        if dropped {
            return None;
        }
    }
    let kind_name = field(&config.type_field)
        .and_then(|t| config.type_map.get(&t).cloned())
        .unwrap_or_else(|| config.default_type.clone());
    let kind = match kind_name.as_str() {
        "skip" => return None,
        "roadworks" => Kind::Roadworks,
        "closure" | "restriction" => Kind::Restriction,
        "accident" => Kind::Accident,
        "obstruction" => Kind::Obstruction,
        "conditions" => Kind::Conditions,
        "congestion" => Kind::Congestion,
        _ => Kind::Other,
    };
    let mut inc = Incident::new(id(f, props, config)?, kind, shape(f, props, config)?);
    let severity = field(&config.severity_field);
    let matches = |re: &Option<Regex>| {
        re.as_ref()
            .zip(severity.as_deref())
            .is_some_and(|(re, s)| re.is_match(s))
    };
    inc.road_closed = kind_name == "closure"
        || severity.as_ref().is_some_and(|s| config.closed_values.contains(s))
        || (matches(&patterns.closed) && !matches(&patterns.closed_exception));
    inc.lanes_restricted = !inc.road_closed
        && (severity.as_ref().is_some_and(|s| config.restricted_values.contains(s)) || matches(&patterns.restricted));
    inc.both_directions = config.both_directions;
    inc.road = field(&config.road_field);
    inc.description = [field(&config.headline_field), field(&config.description_field)]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .into();
    inc.description = inc.description.filter(|d| !d.is_empty());
    inc.lang = config.lang.clone();
    let (from, until) = match field(&config.validity_range_field) {
        Some(range) => split_range(&range),
        None => (field(&config.valid_from_field), field(&config.valid_to_field)),
    };
    inc.valid_from = from.as_deref().and_then(|d| time(d, false));
    inc.valid_until = until.as_deref().and_then(|d| time(d, true));
    Some(inc)
}

fn id(f: &Value, props: &Value, config: &GeoJsonConfig) -> Option<String> {
    if config.id_fields.is_empty() {
        return value(f, "id", &[]).or_else(|| value(props, "id", &[]));
    }
    let parts: Vec<String> = config
        .id_fields
        .iter()
        .map(|n| value(props, n, &config.null_values).unwrap_or_default())
        .collect();
    let joined = parts.join("|");
    Some(if joined.len() > 64 {
        format!("{:016x}", geo::stable_hash(joined.as_bytes()))
    } else {
        joined
    })
    .filter(|s| !s.chars().all(|c| c == '|'))
}

fn shape(f: &Value, props: &Value, config: &GeoJsonConfig) -> Option<Shape> {
    let number = |name: &Option<String>| -> Option<f64> {
        let v = at(props, name.as_deref()?)?;
        v.as_f64().or_else(|| v.as_str()?.replace(',', ".").parse().ok())
    };
    let convert = |x: f64, y: f64| geo::from_epsg(config.crs, x, y).ok();
    if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
        number(&config.start_lon_field),
        number(&config.start_lat_field),
        number(&config.end_lon_field),
        number(&config.end_lat_field),
    ) {
        return Some(Shape::Spans(vec![[convert(x1, y1)?, convert(x2, y2)?]]));
    }
    if let (Some(x), Some(y)) = (number(&config.lon_field), number(&config.lat_field)) {
        return Some(Shape::Points(vec![convert(x, y)?]));
    }
    if let Some(text) = config.polyline_field.as_deref().and_then(|n| at(props, n)?.as_str()) {
        let nums: Vec<f64> = text.split_whitespace().filter_map(|n| n.parse().ok()).collect();
        let line: Vec<Coord> = nums
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|[lat, lon]| convert(*lon, *lat))
            .collect();
        return match line.len() {
            0 => None,
            1 => Some(Shape::Points(line)),
            _ => Some(Shape::Lines(vec![line])),
        };
    }
    let mut lines = Vec::new();
    let mut points = Vec::new();
    collect(
        at(f, &config.geometry_path).unwrap_or(&Value::Null),
        config.crs,
        &mut lines,
        &mut points,
    );
    if !lines.is_empty() {
        Some(Shape::Lines(lines))
    } else if !points.is_empty() {
        Some(Shape::Points(points))
    } else {
        None
    }
}

/// Lines stay lines. Areas count as their centre, since the road they block runs somewhere through them.
fn collect(g: &Value, crs: u32, lines: &mut Vec<Vec<Coord>>, points: &mut Vec<Coord>) {
    let c = &g["coordinates"];
    let line = |v: &Value| -> Option<Vec<Coord>> {
        v.as_array()?
            .iter()
            .map(|p| geo::from_epsg(crs, p.get(0)?.as_f64()?, p.get(1)?.as_f64()?).ok())
            .collect()
    };
    let centre = |ring: &Value| -> Option<Coord> {
        let pts = line(ring)?;
        (!pts.is_empty()).then(|| {
            let n = pts.len() as f64;
            [
                pts.iter().map(|p| p[0]).sum::<f64>() / n,
                pts.iter().map(|p| p[1]).sum::<f64>() / n,
            ]
        })
    };
    match g["type"].as_str().unwrap_or_default() {
        "Point" => points.extend(line(&Value::Array(vec![c.clone()])).into_iter().flatten()),
        "MultiPoint" => points.extend(line(c).into_iter().flatten()),
        "LineString" => lines.extend(line(c).filter(|l| l.len() >= 2)),
        "MultiLineString" => lines.extend(
            c.as_array()
                .into_iter()
                .flatten()
                .filter_map(line)
                .filter(|l| l.len() >= 2),
        ),
        "Polygon" => points.extend(c.get(0).and_then(centre)),
        "MultiPolygon" => points.extend(c.as_array().into_iter().flatten().filter_map(|p| centre(p.get(0)?))),
        "GeometryCollection" => {
            for part in g["geometries"].as_array().into_iter().flatten() {
                collect(part, crs, lines, points);
            }
        }
        _ => {}
    }
}

fn value(v: &Value, path: &str, nulls: &[String]) -> Option<String> {
    let mut cur = v;
    for key in path.split('.') {
        cur = cur.get(key)?;
    }
    let s = match cur {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Array(_) | Value::Object(_) => cur.to_string(),
        Value::Null => return None,
    };
    Some(s).filter(|s| !s.is_empty() && !nulls.contains(s))
}

fn split_range(range: &str) -> (Option<String>, Option<String>) {
    let range = range.trim().trim_start_matches("von ").trim();
    let (from, until) = [" bis ", " - ", " – ", "–"]
        .iter()
        .find_map(|sep| range.split_once(sep))
        .unwrap_or((range, ""));
    let clean = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    (clean(from), clean(until))
}

/// Dates as sources write them. Without a zone, Central European Time is assumed, close enough for the whole days
/// most sources give. A date without a time means the start of that day, or its end for an end date.
fn time(s: &str, end: bool) -> Option<DateTime<Utc>> {
    let s = s.trim().trim_end_matches("Uhr").trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.to_utc());
    }
    if let Ok(n) = s.parse::<i64>() {
        // Below 10^11 a count reads as seconds, since milliseconds that small would fall in early 1973.
        return if n.abs() < 100_000_000_000 {
            DateTime::from_timestamp(n, 0)
        } else {
            DateTime::from_timestamp_millis(n)
        };
    }
    let local = |t: NaiveDateTime| {
        FixedOffset::east_opt(3600)?
            .from_local_datetime(&t)
            .single()
            .map(|t| t.to_utc())
    };
    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%d.%m.%Y %H:%M:%S",
        "%d.%m.%Y %H:%M",
    ] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), format) {
            return local(t);
        }
    }
    let day_time = if end {
        NaiveTime::from_hms_opt(23, 59, 59)?
    } else {
        NaiveTime::MIN
    };
    for format in ["%Y-%m-%d", "%d.%m.%Y", "%d/%m/%Y", "%d.%m.%y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), format)
            .or_else(|_| NaiveDate::parse_from_str(s, format))
        {
            return local(d.and_time(day_time));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(extra: Value) -> GeoJsonConfig {
        let mut base = json!({
            "id_fields": ["id"],
            "type_field": "subtype",
            "type_map": {"Sperrung": "closure", "Parken": "skip"},
            "severity_field": "severity",
            "closed_values": ["Vollsperrung"],
            "restricted_values": ["Fahrtrichtungssperrung"],
            "headline_field": "content",
            "road_field": "street",
            "valid_from_field": "validity.from",
            "valid_to_field": "validity.to",
            "filters": [{"field": "area", "exclude": ["Gehweg"]}, {"field": "source", "include": ["Stadt"]},
                        {"field": "what", "exclude_pattern": "gehwegsp"}]
        });
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn collection(features: Value) -> Vec<Value> {
        vec![json!({"type": "FeatureCollection", "features": features})]
    }

    #[test]
    fn maps_types_severity_dates_and_filters() {
        let features = json!([
            {"type": "Feature", "geometry": {"type": "LineString", "coordinates": [[13.36, 52.53], [13.37, 52.54]]},
             "properties": {"id": "a", "source": "Stadt", "subtype": "Baustelle", "severity": "Vollsperrung", "content": "Kanalbau", "street": "Invalidenstraße",
                            "validity": {"from": "2026-11-01T06:00:00+01:00", "to": "30.11.2026"}}},
            {"type": "Feature", "geometry": {"type": "Polygon", "coordinates": [[[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]]]},
             "properties": {"id": "b", "source": "Stadt", "subtype": "Baustelle", "severity": "Fahrtrichtungssperrung"}},
            {"type": "Feature", "geometry": {"type": "Point", "coordinates": [1.0, 1.0]}, "properties": {"id": "c", "subtype": "Parken"}},
            {"type": "Feature", "geometry": {"type": "Point", "coordinates": [1.0, 1.0]}, "properties": {"id": "d", "area": "Gehweg"}},
            {"type": "Feature", "geometry": {"type": "Point", "coordinates": [1.0, 1.0]}, "properties": {"id": "e", "source": "Land"}},
            {"type": "Feature", "geometry": {"type": "Point", "coordinates": [1.0, 1.0]}, "properties": {"id": "f", "what": "Kanal // Gehwegsperrung"}}
        ]);

        let incidents = parse(&collection(features), &config(json!({}))).unwrap();

        assert_eq!(incidents.len(), 2);
        let closed = &incidents[0];
        assert!(closed.road_closed && closed.both_directions);
        assert_eq!(closed.kind, Kind::Roadworks);
        assert_eq!(closed.road.as_deref(), Some("Invalidenstraße"));
        assert_eq!(closed.description.as_deref(), Some("Kanalbau"));
        assert_eq!(closed.valid_from.unwrap().to_rfc3339(), "2026-11-01T05:00:00+00:00");
        assert_eq!(closed.valid_until.unwrap().to_rfc3339(), "2026-11-30T22:59:59+00:00");
        let area = &incidents[1];
        assert!(area.lanes_restricted && !area.road_closed);
        assert_eq!(area.shape, Shape::Points(vec![[1.0, 1.0]]));
    }

    #[test]
    fn reprojects_and_reads_free_text_severity_and_ranges() {
        let features = json!([
            {"type": "Feature", "geometry": {"type": "Point", "coordinates": [513712.674, 5403176.256]},
             "properties": {"id": "s", "source": "Stadt", "severity": "Kanalbau // Vollsperrung der Fahrbahn", "range": "01.03.2026 - 31.12.2026"}}
        ]);
        let cfg = config(
            json!({"crs": 25832, "closed_pattern": "vollsperr", "closed_exception_pattern": "vollsperrung\\s+fu",
                                "validity_range_field": "range"}),
        );

        let inc = parse(&collection(features), &cfg).unwrap().remove(0);

        assert!(inc.road_closed);
        assert_eq!(inc.valid_until.unwrap().to_rfc3339(), "2026-12-31T22:59:59+00:00");
        let Shape::Points(p) = inc.shape else {
            panic!("expected a point")
        };
        assert!(
            (p[0][0] - 9.186663).abs() < 1e-5 && (p[0][1] - 48.781435).abs() < 1e-5,
            "{p:?}"
        );
    }

    #[test]
    fn reads_the_periods_sources_write() {
        let period = |range: &str| {
            let (from, until) = split_range(range);
            (
                time(&from.unwrap(), false).unwrap().to_rfc3339(),
                time(&until.unwrap(), true).unwrap().to_rfc3339(),
            )
        };

        assert_eq!(
            period("2026-10-12 23:00:00 bis 2026-11-29 22:59:00"),
            ("2026-10-12T22:00:00+00:00".into(), "2026-11-29T21:59:00+00:00".into())
        );
        assert_eq!(
            period("von 16.11.2026 19:00 Uhr bis 17.11.2026 05:00 Uhr"),
            ("2026-11-16T18:00:00+00:00".into(), "2026-11-17T04:00:00+00:00".into())
        );
        assert_eq!(
            period("01.03.2026 - 21/03/2027"),
            ("2026-02-28T23:00:00+00:00".into(), "2027-03-21T22:59:59+00:00".into())
        );
    }

    #[test]
    fn records_outside_feature_collections_and_rss_items() {
        let lithuania = json!([{"_id": "x", "type": "ROADWORKS",
            "location": {"polyline": "56.24 24.85 56.25 24.86"}}]);
        let cfg: GeoJsonConfig = serde_json::from_value(json!({
            "features_path": "", "properties_path": "", "id_fields": ["_id"], "polyline_field": "location.polyline"
        }))
        .unwrap();
        let inc = parse(&[lithuania], &cfg).unwrap().remove(0);
        assert_eq!(inc.shape, Shape::Lines(vec![vec![[24.85, 56.24], [24.86, 56.25]]]));

        let rss = r#"<rss xmlns:georss="http://www.georss.org/georss"><channel><item><title>A40 closed</title>
            <guid>g1</guid><georss:point>51.7272 -4.067661</georss:point></item></channel></rss>"#;
        let cfg: GeoJsonConfig = serde_json::from_value(json!({
            "features_path": "items", "properties_path": "", "id_fields": ["guid"], "lat_field": "lat", "lon_field": "lon",
            "headline_field": "title"
        }))
        .unwrap();
        let inc = parse(&[rss_items(rss).unwrap()], &cfg).unwrap().remove(0);
        assert_eq!(inc.shape, Shape::Points(vec![[-4.067661, 51.7272]]));
        assert_eq!(inc.description.as_deref(), Some("A40 closed"));
    }

    #[test]
    fn start_and_end_fields_make_a_span() {
        let features = json!([{"type": "Feature", "geometry": null,
            "properties": {"id": "i", "source": "Stadt", "STARTX": 4.1, "STARTY": 51.0, "ENDX": 4.2, "ENDY": 51.1}}]);
        let cfg = config(
            json!({"start_lon_field": "STARTX", "start_lat_field": "STARTY", "end_lon_field": "ENDX", "end_lat_field": "ENDY"}),
        );

        let inc = parse(&collection(features), &cfg).unwrap().remove(0);

        assert_eq!(inc.shape, Shape::Spans(vec![[[4.1, 51.0], [4.2, 51.1]]]));
    }
}
