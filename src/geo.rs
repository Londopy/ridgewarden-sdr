//! Positions on the earth: distances, bearings, and whether a point is inside the ring of
//! receivers. Spherical, which is within a few metres per kilometre of the ellipsoid.

use serde::{Deserialize, Serialize};

pub const R_KM: f64 = 6371.0088;
/// Speed of light, km/s.
pub const C_KM_S: f64 = 299_792.458;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

impl LatLon {
    pub fn new(lat: f64, lon: f64) -> Self {
        Self { lat, lon }
    }
}

pub fn distance_km(a: LatLon, b: LatLon) -> f64 {
    let (p1, p2) = (a.lat.to_radians(), b.lat.to_radians());
    let dp = p2 - p1;
    let dl = (b.lon - a.lon).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * R_KM * h.sqrt().min(1.0).asin()
}

/// Initial bearing from `a` to `b`, degrees clockwise from true north, 0 to 360.
pub fn azimuth(a: LatLon, b: LatLon) -> f64 {
    let (p1, p2) = (a.lat.to_radians(), b.lat.to_radians());
    let dl = (b.lon - a.lon).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// The point `km` from `a` along bearing `az_deg`.
pub fn destination(a: LatLon, az_deg: f64, km: f64) -> LatLon {
    let d = km / R_KM;
    let (p1, l1, t) = (a.lat.to_radians(), a.lon.to_radians(), az_deg.to_radians());
    let p2 = (p1.sin() * d.cos() + p1.cos() * d.sin() * t.cos()).asin();
    let l2 = l1 + (t.sin() * d.sin() * p1.cos()).atan2(d.cos() - p1.sin() * p2.sin());
    LatLon::new(
        p2.to_degrees(),
        (l2.to_degrees() + 540.0).rem_euclid(360.0) - 180.0,
    )
}

/// Move `a` by `east` and `north` km.
pub fn offset(a: LatLon, east: f64, north: f64) -> LatLon {
    let km = east.hypot(north);
    if km == 0.0 {
        return a;
    }
    destination(a, east.atan2(north).to_degrees(), km)
}

/// `b` as (east, north) km from `a`, on a plane tangent at `a`. Good to well under a
/// percent across a county.
pub fn local(a: LatLon, b: LatLon) -> (f64, f64) {
    let d = distance_km(a, b);
    let az = azimuth(a, b).to_radians();
    (d * az.sin(), d * az.cos())
}

fn cross(o: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

fn half_hull(pts: impl Iterator<Item = (f64, f64)>) -> Vec<(f64, f64)> {
    let mut h: Vec<(f64, f64)> = vec![];
    for q in pts {
        while h.len() >= 2 && cross(h[h.len() - 2], h[h.len() - 1], q) <= 0.0 {
            h.pop();
        }
        h.push(q);
    }
    h.pop();
    h
}

/// Whether `p` lies inside the convex hull of `sites`: inside the ring of receivers,
/// where location fixes are much better than outside it.
pub fn inside_ring(sites: &[LatLon], p: LatLon) -> bool {
    let mut pts: Vec<(f64, f64)> = sites.iter().map(|s| local(p, *s)).collect();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    pts.dedup();
    if pts.len() < 3 {
        return false;
    }
    // Andrew's monotone chain, counter-clockwise
    let mut hull = half_hull(pts.iter().copied());
    hull.extend(half_hull(pts.iter().rev().copied()));
    if hull.len() < 3 {
        return false;
    }
    (0..hull.len()).all(|i| cross(hull[i], hull[(i + 1) % hull.len()], (0.0, 0.0)) > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_and_bearings() {
        let a = LatLon::new(35.0, -120.0);
        let north = destination(a, 0.0, 10.0);
        assert!((distance_km(a, north) - 10.0).abs() < 1e-9);
        assert!((north.lat - 35.0899).abs() < 1e-3);
        let east = offset(a, 7.0, 0.0);
        assert!((azimuth(a, east) - 90.0).abs() < 0.01);
        assert!((distance_km(a, east) - 7.0).abs() < 1e-6);
        let (e, n) = local(a, offset(a, 3.0, -4.0));
        assert!((e - 3.0).abs() < 1e-3 && (n + 4.0).abs() < 1e-3);
        // meridians converge, so the way back isn't quite due west
        assert!((azimuth(east, a) - 270.0).abs() < 0.1);
    }

    #[test]
    fn inside_the_ring_of_receivers() {
        let c = LatLon::new(35.3, -120.6);
        let ring = [
            offset(c, -10.0, -10.0),
            offset(c, 10.0, -10.0),
            offset(c, 0.0, 12.0),
        ];
        assert!(inside_ring(&ring, c));
        assert!(!inside_ring(&ring, offset(c, 30.0, 0.0)));
        assert!(!inside_ring(&ring[..2], c), "two sites have no inside");
        let mut more = ring.to_vec();
        more.push(offset(c, 1.0, 1.0));
        assert!(inside_ring(&more, offset(c, -1.0, -2.0)));
    }
}
