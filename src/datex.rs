use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use roxmltree::{Document, Node, ParsingOptions};

use crate::geo;
use crate::model::{Coord, Incident, Kind, Shape};

const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";

const CLOSED: &[&str] = &["roadClosed", "carriagewayClosures", "closedPermanentlyForTheWinter"];
const RESTRICTED: &[&str] = &[
    "laneClosures",
    "narrowLanes",
    "singleAlternateLineTraffic",
    "intermittentShortTermClosures",
    "contraflow",
    "lanesDeviated",
    "newRoadworksLayout",
];

pub struct Record {
    pub situation_id: Option<String>,
    pub incident: Incident,
}

/// Parses every situation record in a DATEX II v2 or v3 document, whatever its namespaces.
pub fn parse(xml: &str) -> Result<Vec<Record>> {
    let opts = ParsingOptions {
        allow_dtd: false,
        nodes_limit: u32::MAX,
        ..Default::default()
    };
    let doc = Document::parse_with_options(xml.trim_start_matches('\u{feff}'), opts).context("invalid XML")?;
    let records = doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "situationRecord")
        .filter_map(parse_record)
        .collect();
    Ok(records)
}

fn parse_record(rec: Node) -> Option<Record> {
    let validity = child(rec, "validity");
    if validity.and_then(|v| text(v, "validityStatus")) == Some("suspended") {
        return None;
    }
    let situation = rec.parent_element().filter(|p| p.tag_name().name() == "situation");
    let xtype = xsi_type(rec).unwrap_or_default();
    let mgmt = text(rec, "roadOrCarriagewayOrLaneManagementType");
    let location = child(rec, "locationReference").or_else(|| child(rec, "groupOfLocations"));

    let mut inc = Incident::new(
        rec.attribute("id")?.to_string(),
        kind(rec, xtype, mgmt),
        location.map(shape_of).unwrap_or(Shape::Points(Vec::new())),
    );
    inc.road_closed = mgmt.is_some_and(|m| CLOSED.contains(&m))
        || text(rec, "numberOfOperationalLanes") == Some("0")
        || text(rec, "drivingConditionType") == Some("impossible");
    inc.lanes_restricted = mgmt.is_some_and(|m| RESTRICTED.contains(&m))
        || text(rec, "numberOfLanesRestricted")
            .and_then(|v| v.parse::<u32>().ok())
            .is_some_and(|n| n > 0);
    inc.speed_limit = text(rec, "temporarySpeedLimit")
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v as u32);
    inc.severity = text(rec, "severity")
        .or_else(|| situation.and_then(|s| text(s, "overallSeverity")))
        .map(str::to_string);
    inc.road = text(rec, "roadName")
        .or_else(|| text(rec, "roadNumber"))
        .map(str::to_string);
    inc.valid_from = validity.and_then(|v| text(v, "overallStartTime")).and_then(parse_time);
    inc.valid_until = validity.and_then(|v| text(v, "overallEndTime")).and_then(parse_time);
    if let Some((desc, lang)) = description(rec) {
        inc.description = Some(desc);
        inc.lang = lang;
    }
    Some(Record {
        situation_id: situation.and_then(|s| s.attribute("id")).map(str::to_string),
        incident: inc,
    })
}

fn kind(rec: Node, xtype: &str, mgmt: Option<&str>) -> Kind {
    match xtype {
        "MaintenanceWorks" | "ConstructionWorks" => Kind::Roadworks,
        "Accident" => Kind::Accident,
        t if t.ends_with("Obstruction") => Kind::Obstruction,
        "WeatherRelatedRoadConditions" | "NonWeatherRelatedRoadConditions" | "PoorEnvironmentConditions" => {
            Kind::Conditions
        }
        "AbnormalTraffic" => Kind::Congestion,
        t if t.ends_with("Management") => {
            let works = matches!(text(rec, "causeType"), Some("roadMaintenance" | "constructionWork"))
                || text(rec, "roadMaintenanceType").is_some()
                || mgmt == Some("newRoadworksLayout");
            if works { Kind::Roadworks } else { Kind::Restriction }
        }
        _ => Kind::Other,
    }
}

fn shape_of(loc: Node) -> Shape {
    let lines: Vec<Vec<Coord>> = loc
        .descendants()
        .filter(|n| n.tag_name().name() == "gmlLineString")
        .filter_map(|n| {
            let lambert = n.attribute("srsName").is_some_and(|s| s.contains("31370"));
            pos_list(text(n, "posList")?, lambert)
        })
        .collect();
    if !lines.is_empty() {
        return Shape::Lines(lines);
    }

    let spans: Vec<[Coord; 2]> = loc
        .descendants()
        .filter(|n| n.tag_name().name() == "tpegLinearLocation")
        .filter_map(|n| Some([point(child(n, "from")?)?, point(child(n, "to")?)?]))
        .collect();
    if !spans.is_empty() {
        return Shape::Spans(spans);
    }

    let mut points: Vec<Coord> = Vec::new();
    for p in loc.descendants().filter(|n| n.tag_name().name() == "pointCoordinates") {
        if let Some(c) = coord(p).filter(|c| !points.contains(c)) {
            points.push(c);
        }
    }
    Shape::Points(points)
}

fn pos_list(s: &str, lambert: bool) -> Option<Vec<Coord>> {
    let nums: Vec<f64> = s.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
    if nums.len() < 4 || !nums.len().is_multiple_of(2) {
        return None;
    }
    nums.chunks(2)
        .map(|p| {
            if lambert {
                geo::from_lambert72(p[0], p[1]).ok()
            } else {
                Some([p[1], p[0]])
            }
        })
        .collect()
}

fn point(n: Node) -> Option<Coord> {
    coord(n.descendants().find(|d| d.tag_name().name() == "pointCoordinates")?)
}

fn coord(n: Node) -> Option<Coord> {
    let lat: f64 = text(n, "latitude")?.parse().ok()?;
    let lon: f64 = text(n, "longitude")?.parse().ok()?;
    // Flanders puts Lambert 72 metres into these fields without declaring a CRS.
    if lat.abs() > 90.0 || lon.abs() > 180.0 {
        return geo::from_lambert72(lon, lat).ok();
    }
    Some([lon, lat])
}

fn description(rec: Node) -> Option<(String, Option<String>)> {
    let comments: Vec<Node> = rec
        .children()
        .filter(|n| n.tag_name().name() == "generalPublicComment")
        .collect();
    let best = comments
        .iter()
        .find(|c| text(**c, "commentType") != Some("locationDescriptor"))
        .or(comments.first())?;
    let value = best
        .descendants()
        .find(|n| n.tag_name().name() == "value" && n.text().is_some_and(|t| !t.trim().is_empty()))?;
    Some((
        value.text()?.trim().to_string(),
        value.attribute("lang").map(str::to_string),
    ))
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc))
}

fn xsi_type<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    let t = n.attribute((XSI, "type"))?;
    Some(t.rsplit(':').next().unwrap_or(t))
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.tag_name().name() == name)
}

fn text<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.descendants()
        .find(|d| d.tag_name().name() == name)?
        .text()
        .map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(xml: &str) -> Incident {
        let mut records = parse(xml).unwrap();
        assert_eq!(records.len(), 1);
        records.remove(0).incident
    }

    #[test]
    fn v3_gml_line_with_zero_operational_lanes_is_closed() {
        let inc = one(include_str!("../tests/fixtures/lu-v3.xml"));
        assert_eq!(inc.id, "238510-1");
        assert_eq!(inc.kind, Kind::Roadworks);
        assert!(inc.road_closed);
        assert_eq!(inc.road.as_deref(), Some("A6"));
        let Shape::Lines(lines) = inc.shape else {
            panic!("expected lines")
        };
        assert_eq!(lines[0][0], [5.954695, 49.636867]);
    }

    #[test]
    fn v3_custom_namespaces_and_tpeg_span() {
        let inc = one(include_str!("../tests/fixtures/es-v3.xml"));
        assert_eq!(inc.kind, Kind::Roadworks);
        assert!(inc.road_closed);
        assert_eq!(inc.road.as_deref(), Some("A-8005"));
        assert_eq!(
            inc.shape,
            Shape::Spans(vec![[[-5.96623, 37.43413], [-5.9546967, 37.451286]]])
        );
    }

    #[test]
    fn v3_lambert72_line_is_reprojected() {
        let records = parse(include_str!("../tests/fixtures/be-lambert.xml")).unwrap();
        assert_eq!(records[0].situation_id.as_deref(), Some("EVT22443543"));
        let inc = &records[0].incident;
        assert!(inc.lanes_restricted && !inc.road_closed);
        let Shape::Lines(lines) = &inc.shape else {
            panic!("expected lines")
        };
        assert!((lines[0][0][0] - 4.107091).abs() < 1e-5 && (lines[0][0][1] - 51.032432).abs() < 1e-5);
    }

    #[test]
    fn lambert72_in_latitude_longitude_fields() {
        let xml = r#"<p xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><situationRecord id="r" xsi:type="RoadOrCarriagewayOrLaneManagement">
            <locationReference><pointByCoordinates><pointCoordinates>
            <latitude>191393.25</latitude><longitude>131644.95</longitude>
            </pointCoordinates></pointByCoordinates></locationReference></situationRecord></p>"#;
        let Shape::Points(points) = one(xml).shape else {
            panic!("expected points")
        };
        assert!((points[0][0] - 4.107091).abs() < 1e-5 && (points[0][1] - 51.032432).abs() < 1e-5);
    }

    #[test]
    fn v2_soap_envelope_span_and_point() {
        let records = parse(include_str!("../tests/fixtures/fr-v2.xml")).unwrap();
        assert_eq!(records.len(), 2);

        let works = &records[0].incident;
        assert!(works.lanes_restricted && !works.road_closed);
        assert_eq!(works.road.as_deref(), Some("N0024"));
        assert_eq!(works.severity.as_deref(), Some("medium"));
        assert_eq!(works.description.as_deref(), Some("La mesure est obligatoire"));
        assert_eq!(
            works.shape,
            Shape::Spans(vec![[[-1.7527946, 48.102207], [-1.7477167, 48.104973]]])
        );

        let pass = &records[1].incident;
        assert_eq!(pass.kind, Kind::Conditions);
        assert!(pass.road_closed);
        assert_eq!(pass.shape, Shape::Points(vec![[6.1781034, 45.23877]]));
        assert!(
            pass.description
                .as_deref()
                .unwrap()
                .starts_with("Le col du Glandon est fermé")
        );
    }
}
