//! KrakenSDR bearings. krakensdr_doa, with its output format set to "Kraken App", writes
//! its latest result to `DOA_value.html` (served on port 8081): one line per VFO,
//! comma-separated, as its `kraken_sdr_signal_processor.py` builds it:
//!
//! epoch ms, 360 - theta (bearing relative to the array, clockwise), confidence (peak over
//! mean of the DOA spectrum, dB), max power (dB), frequency (Hz), array arrangement,
//! latency (ms), station id, latitude, longitude, heading, heading, "GPS", four reserved
//! fields, then the 360-point DOA spectrum.
//!
//! The true bearing is the relative one plus the heading, as krakensdr_doa's own map
//! output computes it. During a capture the page is read once a second and the bearings
//! for the requested frequency are averaged.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::util::{now_ms, valid_node};

use crate::app::{App, short_err};
use crate::capture::Cap;
use crate::config::{KrakenCfg, valid_name};
use crate::directory::{clean, station_node, valid_pos};
use crate::runner::sleep_until;

#[derive(Clone, Debug, PartialEq)]
pub struct Doa {
    pub ts_ms: i64,
    pub rel_deg: f64,
    pub conf: f64,
    pub freq: u64,
    pub lat: f64,
    pub lon: f64,
    pub heading: f64,
}

/// Every well-formed line of a `DOA_value.html`.
pub fn parse(text: &str) -> Vec<Doa> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').map(str::trim).collect();
            if f.len() < 17 {
                return None;
            }
            let num = |i: usize| f[i].parse::<f64>().ok().filter(|v| v.is_finite());
            Some(Doa {
                ts_ms: f[0].parse().ok()?,
                rel_deg: num(1)?.rem_euclid(360.0),
                conf: num(2)?,
                freq: num(4)? as u64,
                lat: num(8).unwrap_or(0.0),
                lon: num(9).unwrap_or(0.0),
                heading: num(10).unwrap_or(0.0),
            })
        })
        .collect()
}

impl Doa {
    pub fn bearing(&self, heading: Option<f64>) -> f64 {
        (self.rel_deg + heading.unwrap_or(self.heading)).rem_euclid(360.0)
    }

    /// The position the KrakenSDR reports, unless it is the 0, 0 it gives without GPS.
    pub fn pos(&self) -> Option<(f64, f64)> {
        let ok = (self.lat != 0.0 || self.lon != 0.0) && valid_pos(Some(self.lat), Some(self.lon));
        ok.then_some((self.lat, self.lon))
    }
}

/// Mean direction and circular spread of bearings, degrees.
pub fn circular_mean(deg: &[f64]) -> Option<(f64, f64)> {
    if deg.is_empty() {
        return None;
    }
    let (s, c) = deg.iter().fold((0.0, 0.0), |(s, c), d| {
        let r = d.to_radians();
        (s + r.sin(), c + r.cos())
    });
    let n = deg.len() as f64;
    let r = (s.hypot(c) / n).clamp(1e-12, 1.0);
    Some((
        s.atan2(c).to_degrees().rem_euclid(360.0),
        (-2.0 * r.ln()).sqrt().to_degrees(),
    ))
}

/// The `sdr.bearing` payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Bearing {
    pub capture: String,
    pub kraken: String,
    pub node: String,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    /// "ok", "refused" or "failed"
    pub status: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub bearing_deg: Option<f64>,
    #[serde(default)]
    pub sigma_deg: Option<f64>,
    #[serde(default)]
    pub samples: u32,
    #[serde(default)]
    pub conf_db: Option<f64>,
}

impl Bearing {
    pub fn from_payload(v: &Value) -> Option<Self> {
        let b: Bearing = serde_json::from_value(v.clone()).ok()?;
        let ok = b.capture.len() == 64
            && b.capture
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            && valid_name(&b.kraken)
            && valid_node(&b.node)
            && valid_pos(b.lat, b.lon)
            && matches!(b.status.as_str(), "ok" | "refused" | "failed")
            && clean(&b.detail, 300)
            && b.bearing_deg.is_none_or(|d| (0.0..360.0).contains(&d))
            && b.sigma_deg.is_none_or(|s| s > 0.0 && s <= 180.0)
            && b.conf_db.is_none_or(|c| c.is_finite() && c.abs() < 1000.0)
            && (b.status != "ok"
                || (b.bearing_deg.is_some() && b.sigma_deg.is_some() && b.lat.is_some()));
        ok.then_some(b)
    }
}

pub struct Bearings;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS bearings (
    capture    TEXT NOT NULL,
    author_key TEXT NOT NULL,
    call       TEXT NOT NULL,
    kraken     TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    json       TEXT NOT NULL,
    PRIMARY KEY (capture, author_key, kraken)
);";

impl Projection for Bearings {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM bearings", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != "sdr.bearing" {
            return Ok(());
        }
        if let Some(b) = Bearing::from_payload(&e.payload) {
            tx.execute(
                "INSERT OR IGNORE INTO bearings (capture, author_key, call, kraken, ts, json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    b.capture,
                    e.author.key,
                    e.author.call,
                    b.kraken,
                    ts,
                    serde_json::to_string(&b)?
                ],
            )?;
        }
        Ok(())
    }
}

/// Bearings logged for a capture, as (station callsign, bearing), pinned to each
/// station's own node.
pub fn bearings(conn: &Connection, capture: &str) -> Result<Vec<(String, Bearing)>> {
    let mut st = conn.prepare(
        "SELECT author_key, call, json FROM bearings WHERE capture = ?1 ORDER BY call, kraken",
    )?;
    let rows: Vec<(String, String, String)> = st
        .query_map([capture], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut out = vec![];
    for (key, call, json) in rows {
        let Ok(b) = serde_json::from_str::<Bearing>(&json) else {
            continue;
        };
        if station_node(conn, &key)?.is_none_or(|n| n == b.node) {
            out.push((call, b));
        }
    }
    Ok(out)
}

/// Read a KrakenSDR's bearings for the length of a capture and say what it found.
pub async fn collect(
    app: Arc<App>,
    cap: Cap,
    k: KrakenCfg,
    node: String,
    home: Option<(f64, f64)>,
) -> Bearing {
    let mut out = Bearing {
        capture: cap.id.clone(),
        kraken: k.name.clone(),
        node: node.clone(),
        ..Default::default()
    };
    let req = &cap.req;
    if let Err(why) = app.cfg.may_measure(req.lo(), req.hi()) {
        out.status = "refused".into();
        out.detail = why;
        return out;
    }
    sleep_until(req.at_ms()).await;
    let end = req.at_ms() + req.secs.max(3) as i64 * 1000;
    let mut seen: Vec<Doa> = vec![];
    let mut other: Vec<u64> = vec![];
    let mut last_err = None;
    while now_ms() < end {
        match fetch(&app, &k.url).await {
            Ok(text) => {
                for d in parse(&text) {
                    if d.freq.abs_diff(req.freq) <= req.bw {
                        if !seen.iter().any(|s| s.ts_ms == d.ts_ms) {
                            seen.push(d);
                        }
                    } else if !other.contains(&d.freq) {
                        other.push(d.freq);
                    }
                }
            }
            Err(e) => last_err = Some(short_err(&e)),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let pos = match (k.lat, k.lon) {
        (Some(a), Some(o)) => Some((a, o)),
        _ => seen.iter().find_map(Doa::pos).or(home),
    };
    (out.lat, out.lon) = (pos.map(|p| p.0), pos.map(|p| p.1));
    let deg: Vec<f64> = seen.iter().map(|d| d.bearing(k.heading_deg)).collect();
    match circular_mean(&deg) {
        Some(_) if pos.is_none() => {
            out.status = "failed".into();
            out.detail =
                "no position for the KrakenSDR: set lat and lon for it, or give it GPS".into();
        }
        Some((mean, spread)) => {
            out.status = "ok".into();
            out.bearing_deg = Some((mean * 10.0).round() / 10.0 % 360.0);
            out.sigma_deg = Some((spread.max(k.sigma_deg) * 10.0).round() / 10.0);
            out.samples = deg.len() as u32;
            out.conf_db = Some(seen.iter().map(|d| d.conf).sum::<f64>() / seen.len() as f64);
        }
        None => {
            out.status = "failed".into();
            out.detail = match (last_err, other.is_empty()) {
                (Some(e), true) => format!("its DOA_value.html didn't answer: {e}"),
                (_, false) => format!(
                    "it isn't tuned to {} Hz; it reports {}",
                    req.freq,
                    other
                        .iter()
                        .map(|f| f.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                (None, true) => {
                    "it gave no bearings; is its output format set to Kraken App?".into()
                }
            };
        }
    }
    out
}

async fn fetch(app: &App, url: &str) -> Result<String> {
    Ok(app
        .http
        .get(url)
        .timeout(Duration::from_secs(3))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// One `DOA_value.html` line in krakensdr_doa's "Kraken App" format, for tests and demos.
pub fn line(ts_ms: i64, rel_deg: f64, freq: u64, lat: f64, lon: f64, heading: f64) -> String {
    let mut s = format!(
        "{ts_ms}, {rel_deg}, 14.2, -35.5, {freq}, UCA, 412, TST-KRAKEN, {lat}, {lon}, {heading}, {heading}, GPS, R, R, R, R"
    );
    for i in 0..360 {
        let d = ((i as f64 - (360.0 - rel_deg)).rem_euclid(360.0))
            .min((360.0 - rel_deg - i as f64).rem_euclid(360.0));
        s.push_str(&format!(", {:.2}", (20.0 - d / 4.0).max(0.0)));
    }
    s.push_str(" \n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_kraken_app_format() {
        let text = line(1795135000123, 300.0, 146_520_000, 35.33, -120.62, 45.0)
            + &line(1795135000123, 10.0, 147_000_000, 0.0, 0.0, 0.0);
        let d = parse(&text);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].freq, 146_520_000);
        assert_eq!(d[0].bearing(None), 345.0, "relative 300 plus heading 45");
        assert_eq!(d[0].bearing(Some(90.0)), 30.0, "configured heading wins");
        assert_eq!(d[0].pos(), Some((35.33, -120.62)));
        assert_eq!(d[1].pos(), None, "0, 0 means no GPS");
        assert!(parse("<html>not running</html>\n1, 2, 3").is_empty());
    }

    #[test]
    fn bearings_average_across_north() {
        let (m, s) = circular_mean(&[358.0, 2.0, 0.0]).unwrap();
        assert!(m < 0.01 || m > 359.99, "{m}");
        assert!(s > 1.0 && s < 3.0, "{s}");
        assert!(circular_mean(&[]).is_none());
    }

    #[test]
    fn hostile_bearings_are_ignored() {
        let id = "b".repeat(64);
        let ok = serde_json::json!({"capture": id, "kraken": "k1", "node": "tst-a", "lat": 35.0,
            "lon": -120.0, "status": "ok", "bearing_deg": 12.5, "sigma_deg": 5.0, "samples": 4});
        assert!(Bearing::from_payload(&ok).is_some());
        for (k, v) in [
            ("bearing_deg", serde_json::json!(400.0)),
            ("sigma_deg", serde_json::json!(0.0)),
            ("detail", serde_json::json!("\u{7}")),
            ("lat", serde_json::Value::Null),
        ] {
            let mut b = ok.clone();
            b[k] = v;
            assert!(Bearing::from_payload(&b).is_none(), "{k}");
        }
    }
}
