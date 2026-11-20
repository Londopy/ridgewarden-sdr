//! One location fix from whatever the sites measured: signal strength, KrakenSDR
//! bearings and time differences of arrival, weighted by how good each kind is.
//!
//! Unknowns are the transmitter's position and, when signal strengths are used, its level
//! at 1 km (they are only relative). Gauss-Newton steps are taken in a local east/north
//! frame at the current estimate, starting from the best point of a coarse grid over the
//! area, so a bad start can't trap the fit. The 95% error ellipse comes from the fit's
//! covariance (J^T W J)^-1: it reflects geometry and each measurement's stated error, and
//! is widened when redundant measurements disagree by more than those errors allow.

use crate::geo::{LatLon, azimuth, distance_km, inside_ring, local, offset};

/// sqrt(chi-square(2 dof, 0.95)): 1-sigma ellipse to 95%.
pub const K95: f64 = 2.447_746_830_680_816;
/// Closer than this to a site, distances and bearings stop being smooth.
const NEAR_KM: f64 = 0.05;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Obs {
    /// d(transmitter, b) - d(transmitter, a), km, from a time difference times c
    Tdoa {
        a: LatLon,
        b: LatLon,
        dd_km: f64,
        sigma_km: f64,
    },
    /// true bearing from `site` toward the transmitter
    Bearing {
        site: LatLon,
        az_deg: f64,
        sigma_deg: f64,
    },
    /// a level in dB on any scale shared by every Level in the fit
    Level {
        site: LatLon,
        db: f64,
        sigma_db: f64,
    },
}

impl Obs {
    pub fn sites(&self) -> Vec<LatLon> {
        match self {
            Obs::Tdoa { a, b, .. } => vec![*a, *b],
            Obs::Bearing { site, .. } | Obs::Level { site, .. } => vec![*site],
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Obs::Tdoa { .. } => "time difference",
            Obs::Bearing { .. } => "bearing",
            Obs::Level { .. } => "signal strength",
        }
    }
}

/// 95% error ellipse: semi-axes in km, and the major axis' bearing from north.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ellipse {
    pub major_km: f64,
    pub minor_km: f64,
    pub az_deg: f64,
}

impl Ellipse {
    /// Whether `p` lies inside the ellipse centred on `center`.
    pub fn contains(&self, center: LatLon, p: LatLon) -> bool {
        let (e, n) = local(center, p);
        let t = self.az_deg.to_radians();
        let along = e * t.sin() + n * t.cos();
        let across = e * t.cos() - n * t.sin();
        (along / self.major_km).powi(2) + (across / self.minor_km.max(1e-6)).powi(2) <= 1.0
    }

    /// Points around it, for drawing.
    pub fn outline(&self, center: LatLon) -> Vec<LatLon> {
        let t = self.az_deg.to_radians();
        (0..=72)
            .map(|i| {
                let a = i as f64 / 72.0 * std::f64::consts::TAU;
                let (x, y) = (self.major_km * a.cos(), self.minor_km * a.sin());
                offset(center, x * t.sin() + y * t.cos(), x * t.cos() - y * t.sin())
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct Fix {
    pub pos: LatLon,
    pub ellipse: Ellipse,
    /// sum of squared residuals over their errors
    pub chi2: f64,
    /// measurements beyond the minimum needed; 0 means nothing cross-checks the fit
    pub dof: i32,
    /// whether the fix is inside the ring of the sites that measured it
    pub inside: bool,
    /// another, distant position fits about as well
    pub ambiguous: bool,
    /// how many measurements of each kind went in
    pub used: Vec<(&'static str, usize)>,
}

struct Model<'a> {
    obs: &'a [Obs],
    path_exp: f64,
    levels: bool,
}

fn wrap180(d: f64) -> f64 {
    (d + 180.0).rem_euclid(360.0) - 180.0
}

impl Model<'_> {
    fn unknowns(&self) -> usize {
        if self.levels { 3 } else { 2 }
    }

    /// Residual over sigma and its derivatives (east km, north km, level) for one measurement.
    fn row(&self, o: &Obs, p: LatLon, level: f64) -> (f64, [f64; 3]) {
        // moving p east/north changes d(p, s) by -sin/-cos of the bearing from p to s
        let dd = |s: LatLon| {
            let az = azimuth(p, s).to_radians();
            (distance_km(p, s).max(NEAR_KM), [-az.sin(), -az.cos()])
        };
        match *o {
            Obs::Tdoa {
                a,
                b,
                dd_km,
                sigma_km,
            } => {
                let ((da, ga), (db, gb)) = (dd(a), dd(b));
                let r = (dd_km - (db - da)) / sigma_km;
                (
                    r,
                    [(gb[0] - ga[0]) / sigma_km, (gb[1] - ga[1]) / sigma_km, 0.0],
                )
            }
            Obs::Bearing {
                site,
                az_deg,
                sigma_deg,
            } => {
                let d = distance_km(site, p).max(NEAR_KM);
                let az = azimuth(site, p);
                let s = sigma_deg.to_radians();
                let r = wrap180(az_deg - az).to_radians() / s;
                let t = az.to_radians();
                (r, [t.cos() / d / s, -t.sin() / d / s, 0.0])
            }
            Obs::Level { site, db, sigma_db } => {
                let (d, g) = dd(site);
                let k = -10.0 * self.path_exp / (d * std::f64::consts::LN_10);
                let pred = level - 10.0 * self.path_exp * d.log10();
                (
                    (db - pred) / sigma_db,
                    [k * g[0] / sigma_db, k * g[1] / sigma_db, 1.0 / sigma_db],
                )
            }
        }
    }

    /// The level at 1 km that best fits the levels from `p`, which has a closed form.
    fn best_level(&self, p: LatLon) -> f64 {
        let (mut num, mut den) = (0.0, 0.0);
        for o in self.obs {
            if let Obs::Level { site, db, sigma_db } = *o {
                let d = distance_km(site, p).max(NEAR_KM);
                let w = 1.0 / sigma_db.powi(2);
                num += w * (db + 10.0 * self.path_exp * d.log10());
                den += w;
            }
        }
        if den > 0.0 { num / den } else { 0.0 }
    }

    fn cost(&self, p: LatLon, level: f64) -> f64 {
        self.obs
            .iter()
            .map(|o| self.row(o, p, level).0.powi(2))
            .sum()
    }

    /// Normal matrix and right-hand side at `p`.
    fn normal(&self, p: LatLon, level: f64) -> ([[f64; 3]; 3], [f64; 3]) {
        let mut a = [[0.0; 3]; 3];
        let mut b = [0.0; 3];
        for o in self.obs {
            let (r, j) = self.row(o, p, level);
            for i in 0..3 {
                b[i] += j[i] * r;
                for k in 0..3 {
                    a[i][k] += j[i] * j[k];
                }
            }
        }
        (a, b)
    }

    fn refine(&self, mut p: LatLon) -> Option<(LatLon, f64, f64)> {
        let mut level = self.best_level(p);
        let mut cost = self.cost(p, level);
        let n = self.unknowns();
        for _ in 0..50 {
            let (a, b) = self.normal(p, level);
            let step = solve(&a, &b, n)?;
            let mut scale = 1.0;
            let mut moved = false;
            for _ in 0..12 {
                let q = offset(p, step[0] * scale, step[1] * scale);
                let ql = level + step[2] * scale;
                let c = self.cost(q, ql);
                if c <= cost {
                    let small = step[0].hypot(step[1]) * scale < 1e-4;
                    p = q;
                    level = ql;
                    cost = c;
                    moved = !small;
                    break;
                }
                scale /= 2.0;
            }
            if !moved {
                break;
            }
        }
        Some((p, level, cost))
    }
}

/// Solve A x = b for the first `n` unknowns by Gaussian elimination; None if singular.
fn solve(a: &[[f64; 3]; 3], b: &[f64; 3], n: usize) -> Option<[f64; 3]> {
    let mut m = [[0.0; 4]; 3];
    for i in 0..n {
        m[i][..n].copy_from_slice(&a[i][..n]);
        m[i][3] = b[i];
    }
    let scale = (0..n).map(|i| a[i][i].abs()).fold(0.0, f64::max);
    for c in 0..n {
        let piv = (c..n).max_by(|x, y| m[*x][c].abs().total_cmp(&m[*y][c].abs()))?;
        if m[piv][c].abs() <= 1e-12 * scale.max(1e-300) {
            return None;
        }
        m.swap(c, piv);
        for r in 0..n {
            if r != c {
                let f = m[r][c] / m[c][c];
                let pivot = m[c];
                for (x, p) in m[r].iter_mut().zip(pivot).skip(c) {
                    *x -= f * p;
                }
            }
        }
    }
    let mut x = [0.0; 3];
    for i in 0..n {
        x[i] = m[i][3] / m[i][i];
    }
    Some(x)
}

/// The position block of the inverse of the normal matrix.
fn position_cov(a: &[[f64; 3]; 3], n: usize) -> Option<[[f64; 2]; 2]> {
    let c0 = solve(a, &[1.0, 0.0, 0.0], n)?;
    let c1 = solve(a, &[0.0, 1.0, 0.0], n)?;
    Some([[c0[0], c1[0]], [c0[1], c1[1]]])
}

fn ellipse(cov: [[f64; 2]; 2]) -> Ellipse {
    // covariance in (east, north)
    let (ee, nn, en) = (cov[0][0], cov[1][1], cov[0][1]);
    let mid = (ee + nn) / 2.0;
    let rad = (((ee - nn) / 2.0).powi(2) + en * en).sqrt();
    let (l1, l2) = (mid + rad, (mid - rad).max(0.0));
    // major axis direction: eigenvector of l1
    let az = if en.abs() < 1e-15 && ee >= nn {
        90.0
    } else if en.abs() < 1e-15 {
        0.0
    } else {
        (l1 - nn).atan2(en).to_degrees()
    };
    Ellipse {
        major_km: K95 * l1.sqrt(),
        minor_km: K95 * l2.sqrt(),
        az_deg: az.rem_euclid(180.0),
    }
}

/// The best fix for `obs`, or why there isn't one. `path_exp` is the path-loss exponent
/// for signal strengths (2 in free space, 3 to 4 over rough ground).
pub fn locate(obs: &[Obs], path_exp: f64) -> Result<Fix, String> {
    let levels = obs
        .iter()
        .filter(|o| matches!(o, Obs::Level { .. }))
        .count();
    // one level only sets the unknown transmitter level, so it can't help
    let obs: Vec<Obs> = obs
        .iter()
        .copied()
        .filter(|o| levels >= 2 || !matches!(o, Obs::Level { .. }))
        .collect();
    let m = Model {
        obs: &obs,
        path_exp,
        levels: levels >= 2,
    };
    let need = m.unknowns();
    if obs.len() < need {
        return Err(format!(
            "{} usable measurements, and a fix needs at least {need}: two bearings, three sites' \
             time differences or signal strengths, or a mix",
            obs.len()
        ));
    }
    let sites: Vec<LatLon> = obs.iter().flat_map(Obs::sites).collect();
    let center = LatLon::new(
        sites.iter().map(|s| s.lat).sum::<f64>() / sites.len() as f64,
        sites.iter().map(|s| s.lon).sum::<f64>() / sites.len() as f64,
    );
    let reach = sites
        .iter()
        .map(|s| distance_km(center, *s))
        .fold(0.0, f64::max)
        .max(10.0)
        * 2.0;
    let mut grid: Vec<(f64, LatLon)> = vec![];
    let steps = 60;
    for i in 0..=steps {
        for k in 0..=steps {
            let e = -reach + 2.0 * reach * i as f64 / steps as f64;
            let n = -reach + 2.0 * reach * k as f64 / steps as f64;
            let p = offset(center, e, n);
            grid.push((m.cost(p, m.best_level(p)), p));
        }
    }
    grid.sort_by(|a, b| a.0.total_cmp(&b.0));
    // start from the best few grid points that are well apart
    let mut starts: Vec<LatLon> = vec![];
    for (_, p) in &grid {
        if starts.iter().all(|s| distance_km(*s, *p) > reach / 6.0) {
            starts.push(*p);
        }
        if starts.len() == 4 {
            break;
        }
    }
    let mut fits: Vec<(LatLon, f64, f64)> = starts.iter().filter_map(|s| m.refine(*s)).collect();
    fits.sort_by(|a, b| a.2.total_cmp(&b.2));
    let Some(&(pos, level, chi2)) = fits.first() else {
        return Err(
            "the measurements don't pin down a position (they may all be from one place)".into(),
        );
    };
    let (a, _) = m.normal(pos, level);
    let Some(mut cov) = position_cov(&a, need) else {
        return Err("the measurements don't pin down a position: the geometry is degenerate, e.g. parallel bearings".into());
    };
    let dof = obs.len() as i32 - need as i32;
    // redundant measurements that disagree more than their errors say widen the ellipse
    if dof > 0 && chi2 / dof as f64 > 1.0 {
        let k = chi2 / dof as f64;
        cov.iter_mut().flatten().for_each(|x| *x *= k);
    }
    let ellipse = ellipse(cov);
    if !ellipse.major_km.is_finite() || ellipse.major_km > 1000.0 {
        return Err("the measurements don't pin down a position: the geometry is degenerate, e.g. parallel bearings".into());
    }
    let ambiguous = fits[1..]
        .iter()
        .any(|f| distance_km(f.0, pos) > 2.0 * ellipse.major_km.max(1.0) && f.2 <= chi2 + 2.0);
    let mut used: Vec<(&'static str, usize)> = vec![];
    for o in &obs {
        match used.iter_mut().find(|u| u.0 == o.kind()) {
            Some(u) => u.1 += 1,
            None => used.push((o.kind(), 1)),
        }
    }
    Ok(Fix {
        pos,
        ellipse,
        chi2,
        dof,
        inside: inside_ring(&sites, pos),
        ambiguous,
        used,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::C_KM_S;

    fn c() -> LatLon {
        LatLon::new(35.35, -120.6)
    }

    fn sites() -> Vec<LatLon> {
        vec![
            offset(c(), -15.0, -10.0),
            offset(c(), 15.0, -12.0),
            offset(c(), 2.0, 16.0),
            offset(c(), -12.0, 9.0),
        ]
    }

    fn tdoa(tx: LatLon, s: &[LatLon], sigma_km: f64, err_km: &[f64]) -> Vec<Obs> {
        (1..s.len())
            .map(|i| Obs::Tdoa {
                a: s[0],
                b: s[i],
                dd_km: distance_km(tx, s[i]) - distance_km(tx, s[0])
                    + err_km.get(i).copied().unwrap_or(0.0),
                sigma_km,
            })
            .collect()
    }

    #[test]
    fn time_differences_find_the_transmitter() {
        let tx = offset(c(), 3.0, -2.0);
        let f = locate(&tdoa(tx, &sites(), 0.5, &[]), 3.0).unwrap();
        assert!(distance_km(f.pos, tx) < 0.01, "{:?}", f.pos);
        assert!(f.inside);
        assert_eq!(f.dof, 1);
        assert!(
            f.ellipse.major_km > 0.3 && f.ellipse.major_km < 5.0,
            "{:?}",
            f.ellipse
        );
        assert!(f.ellipse.contains(f.pos, tx));
        // 1 microsecond is 0.3 km of path difference
        assert!((C_KM_S * 1e-6 - 0.3).abs() < 0.001);
    }

    #[test]
    fn outside_the_ring_the_ellipse_stretches() {
        let inside = locate(&tdoa(c(), &sites(), 0.5, &[]), 3.0).unwrap();
        let far = offset(c(), 60.0, 5.0);
        let outside = locate(&tdoa(far, &sites(), 0.5, &[]), 3.0).unwrap();
        assert!(!outside.inside);
        assert!(
            outside.ellipse.major_km > 3.0 * inside.ellipse.major_km,
            "{:?} vs {:?}",
            outside.ellipse,
            inside.ellipse
        );
    }

    #[test]
    fn two_bearings_cross_at_the_transmitter() {
        let s = sites();
        let tx = offset(c(), 4.0, 1.0);
        let obs: Vec<Obs> = s[..2]
            .iter()
            .map(|site| Obs::Bearing {
                site: *site,
                az_deg: azimuth(*site, tx),
                sigma_deg: 5.0,
            })
            .collect();
        let f = locate(&obs, 3.0).unwrap();
        assert!(distance_km(f.pos, tx) < 0.05);
        assert_eq!(f.dof, 0);
        assert!(f.ellipse.major_km > f.ellipse.minor_km);
        let parallel = [
            Obs::Bearing {
                site: s[0],
                az_deg: 90.0,
                sigma_deg: 5.0,
            },
            Obs::Bearing {
                site: offset(s[0], 0.0, 0.001),
                az_deg: 90.0,
                sigma_deg: 5.0,
            },
        ];
        assert!(locate(&parallel, 3.0).is_err());
        assert!(locate(&obs[..1], 3.0).unwrap_err().contains("at least 2"));
    }

    #[test]
    fn signal_strengths_alone_give_a_rough_fix_with_a_big_ellipse() {
        let tx = offset(c(), -2.0, 3.0);
        let obs: Vec<Obs> = sites()
            .iter()
            .map(|s| Obs::Level {
                site: *s,
                db: 40.0 - 30.0 * distance_km(tx, *s).log10(),
                sigma_db: 6.0,
            })
            .collect();
        let f = locate(&obs, 3.0).unwrap();
        assert!(distance_km(f.pos, tx) < 0.5, "{:?}", f.pos);
        let t = locate(&tdoa(tx, &sites(), 0.5, &[]), 3.0).unwrap();
        assert!(
            f.ellipse.major_km > t.ellipse.major_km,
            "strength is the weakest method"
        );
    }

    #[test]
    fn disagreeing_measurements_widen_the_ellipse() {
        let tx = offset(c(), 1.0, 1.0);
        let clean = locate(&tdoa(tx, &sites(), 0.3, &[]), 3.0).unwrap();
        let noisy = locate(&tdoa(tx, &sites(), 0.3, &[0.0, 2.0, -2.0, 2.0]), 3.0).unwrap();
        assert!(noisy.chi2 > 10.0);
        assert!(noisy.ellipse.major_km > clean.ellipse.major_km);
    }

    #[test]
    fn mixed_bearing_and_time_difference() {
        let s = sites();
        let tx = offset(c(), -4.0, -3.0);
        let mut obs = tdoa(tx, &s[..2], 0.5, &[]);
        obs.push(Obs::Bearing {
            site: s[2],
            az_deg: azimuth(s[2], tx),
            sigma_deg: 4.0,
        });
        let f = locate(&obs, 3.0).unwrap();
        assert!(distance_km(f.pos, tx) < 0.05, "{:?}", f.pos);
        assert_eq!(f.used.len(), 2);
    }

    #[test]
    fn ellipse_axes_point_the_right_way() {
        let e = ellipse([[4.0, 0.0], [0.0, 1.0]]);
        assert!((e.az_deg - 90.0).abs() < 1e-9, "wider east-west");
        assert!((e.major_km / e.minor_km - 2.0).abs() < 1e-9);
        let n = ellipse([[1.0, 0.0], [0.0, 4.0]]);
        assert!(n.az_deg.abs() < 1e-9);
        let d = ellipse([[2.5, 1.5], [1.5, 2.5]]);
        assert!((d.az_deg - 45.0).abs() < 1e-6, "{d:?}");
        let center = c();
        assert!(d.contains(center, offset(center, 1.0, 1.0)));
        assert!(!d.contains(center, offset(center, 2.5, -2.5)));
    }
}
