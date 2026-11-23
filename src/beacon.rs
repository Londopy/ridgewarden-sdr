//! The weekly accuracy test. Once a week one site asks every site to locate a beacon whose
//! position was surveyed, then publishes how far the fix missed it, signed, so the record
//! of real accuracy lasts and every site shows the same history.

use anyhow::Result;
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::util::now_ms;

use crate::app::App;
use crate::capture::{self, Cap, RefTx, Request, Spot};
use crate::config::{BeaconCfg, Config};
use crate::directory::{clean, valid_pos};
use crate::geo::{LatLon, distance_km};
use crate::locate;
use crate::tdoa::{self, Pair, trim_pairs};

const DAY: i64 = 86_400_000;
/// The request goes out this long before the test, so every site has it in time.
const ASK_BEFORE_MS: i64 = 15 * 60_000;
/// The result is worked out this long after, once readings and snippets have synced.
const RESULT_AFTER_MS: i64 = 20 * 60_000;

/// The start of the test's slot in the week that holds `now`, UTC.
pub fn slot(b: &BeaconCfg, now: i64) -> i64 {
    let days = now.div_euclid(DAY);
    // 1970-01-01 was a Thursday
    let monday = days - (days + 3).rem_euclid(7);
    (monday + b.weekday_index().unwrap_or(0)) * DAY
        + b.hour_utc as i64 * 3_600_000
        + b.minute as i64 * 60_000
}

/// The `sdr.beacon` payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Outcome {
    pub capture: String,
    pub name: String,
    /// when the test ran, ms
    pub at: i64,
    pub spot_lat: f64,
    pub spot_lon: f64,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub miss_km: Option<f64>,
    #[serde(default)]
    pub major_km: Option<f64>,
    #[serde(default)]
    pub minor_km: Option<f64>,
    /// whether the surveyed spot fell inside the fix's 95% ellipse
    #[serde(default)]
    pub inside: Option<bool>,
    /// e.g. "3 time difference, 2 bearing"
    #[serde(default)]
    pub used: String,
    /// why there was no fix
    #[serde(default)]
    pub problem: Option<String>,
    /// the test's time differences, to set against the delays the surveyed spot gives
    #[serde(default)]
    pub pairs: Vec<Pair>,
}

impl Outcome {
    pub fn from_payload(v: &Value) -> Option<Self> {
        let mut o: Outcome = serde_json::from_value(v.clone()).ok()?;
        trim_pairs(&mut o.pairs);
        let num = |x: Option<f64>| x.is_none_or(|v| v.is_finite() && (0.0..20_000.0).contains(&v));
        let ok = o.capture.len() == 64
            && clean(&o.name, 40)
            && clean(&o.used, 200)
            && o.problem.as_deref().is_none_or(|p| clean(p, 400))
            && valid_pos(Some(o.spot_lat), Some(o.spot_lon))
            && valid_pos(o.lat, o.lon)
            && num(o.miss_km)
            && num(o.major_km)
            && num(o.minor_km)
            && o.at > 0;
        ok.then_some(o)
    }
}

pub struct Beacons;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS beacon_results (
    capture    TEXT NOT NULL,
    author_key TEXT NOT NULL,
    call       TEXT NOT NULL,
    at         INTEGER NOT NULL,
    json       TEXT NOT NULL,
    PRIMARY KEY (capture, author_key)
);";

impl Projection for Beacons {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM beacon_results", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if e.kind != "sdr.beacon" {
            return Ok(());
        }
        if let Some(mut o) = Outcome::from_payload(&e.payload) {
            // the test ran before its result reached this site, whatever its author's clock says
            o.at = o.at.min(ts);
            tx.execute(
                "INSERT OR IGNORE INTO beacon_results (capture, author_key, call, at, json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    o.capture,
                    e.author.key,
                    e.author.call,
                    o.at,
                    serde_json::to_string(&o)?
                ],
            )?;
        }
        Ok(())
    }
}

/// The latest thousand test results, oldest first, with who published them. Only results
/// for a beacon capture, published by the station that asked for it, count, and of their
/// time differences only those between snippets really published for the capture.
pub fn history(conn: &Connection) -> Result<Vec<(String, Outcome)>> {
    let mut st = conn.prepare(
        "SELECT * FROM (SELECT b.call, b.json, b.at FROM beacon_results b
             JOIN captures c ON c.id = b.capture AND c.author_key = b.author_key
             WHERE c.json LIKE '%\"beacon\":%' ORDER BY b.at DESC LIMIT 1000)
         ORDER BY at",
    )?;
    let rows: Vec<(String, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut out = vec![];
    for (call, json) in rows {
        let Ok(mut o) = serde_json::from_str::<Outcome>(&json) else {
            continue;
        };
        if !o.pairs.is_empty() {
            tdoa::matched(&mut o.pairs, &tdoa::snippets(conn, &o.capture)?);
        }
        out.push((call, o));
    }
    Ok(out)
}

/// Median miss of the latest four tests against the four before, km: (now, before).
pub fn trend(h: &[(String, Outcome)]) -> Option<(f64, f64)> {
    let misses: Vec<f64> = h.iter().filter_map(|(_, o)| o.miss_km).collect();
    if misses.len() < 8 {
        return None;
    }
    let n = misses.len();
    let mut late = misses[n - 4..].to_vec();
    let mut early = misses[n - 8..n - 4].to_vec();
    Some((
        ridgewarden_client::util::median(&mut late)?,
        ridgewarden_client::util::median(&mut early)?,
    ))
}

/// What the fix of a beacon capture says about accuracy.
pub fn outcome(conn: &Connection, cfg: &Config, cap: &Cap) -> Result<Option<Outcome>> {
    let Some(spot) = &cap.req.beacon else {
        return Ok(None);
    };
    let (_, fix) = locate::capture_fix(conn, cfg, cap)?;
    let truth = LatLon::new(spot.lat, spot.lon);
    let mut o = Outcome {
        capture: cap.id.clone(),
        name: spot.name.clone(),
        at: cap.req.at_ms(),
        spot_lat: spot.lat,
        spot_lon: spot.lon,
        pairs: tdoa::result(conn, &cap.id)?.map_or_else(Vec::new, |(_, s)| s.pairs),
        ..Default::default()
    };
    match fix {
        Ok(f) => {
            let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
            o.lat = Some((f.pos.lat * 1e6).round() / 1e6);
            o.lon = Some((f.pos.lon * 1e6).round() / 1e6);
            o.miss_km = Some(r3(distance_km(f.pos, truth)));
            o.major_km = Some(r3(f.ellipse.major_km));
            o.minor_km = Some(r3(f.ellipse.minor_km));
            o.inside = Some(f.ellipse.contains(f.pos, truth));
            o.used = f
                .used
                .iter()
                .map(|(k, n)| format!("{n} {k}"))
                .collect::<Vec<_>>()
                .join(", ");
        }
        Err(why) => o.problem = Some(why.chars().take(400).collect()),
    }
    Ok(Some(o))
}

fn request(cfg: &Config, b: &BeaconCfg, at: i64) -> Result<Request, String> {
    let mut r = Request::new(
        cfg,
        b.freq_hz(),
        b.bw_hz(),
        b.secs,
        at,
        "weekly beacon test",
        at,
    )?;
    r.beacon = Some(Spot {
        name: b.name.clone(),
        lat: b.lat,
        lon: b.lon,
    });
    if let Some(name) = &b.reference
        && let Some(rc) = cfg.reference(name)
    {
        r = r.with_reference(
            cfg,
            RefTx {
                name: rc.name.clone(),
                freq: rc.freq_hz(),
                bw: rc.bw_hz(),
                lat: rc.lat,
                lon: rc.lon,
            },
        )?;
    }
    Ok(r)
}

fn mine_at(conn: &Connection, key: &str, at: i64) -> Result<Option<Cap>> {
    Ok(capture::between(conn, at, at + 1)?
        .into_iter()
        .find(|c| c.key == key && c.req.at_ms() == at && c.req.beacon.is_some()))
}

/// Ask for this week's test when it is near, and publish its result when it is in.
pub async fn round(app: &App) -> Result<()> {
    let Some(b) = app.cfg.beacon.as_ref().filter(|b| b.schedule) else {
        return Ok(());
    };
    let now = now_ms();
    let key = app.whoami().await?.key;
    // this week's slot, or last week's while its result is still due
    for at in [slot(b, now), slot(b, now) - 7 * DAY] {
        let existing = mine_at(&app.db(), &key, at)?;
        match existing {
            None if (at - ASK_BEFORE_MS..at - 120_000).contains(&now) => {
                let req = request(&app.cfg, b, at).map_err(anyhow::Error::msg)?;
                app.core
                    .append("sdr.capture", serde_json::to_value(&req)?)
                    .await?;
                tracing::info!(
                    "asked for the weekly beacon test at {}",
                    ridgewarden_client::util::short_time(at)
                );
            }
            Some(cap) if now > at + RESULT_AFTER_MS && now < at + DAY => {
                let done: bool = app.db().query_row(
                    "SELECT count(*) > 0 FROM beacon_results WHERE capture = ?1 AND author_key = ?2",
                    params![cap.id, key],
                    |r| r.get(0),
                )?;
                if done {
                    continue;
                }
                let o = outcome(&app.db(), &app.cfg, &cap)?.expect("a beacon capture");
                app.core
                    .append("sdr.beacon", serde_json::to_value(&o)?)
                    .await?;
                tracing::info!(
                    "beacon test result published: {}",
                    o.miss_km.map_or_else(
                        || o.problem.clone().unwrap_or_default(),
                        |m| format!("missed by {m} km")
                    )
                );
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;
    use crate::geo::offset;
    use crate::testing::{ev, init};
    use ridgewarden_client::util::{ms_to_rfc3339, rfc3339_to_ms};
    use serde_json::json;

    fn cfg() -> BeaconCfg {
        BeaconCfg {
            name: "TST beacon".into(),
            freq_mhz: 144.285,
            bw_khz: 16.0,
            lat: 35.4,
            lon: -120.6,
            weekday: "sun".into(),
            hour_utc: 18,
            minute: 30,
            secs: 10,
            schedule: true,
            reference: None,
        }
    }

    #[test]
    fn the_slot_is_this_weeks_day_and_hour() {
        // 2026-11-19 is a Thursday
        let now = rfc3339_to_ms("2026-11-19T12:00:00Z").unwrap();
        assert_eq!(ms_to_rfc3339(slot(&cfg(), now)), "2026-11-22T18:30:00Z");
        let mon = BeaconCfg {
            weekday: "monday".into(),
            ..cfg()
        };
        assert_eq!(ms_to_rfc3339(slot(&mon, now)), "2026-11-16T18:30:00Z");
    }

    #[test]
    fn a_beacon_capture_reports_its_miss_distance() {
        let mut c = init();
        let config: Config = toml::from_str(EXAMPLE).unwrap();
        let spot = LatLon::new(35.4, -120.6);
        let req = json!({"freq": 144_285_000u64, "bw": 16_000u64, "secs": 10, "at": "2026-11-22T18:30:00Z",
                         "beacon": {"name": "TST beacon", "lat": spot.lat, "lon": spot.lon}});
        let at = rfc3339_to_ms("2026-11-22T18:30:00Z").unwrap();
        let id = ev(&mut c, "aa", "sdr.capture", req, at - 600_000).id;
        // four sites hear it, strongest nearest; the fix lands near the spot
        for (i, (e, n)) in [(-12.0, -8.0), (11.0, -10.0), (2.0, 14.0), (-10.0, 9.0)]
            .iter()
            .enumerate()
        {
            let site = offset(spot, *e, *n);
            let snr = 60.0 - 30.0 * distance_km(spot, site).log10();
            ev(
                &mut c,
                &format!("k{i}"),
                "sdr.reading",
                json!({"capture": id, "receiver": "rx1", "node": format!("tst-{i}"),
                "status": "ok", "level_db": -20.0 + snr, "floor_db": -20.0, "bins": 1, "lat": site.lat, "lon": site.lon}),
                at + 5000,
            );
        }
        let cap = capture::find(&c, &id).unwrap().unwrap();
        let o = outcome(&c, &config, &cap).unwrap().unwrap();
        assert!(o.miss_km.unwrap() < 1.0, "{o:?}");
        assert_eq!(o.used, "4 signal strength");
        assert!(o.major_km.unwrap() > 0.0);
        let back = Outcome::from_payload(&serde_json::to_value(&o).unwrap()).unwrap();
        assert_eq!(back, o);
    }

    #[test]
    fn a_beacon_result_keeps_only_the_time_differences_it_measured() {
        let mut c = init();
        let config: Config = toml::from_str(EXAMPLE).unwrap();
        let spot = LatLon::new(35.4, -120.6);
        let req = json!({"freq": 144_285_000u64, "bw": 16_000u64, "secs": 10, "at": "2026-11-22T18:30:00Z",
                         "beacon": {"name": "TST beacon", "lat": spot.lat, "lon": spot.lon},
                         "reference": {"name": "TST digipeater", "freq": 144_390_000u64, "bw": 16_000u64,
                                       "lat": 35.45, "lon": -120.55}});
        let at = rfc3339_to_ms("2026-11-22T18:30:00Z").unwrap();
        let id = ev(&mut c, "aa", "sdr.capture", req, at - 600_000).id;
        let sites = [offset(spot, -12.0, -8.0), offset(spot, 11.0, -10.0)];
        let end = |i: usize| {
            json!({"key": format!("k{i}"), "receiver": "rx1", "node": format!("tst-{i}"),
                   "lat": sites[i].lat, "lon": sites[i].lon})
        };
        for i in 0..2 {
            let mut note = end(i);
            note.as_object_mut().unwrap().remove("key");
            note["capture"] = json!(id);
            note["root"] = json!("d".repeat(64));
            note["clipped"] = json!(0.0);
            ev(&mut c, &format!("k{i}"), "sdr.snippet", note, at + 5000);
        }
        let pair = |dd_km: f64| {
            json!({"a": end(0), "b": end(1), "dd_km": dd_km, "sigma_km": 0.9,
                   "psr_target": 14.0, "psr_ref": 30.0})
        };
        let sol = json!({"capture": id, "snippets": 2, "pairs": [pair(3.2)]});
        ev(&mut c, "solver", "sdr.tdoa", sol, at + 60_000);

        let cap = capture::find(&c, &id).unwrap().unwrap();
        let o = outcome(&c, &config, &cap).unwrap().unwrap();
        assert_eq!(o.pairs.len(), 1);
        assert_eq!(o.pairs[0].dd_km, 3.2);
        let mut v = serde_json::to_value(&o).unwrap();
        assert_eq!(Outcome::from_payload(&v).unwrap(), o);
        // a path difference longer than the sites are apart can't have been measured
        v["pairs"] = json!([pair(3.2), pair(80.0)]);
        let back = Outcome::from_payload(&v).unwrap();
        assert_eq!(
            back.pairs.len(),
            1,
            "the impossible pair is dropped, the result kept"
        );

        // published dated a year ahead, led by a pair from a receiver that sent no snippet
        let mut forged = pair(1.0);
        forged["b"]["receiver"] = json!("rx9");
        v["pairs"] = json!([forged, pair(3.2)]);
        v["at"] = json!(at + 400 * DAY);
        let arrived = at + RESULT_AFTER_MS;
        ev(&mut c, "aa", "sdr.beacon", v, arrived);
        let h = history(&c).unwrap();
        assert_eq!(h.len(), 1);
        let dd: Vec<f64> = h[0].1.pairs.iter().map(|p| p.dd_km).collect();
        assert_eq!(dd, [3.2], "only pairs between published snippets count");
        assert_eq!(
            h[0].1.at, arrived,
            "a test can't have run after its result arrived"
        );
    }

    #[test]
    fn trend_compares_the_last_four_tests_with_the_four_before() {
        let h: Vec<(String, Outcome)> = [3.0, 2.5, 2.8, 2.6, 1.2, 1.0, 0.9, 1.1]
            .iter()
            .map(|m| {
                (
                    "N0CALL".to_string(),
                    Outcome {
                        miss_km: Some(*m),
                        ..Default::default()
                    },
                )
            })
            .collect();
        assert_eq!(trend(&h), Some((1.05, 2.7)));
        assert_eq!(trend(&h[..7]), None);
    }
}
