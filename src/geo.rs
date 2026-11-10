use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use proj4rs::proj::Proj;

use crate::model::Coord;

const LAMBERT72: &str = "+proj=lcc +lat_0=90 +lon_0=4.36748666666667 +lat_1=51.1666672333333 \
    +lat_2=49.8333339 +x_0=150000.013 +y_0=5400088.438 +ellps=intl \
    +towgs84=-106.8686,52.2978,-103.7239,0.3366,-0.457,1.8422,-1.2747 +units=m +no_defs";
const WGS84: &str = "+proj=longlat +ellps=WGS84 +datum=WGS84 +no_defs";

/// Belgian Lambert 72 (EPSG:31370) to WGS84.
pub fn from_lambert72(x: f64, y: f64) -> Result<Coord> {
    static PROJS: OnceLock<(Proj, Proj)> = OnceLock::new();
    let (src, dst) = PROJS.get_or_init(|| {
        (
            Proj::from_proj_string(LAMBERT72).expect("valid Lambert 72 definition"),
            Proj::from_proj_string(WGS84).expect("valid WGS84 definition"),
        )
    });
    let mut p = (x, y, 0.0);
    proj4rs::transform::transform(src, dst, &mut p).map_err(|e| anyhow!("reprojection failed: {e:?}"))?;
    Ok([p.0.to_degrees(), p.1.to_degrees()])
}

/// Great-circle distance in metres.
pub fn distance(a: Coord, b: Coord) -> f64 {
    let (lat1, lat2) = (a[1].to_radians(), b[1].to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b[0] - a[0]).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

/// Decodes an encoded polyline. Valhalla uses a factor of 1e6.
pub fn decode_polyline(s: &str, factor: f64) -> Vec<Coord> {
    let mut out = Vec::new();
    let (mut lat, mut lon) = (0i64, 0i64);
    let mut bytes = s.bytes();
    let mut next = || -> Option<i64> {
        let (mut result, mut shift) = (0i64, 0);
        loop {
            let b = bytes.next()? as i64 - 63;
            result |= (b & 0x1f) << shift;
            shift += 5;
            if b < 0x20 {
                break;
            }
        }
        Some(if result & 1 != 0 { !(result >> 1) } else { result >> 1 })
    };
    while let (Some(dlat), Some(dlon)) = (next(), next()) {
        lat += dlat;
        lon += dlon;
        out.push([lon as f64 / factor, lat as f64 / factor]);
    }
    out
}

/// FNV-1a, stable across builds so it can key the on-disk match cache.
pub fn stable_hash(data: &[u8]) -> u64 {
    data.iter()
        .fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lambert72_matches_proj() {
        let [lon, lat] = from_lambert72(131644.95, 191393.25).unwrap();
        assert!((lon - 4.107091).abs() < 1e-5, "{lon}");
        assert!((lat - 51.032432).abs() < 1e-5, "{lat}");
    }

    #[test]
    fn polyline_known_value() {
        let pts = decode_polyline("_p~iF~ps|U_ulLnnqC_mqNvxq`@", 1e5);
        assert_eq!(pts, vec![[-120.2, 38.5], [-120.95, 40.7], [-126.453, 43.252]]);
    }
}
