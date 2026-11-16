use std::io::{Cursor, Read};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeZone, Utc};
use serde_json::Value;
use zip::ZipArchive;

use crate::geo;
use crate::model::{Coord, Incident, Kind, Shape};

/// Closures from Saxony's road works information system, a zip of GeoJSON files in ETRS89 / UTM zone 33N.
pub fn parse(zip: &[u8]) -> Result<Vec<Incident>> {
    let mut archive = ZipArchive::new(Cursor::new(zip)).context("invalid zip")?;
    let name = archive
        .file_names()
        .filter_map(Result::ok)
        .find(|n| n.contains("Sperrungen") && n.ends_with(".geojson"))
        .context("no closures file in the zip")?
        .into_owned();
    let mut text = String::new();
    archive.by_name(&name)?.read_to_string(&mut text)?;
    let doc: Value = serde_json::from_str(&text).context("invalid GeoJSON")?;
    Ok(doc["features"]
        .as_array()
        .context("missing features")?
        .iter()
        .filter_map(feature)
        .collect())
}

fn feature(f: &Value) -> Option<Incident> {
    let p = &f["properties"];
    let art = p["Sperrung_Art_Klartext"].as_str().unwrap_or_default();
    if art.contains("Halte- oder Parkverbot") {
        return None;
    }
    let lorries_only = art.contains("LKW");
    let kind = if lorries_only {
        Kind::Restriction
    } else {
        Kind::Roadworks
    };
    let mut inc = Incident::new(p["ID"].as_str()?.to_string(), kind, shape(&f["geometry"])?);
    let one_direction = matches!(art, "Sperrung einer Fahrtrichtung" | "Sperrung einer Richtungsfahrbahn");
    inc.both_directions = !one_direction;
    inc.road_closed = art == "Vollsperrung";
    // The data does not say which way a one-direction closure runs, so it only slows the road down.
    inc.lanes_restricted = matches!(
        art,
        "Halbseitige Sperrung"
            | "Sperrung einer Fahrtrichtung"
            | "Sperrung einer Richtungsfahrbahn"
            | "Verkehrsbeschränkung"
    );
    inc.road = p["Strasse"].as_str().map(str::to_string);
    inc.description = [Some(art), p["Sperrung_Grund"].as_str()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(": ")
        .into();
    inc.lang = Some("de".into());
    inc.valid_from = p["Sperrung_von"].as_str().and_then(|d| day(d, NaiveTime::MIN));
    inc.valid_until = p["Sperrung_bis"]
        .as_str()
        .and_then(|d| day(d, NaiveTime::from_hms_opt(23, 59, 59)?));
    Some(inc)
}

fn shape(g: &Value) -> Option<Shape> {
    let line = |v: &Value| -> Option<Vec<Coord>> {
        v.as_array()?
            .iter()
            .map(|p| geo::from_utm33(p.get(0)?.as_f64()?, p.get(1)?.as_f64()?).ok())
            .collect()
    };
    let c = &g["coordinates"];
    let lines = match g["type"].as_str()? {
        "LineString" => vec![line(c)?],
        "MultiLineString" => c.as_array()?.iter().map(line).collect::<Option<_>>()?,
        _ => return None,
    };
    Some(Shape::Lines(lines))
}

// The dates carry no time zone. Central European Time is close enough for whole days.
fn day(date: &str, time: NaiveTime) -> Option<DateTime<Utc>> {
    let local = NaiveDate::parse_from_str(date, "%d.%m.%Y").ok()?.and_time(time);
    Some(
        FixedOffset::east_opt(3600)?
            .from_local_datetime(&local)
            .single()?
            .to_utc(),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;

    fn closure(art: &str) -> Value {
        json!({
            "type": "Feature",
            "geometry": {"type": "LineString", "coordinates": [[363386.0, 5687280.0], [363420.0, 5687300.0]]},
            "properties": {
                "ID": "627360VAO-028-2026",
                "Sperrung_Art_Klartext": art,
                "Strasse": "Industriestraße H",
                "Sperrung_von": "12.10.2026",
                "Sperrung_bis": "31.12.2026",
                "Sperrung_Grund": "Medienquerung für Neubau Umspannwerk"
            }
        })
    }

    #[test]
    fn full_closure_from_the_zip_in_wgs84() {
        let geojson = json!({"type": "FeatureCollection", "features": [closure("Vollsperrung"), closure("Zeitweiliges Halte- oder Parkverbot")]});
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file(
            "Baustelleninfo_Sperrungen_Sachsen.geojson",
            SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(geojson.to_string().as_bytes()).unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let incidents = parse(&bytes).unwrap();

        assert_eq!(incidents.len(), 1);
        let inc = &incidents[0];
        assert!(inc.road_closed && !inc.lanes_restricted && inc.both_directions);
        assert_eq!(inc.kind, Kind::Roadworks);
        assert_eq!(
            inc.description.as_deref(),
            Some("Vollsperrung: Medienquerung für Neubau Umspannwerk")
        );
        assert_eq!(inc.valid_from.unwrap().to_rfc3339(), "2026-10-11T23:00:00+00:00");
        assert_eq!(inc.valid_until.unwrap().to_rfc3339(), "2026-12-31T22:59:59+00:00");
        let Shape::Lines(lines) = &inc.shape else {
            panic!("expected lines")
        };
        let [lon, lat] = lines[0][0];
        assert!(
            (lon - 13.039472).abs() < 1e-6 && (lat - 51.320395).abs() < 1e-6,
            "{lon} {lat}"
        );
    }

    #[test]
    fn one_direction_and_lorry_bans_do_not_close_the_road() {
        let one_way = feature(&closure("Sperrung einer Fahrtrichtung")).unwrap();
        assert!(!one_way.road_closed && one_way.lanes_restricted && !one_way.both_directions);
        let lorries = feature(&closure("Sperrung für LKW")).unwrap();
        assert!(!lorries.road_closed && !lorries.lanes_restricted);
        assert_eq!(lorries.kind, Kind::Restriction);
    }
}
