use anyhow::{Context, Result};
use chrono::DateTime;
use roxmltree::{Document, Node};

use crate::model::{Incident, Kind, Shape};

/// Roadworks and closures from the Servei Català de Trànsit, a GML feature collection of WGS84 points.
pub fn parse(xml: &str) -> Result<Vec<Incident>> {
    let doc = Document::parse(xml.trim_start_matches('\u{feff}')).context("invalid XML")?;
    Ok(doc
        .descendants()
        .filter(|n| n.tag_name().name() == "featureMember")
        .filter_map(feature)
        .collect())
}

fn feature(member: Node) -> Option<Incident> {
    let state = text(member, "descripcio").unwrap_or_default();
    let works = text(member, "tipus") == Some("3");
    if !works && !state.starts_with("Calçada") {
        return None;
    }
    let (lon, lat) = text(member, "coordinates")?.split_once(',')?;
    let point = [lon.trim().parse().ok()?, lat.trim().parse().ok()?];
    let kind = if works { Kind::Roadworks } else { Kind::Restriction };
    let mut inc = Incident::new(
        text(member, "identificador")?.to_string(),
        kind,
        Shape::Points(vec![point]),
    );
    inc.road_closed = state.starts_with("Calçada tallada");
    inc.lanes_restricted = state.starts_with("Calçada restringida");
    inc.road = text(member, "carretera").map(str::to_string);
    inc.description = Some(
        [Some(state), text(member, "causa")]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(": "),
    )
    .filter(|d| !d.is_empty());
    inc.lang = Some("ca".into());
    inc.valid_from = text(member, "data")
        .and_then(|d| DateTime::parse_from_rfc2822(d).ok())
        .map(|t| t.to_utc());
    Some(inc)
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

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs" xmlns:gml="http://www.opengis.net/gml" xmlns:cite="http://www.opengeospatial.net/cite">
<gml:featureMember><cite:mct2_v_afectacions_data><cite:geom><gml:Point><gml:coordinates>2.13934623,41.51036627</gml:coordinates></gml:Point></cite:geom>
<cite:identificador>152608008</cite:identificador><cite:tipus>2</cite:tipus><cite:carretera>AP-7</cite:carretera><cite:causa>Circulació</cite:causa>
<cite:data>Mon, 16 Nov 2026 13:41:23 GMT</cite:data><cite:descripcio>Circulació amb congestió</cite:descripcio></cite:mct2_v_afectacions_data></gml:featureMember>
<gml:featureMember><cite:mct2_v_afectacions_data><cite:geom><gml:Point><gml:coordinates>1.86,41.47</gml:coordinates></gml:Point></cite:geom>
<cite:identificador>152600001</cite:identificador><cite:tipus>3</cite:tipus><cite:carretera>C-243c</cite:carretera><cite:causa>Obres en general</cite:causa>
<cite:data>Mon, 16 Nov 2026 07:00:00 GMT</cite:data><cite:descripcio>Calçada tallada</cite:descripcio></cite:mct2_v_afectacions_data></gml:featureMember>
</wfs:FeatureCollection>"#;

    #[test]
    fn keeps_roadworks_and_closures_but_not_congestion() {
        let incidents = parse(XML).unwrap();
        assert_eq!(incidents.len(), 1);
        let inc = &incidents[0];
        assert_eq!(inc.id, "152600001");
        assert_eq!(inc.kind, Kind::Roadworks);
        assert!(inc.road_closed);
        assert_eq!(inc.road.as_deref(), Some("C-243c"));
        assert_eq!(inc.description.as_deref(), Some("Calçada tallada: Obres en general"));
        assert_eq!(inc.shape, Shape::Points(vec![[1.86, 41.47]]));
        assert_eq!(inc.valid_from.unwrap().to_rfc3339(), "2026-11-16T07:00:00+00:00");
    }
}
