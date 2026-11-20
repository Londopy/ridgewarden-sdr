//! Turning what the sites logged for one capture into a fix: signal strengths, bearings
//! and time differences, each with the error it honestly carries.

use anyhow::Result;
use rusqlite::Connection;

use crate::capture::{self, Cap, Scale, rank};
use crate::config::Config;
use crate::fix::{Fix, Obs, locate};
use crate::geo::{LatLon, distance_km};
use crate::kraken;
use crate::tdoa;

/// No KrakenSDR is better than this, whatever a station claims.
const MIN_BEARING_SIGMA_DEG: f64 = 1.0;
/// Nor an RTL-SDR time difference: a thirtieth of a snippet sample.
const MIN_TDOA_SIGMA_KM: f64 = 0.2;
/// A site this far from the middle of the others isn't part of the same hunt.
const FAR_KM: f64 = 300.0;

/// Signal strengths are the weakest evidence; without calibration weaker still.
fn level_sigma(cfg: &Config, scale: Scale) -> f64 {
    let base = cfg.fix.level_sigma_db;
    match scale {
        Scale::Dbm => base,
        Scale::AboveFloor => base + 2.0,
        Scale::Relative => base + 4.0,
    }
}

pub struct Evidence {
    pub obs: Vec<Obs>,
    /// snippets published for the capture
    pub snippets: usize,
}

fn median(mut v: Vec<f64>) -> f64 {
    ridgewarden_client::util::median(&mut v).unwrap_or(0.0)
}

/// Drop measurements from sites far from where the others are, so one stray position
/// can't stretch the search area or the ring of receivers.
fn near(obs: Vec<Obs>) -> Vec<Obs> {
    let sites: Vec<LatLon> = obs.iter().flat_map(Obs::sites).collect();
    let mid = LatLon::new(
        median(sites.iter().map(|s| s.lat).collect()),
        median(sites.iter().map(|s| s.lon).collect()),
    );
    obs.into_iter()
        .filter(|o| o.sites().iter().all(|s| distance_km(*s, mid) <= FAR_KM))
        .collect()
}

pub fn evidence(conn: &Connection, cfg: &Config, cap: &Cap) -> Result<Evidence> {
    let mut obs = vec![];
    let r = rank(capture::readings(conn, &cap.id)?);
    for (row, v) in &r.ranked {
        if let (Some(lat), Some(lon)) = (row.r.lat, row.r.lon) {
            obs.push(Obs::Level {
                site: LatLon::new(lat, lon),
                db: *v,
                sigma_db: level_sigma(cfg, r.scale),
            });
        }
    }
    for (_, b) in kraken::bearings(conn, &cap.id)? {
        if let (Some(lat), Some(lon), Some(az), Some(s)) =
            (b.lat, b.lon, b.bearing_deg, b.sigma_deg)
        {
            obs.push(Obs::Bearing {
                site: LatLon::new(lat, lon),
                az_deg: az,
                sigma_deg: s.max(MIN_BEARING_SIGMA_DEG),
            });
        }
    }
    if let Some((_, sol)) = tdoa::result(conn, &cap.id)? {
        for p in &sol.pairs {
            obs.push(Obs::Tdoa {
                a: p.a.pos(),
                b: p.b.pos(),
                dd_km: p.dd_km,
                sigma_km: p.sigma_km.max(MIN_TDOA_SIGMA_KM),
            });
        }
    }
    Ok(Evidence {
        obs: near(obs),
        snippets: tdoa::snippets(conn, &cap.id)?.len(),
    })
}

/// The fix the evidence gives, or why it gives none. Kept apart from reading the
/// evidence so the database isn't held while it is worked out.
pub fn solve(cfg: &Config, ev: &Evidence) -> Result<Fix, String> {
    locate(&ev.obs, cfg.fix.path_loss_exp)
}

/// The evidence for a capture and the fix it gives.
pub fn capture_fix(
    conn: &Connection,
    cfg: &Config,
    cap: &Cap,
) -> Result<(Evidence, Result<Fix, String>)> {
    let ev = evidence(conn, cfg, cap)?;
    let fix = solve(cfg, &ev);
    Ok((ev, fix))
}
