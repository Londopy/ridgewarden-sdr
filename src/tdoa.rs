//! Time differences of arrival, aligned on a reference transmitter.
//!
//! RTL-SDRs have no common clock: their recordings start up to a few hundred ms apart and
//! their sample clocks drift by tens of ppm. Each snippet therefore carries a second
//! channel with a reference transmitter at a surveyed spot. Its arrival-time difference
//! between two sites is known from geometry, so whatever the cross-correlation measures
//! beyond that is clock offset; measuring it in four parts of the snippet gives the drift
//! too, and both are taken out of the target's time difference (Panoradio's method).
//!
//! Correlation runs on the FM discriminator output, with GCC-PHAT, so a carrier offset
//! between dongles drops out. A peak that doesn't stand well clear of the rest is refused:
//! a plain carrier or a carrier with only a CTCSS tone has nothing to time, and the page
//! says so rather than show a fix built on noise.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::util::valid_node;

use crate::config::valid_name;
use crate::directory::{MAX_PER_STATION, clean, station_node, valid_pos};
use crate::dsp::{Peak, discriminator, gcc_window};
use crate::geo::{C_KM_S, LatLon, distance_km};
use crate::iq::Snippet;

/// Peak height over the spread of the rest of the search window needed to trust a lag.
pub const PSR_MIN: f64 = 8.0;
/// A peak this clear earns the configured timing error; weaker ones get more.
const PSR_GOOD: f64 = 15.0;
const PARTS: usize = 4;
/// Clock drift moves the reference lag only a few samples across a snippet.
const DRIFT_SEARCH: usize = 20;
const MAX_PAIRS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    /// how far apart two sites' recordings may start, s
    pub max_skew_s: f64,
    /// timing error of a clean pair, s (1 sigma)
    pub sigma_s: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct End {
    pub key: String,
    pub receiver: String,
    pub node: String,
    pub lat: f64,
    pub lon: f64,
}

impl End {
    pub fn pos(&self) -> LatLon {
        LatLon::new(self.lat, self.lon)
    }
}

/// One measured time difference: `dd_km` = d(transmitter, b) - d(transmitter, a).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pair {
    pub a: End,
    pub b: End,
    pub dd_km: f64,
    pub sigma_km: f64,
    pub psr_target: f64,
    pub psr_ref: f64,
}

/// The time difference between two sites' snippets, or why there is none.
pub fn pair(
    (a, a_pos): (&Snippet, LatLon),
    (b, b_pos): (&Snippet, LatLon),
    ref_pos: LatLon,
    o: &Opts,
) -> Result<(f64, f64, f64, f64), String> {
    let rate = a.header.rate;
    for (s, who) in [(a, "first"), (b, "second")] {
        if s.header.clipped > 0.01 {
            return Err(format!(
                "the {who} site's ADC clipped on {:.1}% of samples: desense from a nearby \
                 transmitter; add a band-pass filter or lower the gain",
                s.header.clipped * 100.0
            ));
        }
    }
    let (ra, rb) = (discriminator(&a.reference), discriminator(&b.reference));
    let (ta, tb) = (discriminator(&a.target), discriminator(&b.target));
    let len = ra.len().min(rb.len());
    // recording start a minus start b, in samples: where the lags will be
    let skew = (a.header.first_ms - b.header.first_ms) as f64 / 1000.0 * rate;
    let w = (o.max_skew_s * rate) as usize;
    // most of an FM channel's timing detail sits in the lower part of its bandwidth
    let band = |bw: u64| (0.4 * bw as f64 / rate).min(0.5);
    let (band_r, band_t) = (band(a.header.ref_bw), band(a.header.target_bw));
    let whole = gcc_window(&ra, &rb, 0, len, skew, w, band_r).filter(|p| p.psr >= PSR_MIN);
    let Some(whole) = whole else {
        return Err(
            "the reference transmitter isn't clear enough at both sites to line their clocks up"
                .into(),
        );
    };
    let parts: Vec<(usize, usize, Peak)> = (0..PARTS)
        .filter_map(|i| {
            let (s, e) = (i * len / PARTS, (i + 1) * len / PARTS);
            let p = gcc_window(&ra, &rb, s, e, whole.lag, DRIFT_SEARCH, band_r)?;
            (p.psr >= PSR_MIN).then_some((s, e, p))
        })
        .collect();
    let (slope, mid, at_mid) = drift(&parts).unwrap_or((0.0, len as f64 / 2.0, whole.lag));
    let psr_ref = parts.iter().map(|p| p.2.psr).fold(whole.psr, f64::min);
    // searched as widely as the reference, so the peak is judged against the same spread
    let t = gcc_window(&ta, &tb, 0, len, whole.lag, w, band_t);
    let t = match t {
        Some(p) if p.psr >= PSR_MIN => p,
        other => {
            let psr = other.map_or(0.0, |p| p.psr);
            return Err(format!(
                "nothing to time on the target (peak {psr:.1} times the spread, needs {PSR_MIN}): \
                 a plain carrier, or one with only a CTCSS tone, has no detail to correlate. \
                 Time differences need data bursts, digital voice or key-ups"
            ));
        }
    };
    // a transmitter is at most the sites' distance closer to one than the other; the
    // reference's own path difference can add as much again before it is taken out
    let apart = distance_km(a_pos, b_pos);
    let base = apart / C_KM_S * rate;
    if (t.lag - whole.lag).abs() > 2.0 * base + DRIFT_SEARCH as f64 {
        return Err(multipath((t.lag - whole.lag).abs() / rate * C_KM_S, apart));
    }
    // where the target is on in a part, time it against the reference in that same part,
    // which takes the clocks' offset and drift out exactly; a burst too short for any part
    // is timed against the reference's drift line instead
    let same: Vec<f64> = parts
        .iter()
        .filter_map(|(s, e, rp)| {
            let p = gcc_window(
                &ta,
                &tb,
                *s,
                *e,
                rp.lag + t.lag - whole.lag,
                DRIFT_SEARCH,
                band_t,
            )?;
            (p.psr >= PSR_MIN).then_some(p.lag - rp.lag)
        })
        .collect();
    let offset = if same.is_empty() {
        t.lag - (at_mid + slope * (activity_center(a) - mid))
    } else {
        same.iter().sum::<f64>() / same.len() as f64
    };
    let g = (distance_km(ref_pos, b_pos) - distance_km(ref_pos, a_pos)) / C_KM_S;
    let tdoa = offset / rate + g;
    if (tdoa * C_KM_S).abs() > apart + 2.0 / rate * C_KM_S {
        return Err(multipath((tdoa * C_KM_S).abs(), apart));
    }
    let quality = (PSR_GOOD / t.psr.min(psr_ref)).max(1.0);
    Ok((tdoa * C_KM_S, o.sigma_s * quality * C_KM_S, t.psr, psr_ref))
}

fn multipath(dd_km: f64, apart_km: f64) -> String {
    format!(
        "the best match for the target means a path difference of {dd_km:.1} km, more than the          {apart_km:.1} km between the sites allows: multipath, or two transmitters on the frequency"
    )
}

/// Least-squares line through the reference lags: (slope, mean position, lag there).
fn drift(parts: &[(usize, usize, Peak)]) -> Option<(f64, f64, f64)> {
    if parts.len() < 2 {
        return None;
    }
    let n = parts.len() as f64;
    let at = |p: &(usize, usize, Peak)| (p.0 + p.1) as f64 / 2.0;
    let mx = parts.iter().map(at).sum::<f64>() / n;
    let my = parts.iter().map(|p| p.2.lag).sum::<f64>() / n;
    let sxx: f64 = parts.iter().map(|p| (at(p) - mx).powi(2)).sum();
    let sxy: f64 = parts.iter().map(|p| (at(p) - mx) * (p.2.lag - my)).sum();
    Some((if sxx > 0.0 { sxy / sxx } else { 0.0 }, mx, my))
}

/// Where in the snippet the target is, weighted by its power: a short burst is timed at
/// the burst, not at the middle of the recording.
fn activity_center(s: &Snippet) -> f64 {
    let (mut num, mut den) = (0.0, 0.0);
    for (i, c) in s.target.iter().enumerate() {
        let p = c.norm_sqr();
        num += p * i as f64;
        den += p;
    }
    if den > 0.0 {
        num / den
    } else {
        s.target.len() as f64 / 2.0
    }
}

/// Every site's time difference against the site that hears the target best.
pub fn analyze(snips: &[(End, Snippet)], ref_pos: LatLon, o: &Opts) -> (Vec<Pair>, Vec<String>) {
    let power = |s: &Snippet| s.target.iter().map(|c| c.norm_sqr()).sum::<f64>();
    let Some(base) =
        (0..snips.len()).max_by(|x, y| power(&snips[*x].1).total_cmp(&power(&snips[*y].1)))
    else {
        return (vec![], vec![]);
    };
    let (ea, sa) = &snips[base];
    let (mut pairs, mut problems) = (vec![], vec![]);
    for (i, (eb, sb)) in snips.iter().enumerate() {
        if i == base {
            continue;
        }
        match pair((sa, ea.pos()), (sb, eb.pos()), ref_pos, o) {
            Ok((dd_km, sigma_km, psr_target, psr_ref)) => pairs.push(Pair {
                a: ea.clone(),
                b: eb.clone(),
                dd_km,
                sigma_km,
                psr_target,
                psr_ref,
            }),
            Err(why) => problems.push(format!(
                "{} {} and {} {}: {why}",
                ea.node, ea.receiver, eb.node, eb.receiver
            )),
        }
    }
    (pairs, problems)
}

/// The `sdr.snippet` payload: a site recorded a capture and published the snippet as a blob.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SnippetNote {
    pub capture: String,
    pub receiver: String,
    pub node: String,
    pub lat: f64,
    pub lon: f64,
    pub root: String,
    pub clipped: f64,
}

/// The `sdr.tdoa` payload: what a solving site worked out from everyone's snippets.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Solution {
    pub capture: String,
    pub pairs: Vec<Pair>,
    #[serde(default)]
    pub problems: Vec<String>,
    /// snippets it had
    pub snippets: usize,
}

fn hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_end(e: &End) -> bool {
    valid_name(&e.receiver)
        && valid_node(&e.node)
        && valid_pos(Some(e.lat), Some(e.lon))
        && e.key.len() <= 64
}

impl SnippetNote {
    pub fn from_payload(v: &Value) -> Option<Self> {
        let s: SnippetNote = serde_json::from_value(v.clone()).ok()?;
        let ok = hex64(&s.capture)
            && hex64(&s.root)
            && valid_name(&s.receiver)
            && valid_node(&s.node)
            && valid_pos(Some(s.lat), Some(s.lon))
            && (0.0..=1.0).contains(&s.clipped);
        ok.then_some(s)
    }
}

impl Solution {
    /// Read a payload from the feed: malformed pairs and problems are dropped, and a long
    /// list is cut short rather than refused.
    pub fn from_payload(v: &Value) -> Option<Self> {
        let mut r: Solution = serde_json::from_value(v.clone()).ok()?;
        if !hex64(&r.capture) {
            return None;
        }
        r.problems.retain(|p| clean(p, 600));
        r.problems.truncate(MAX_PAIRS);
        r.pairs.retain(|p| {
            valid_end(&p.a)
                && valid_end(&p.b)
                && p.dd_km.is_finite()
                && p.dd_km.abs() <= distance_km(p.a.pos(), p.b.pos()) + 5.0
                && p.sigma_km.is_finite()
                && p.sigma_km > 0.0
                && p.sigma_km <= 100.0
                && p.psr_target.is_finite()
                && p.psr_ref.is_finite()
        });
        r.pairs.truncate(MAX_PAIRS);
        Some(r)
    }
}

pub struct Tdoa;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS snippets (
    capture    TEXT NOT NULL,
    author_key TEXT NOT NULL,
    receiver   TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    json       TEXT NOT NULL,
    PRIMARY KEY (capture, author_key, receiver)
);
CREATE TABLE IF NOT EXISTS tdoa_results (
    capture    TEXT NOT NULL,
    author_key TEXT NOT NULL,
    call       TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    json       TEXT NOT NULL,
    PRIMARY KEY (capture, author_key)
);";

impl Projection for Tdoa {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch("DELETE FROM snippets; DELETE FROM tdoa_results;")?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        match e.kind.as_str() {
            "sdr.snippet" => {
                let n: i64 = tx.query_row(
                    "SELECT count(*) FROM snippets WHERE capture = ?1 AND author_key = ?2",
                    params![
                        e.payload
                            .get("capture")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        e.author.key
                    ],
                    |r| r.get(0),
                )?;
                if n >= MAX_PER_STATION {
                    return Ok(());
                }
                if let Some(s) = SnippetNote::from_payload(&e.payload) {
                    tx.execute(
                        "INSERT OR IGNORE INTO snippets (capture, author_key, receiver, ts, json)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            s.capture,
                            e.author.key,
                            s.receiver,
                            ts,
                            serde_json::to_string(&s)?
                        ],
                    )?;
                }
            }
            "sdr.tdoa" => {
                if let Some(r) = Solution::from_payload(&e.payload) {
                    tx.execute(
                        "INSERT INTO tdoa_results (capture, author_key, call, ts, json) VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(capture, author_key) DO UPDATE SET ts = excluded.ts, json = excluded.json
                         WHERE excluded.ts >= tdoa_results.ts",
                        params![r.capture, e.author.key, e.author.call, ts, serde_json::to_string(&r)?],
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Snippets published for a capture, with the publishing station's key; each pinned to
/// the node that station's core polls.
pub fn snippets(conn: &Connection, capture: &str) -> Result<Vec<(String, SnippetNote)>> {
    let mut st = conn.prepare(
        "SELECT author_key, json FROM snippets WHERE capture = ?1 ORDER BY author_key, receiver",
    )?;
    let rows: Vec<(String, String)> = st
        .query_map([capture], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut out = vec![];
    for (key, json) in rows {
        let Ok(s) = serde_json::from_str::<SnippetNote>(&json) else {
            continue;
        };
        if station_node(conn, &key)?.is_none_or(|n| n == s.node) {
            out.push((key, s));
        }
    }
    Ok(out)
}

/// The latest time-difference result for a capture: the one from the solving site that
/// used the most snippets, newest first among equals.
pub fn result(conn: &Connection, capture: &str) -> Result<Option<(String, Solution)>> {
    let mut st =
        conn.prepare("SELECT call, json FROM tdoa_results WHERE capture = ?1 ORDER BY ts DESC")?;
    let rows: Vec<(String, String)> = st
        .query_map([capture], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    // only pairs between snippets that were really published, from where they were published
    let notes = snippets(conn, capture)?;
    let known = |e: &End| {
        notes.iter().any(|(k, n)| {
            *k == e.key
                && n.receiver == e.receiver
                && n.node == e.node
                && n.lat == e.lat
                && n.lon == e.lon
        })
    };
    let mut best: Option<(String, Solution)> = None;
    for (call, json) in rows {
        let Ok(mut sol) = serde_json::from_str::<Solution>(&json) else {
            continue;
        };
        sol.pairs.retain(|p| known(&p.a) && known(&p.b));
        sol.snippets = sol.snippets.min(notes.len());
        // newest first, so among equals the latest stands
        if best
            .as_ref()
            .is_none_or(|(_, b)| sol.pairs.len() > b.pairs.len())
        {
            best = Some((call, sol));
        }
    }
    Ok(best)
}

/// Local copies of snippets this site has fetched, for solving.
pub const LOCAL: &str = "
CREATE TABLE IF NOT EXISTS iq_have (
    capture    TEXT NOT NULL,
    author_key TEXT NOT NULL,
    receiver   TEXT NOT NULL,
    root       TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    data       BLOB NOT NULL,
    PRIMARY KEY (capture, author_key, receiver)
);
CREATE TABLE IF NOT EXISTS iq_solved (
    capture TEXT PRIMARY KEY,
    n       INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS iq_bad (
    root TEXT PRIMARY KEY
);";

/// Whether a snippet blob already turned out not to be one.
pub fn bad(conn: &Connection, root: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT count(*) > 0 FROM iq_bad WHERE root = ?1",
        [root],
        |r| r.get(0),
    )?)
}

pub fn mark_bad(conn: &Connection, root: &str) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO iq_bad (root) VALUES (?1)", [root])?;
    Ok(())
}

pub fn have(conn: &Connection, capture: &str) -> Result<Vec<(String, String, Vec<u8>)>> {
    let mut st =
        conn.prepare("SELECT author_key, receiver, data FROM iq_have WHERE capture = ?1")?;
    let rows = st.query_map([capture], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn solved(conn: &Connection, capture: &str) -> Result<usize> {
    Ok(conn
        .query_row(
            "SELECT n FROM iq_solved WHERE capture = ?1",
            [capture],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0) as usize)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::fix::{Obs, locate};
    use crate::geo::offset;
    use crate::iq::{Emitter, Header, RATE};

    pub fn center() -> LatLon {
        LatLon::new(35.4, -120.6)
    }

    /// A snippet as a site at `site` would record it: its recording starts at `t0` (s),
    /// its sample clock runs `ppm` fast, and its tuning is `off_hz` out.
    pub fn record(
        site: LatLon,
        tx: (LatLon, Emitter),
        rf: (LatLon, Emitter),
        t0: f64,
        ppm: f64,
        off_hz: f64,
    ) -> Snippet {
        let n = (RATE * 1.0) as usize;
        let delay = |p: LatLon| distance_km(p, site) / C_KM_S;
        let noisy = |mut v: Vec<crate::dsp::C>, seed: u64| {
            let mut s = seed;
            for c in v.iter_mut() {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let a = (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
                let b = ((s >> 3) & 0xffff) as f64 / 65536.0 - 0.5;
                *c = *c + crate::dsp::C::new(a * 0.2, b * 0.2);
            }
            v
        };
        let seed = ((site.lat * 1e5) as u64)
            ^ ((site.lon.abs() * 1e5) as u64).rotate_left(20)
            ^ (t0 * 1e3) as u64;
        Snippet {
            header: Header {
                capture: "c".into(),
                receiver: "rx".into(),
                rate: RATE,
                first_ms: (t0 * 1000.0) as i64,
                target_hz: 0,
                target_bw: 16_000,
                ref_hz: 0,
                ref_bw: 16_000,
                clipped: 0.0,
                scale_t: 0.0,
                scale_r: 0.0,
                n,
            },
            target: noisy(tx.1.samples(t0, n, RATE, delay(tx.0), ppm, off_hz), seed),
            reference: noisy(
                rf.1.samples(t0, n, RATE, delay(rf.0), ppm, off_hz),
                seed + 1,
            ),
        }
    }

    fn end(i: usize, p: LatLon) -> End {
        End {
            key: format!("k{i}"),
            receiver: "rx".into(),
            node: format!("tst-{i}"),
            lat: p.lat,
            lon: p.lon,
        }
    }

    const OPTS: Opts = Opts {
        max_skew_s: 0.25,
        sigma_s: 3e-6,
    };

    fn fsk(seed: u64) -> Emitter {
        Emitter {
            seed,
            dev: 600.0,
            amp: 1.0,
        }
    }

    #[test]
    fn reference_alignment_removes_clock_offsets_and_drift() {
        let c = center();
        let sites = [
            offset(c, -14.0, -9.0),
            offset(c, 13.0, -11.0),
            offset(c, 1.0, 15.0),
        ];
        let tx = offset(c, 3.0, 2.0);
        let rf = offset(c, -6.0, 20.0);
        // starts up to 180 ms apart, clocks 30 ppm apart, tuning a few hundred Hz out
        let starts = [100.0, 100.18, 99.93];
        let ppms = [0.0, 30.0, -25.0];
        let snips: Vec<(End, Snippet)> = (0..3)
            .map(|i| {
                (
                    end(i, sites[i]),
                    record(
                        sites[i],
                        (tx, fsk(7)),
                        (rf, fsk(99)),
                        starts[i],
                        ppms[i],
                        150.0 * i as f64,
                    ),
                )
            })
            .collect();
        let (pairs, problems) = analyze(&snips, rf, &OPTS);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(pairs.len(), 2);
        for p in &pairs {
            let truth = distance_km(tx, p.b.pos()) - distance_km(tx, p.a.pos());
            assert!((p.dd_km - truth).abs() < 0.3, "{} vs {truth}", p.dd_km);
        }
        let obs: Vec<Obs> = pairs
            .iter()
            .map(|p| Obs::Tdoa {
                a: p.a.pos(),
                b: p.b.pos(),
                dd_km: p.dd_km,
                sigma_km: p.sigma_km,
            })
            .collect();
        let f = locate(&obs, 3.0).unwrap();
        assert!(
            distance_km(f.pos, tx) < 2.0,
            "missed by {} km",
            distance_km(f.pos, tx)
        );
        assert!(f.inside);
    }

    #[test]
    fn a_plain_carrier_has_nothing_to_time() {
        let c = center();
        let sites = [offset(c, -10.0, 0.0), offset(c, 10.0, 0.0)];
        let carrier = Emitter {
            seed: 1,
            dev: 0.0,
            amp: 1.0,
        };
        let snips: Vec<(End, Snippet)> = (0..2)
            .map(|i| {
                (
                    end(i, sites[i]),
                    record(
                        sites[i],
                        (c, carrier),
                        (offset(c, 0.0, 20.0), fsk(5)),
                        50.0 + i as f64 * 0.05,
                        0.0,
                        0.0,
                    ),
                )
            })
            .collect();
        let (pairs, problems) = analyze(&snips, offset(c, 0.0, 20.0), &OPTS);
        assert!(pairs.is_empty());
        assert!(problems[0].contains("nothing to time"), "{problems:?}");
    }

    #[test]
    fn a_silent_reference_is_reported() {
        let c = center();
        let sites = [offset(c, -10.0, 0.0), offset(c, 10.0, 0.0)];
        let dead = Emitter {
            seed: 1,
            dev: 0.0,
            amp: 0.0,
        };
        let snips: Vec<(End, Snippet)> = (0..2)
            .map(|i| {
                (
                    end(i, sites[i]),
                    record(sites[i], (c, fsk(3)), (c, dead), 50.0, 0.0, 0.0),
                )
            })
            .collect();
        let (_, problems) = analyze(&snips, c, &OPTS);
        assert!(
            problems[0].contains("reference transmitter"),
            "{problems:?}"
        );
    }

    #[test]
    fn hostile_results_are_trimmed() {
        let id = "a".repeat(64);
        let good = serde_json::json!({"capture": id, "snippets": 3, "problems": [], "pairs": [{
            "a": {"key": "k", "receiver": "rx1", "node": "tst-a", "lat": 35.0, "lon": -120.0},
            "b": {"key": "k2", "receiver": "rx1", "node": "tst-b", "lat": 35.1, "lon": -120.1},
            "dd_km": 1.5, "sigma_km": 0.9, "psr_target": 12.0, "psr_ref": 20.0}]});
        assert_eq!(Solution::from_payload(&good).unwrap().pairs.len(), 1);
        let pairs = |v: &Value| Solution::from_payload(v).unwrap().pairs.len();
        let mut bad = good.clone();
        bad["pairs"][0]["dd_km"] = serde_json::json!(30.0);
        assert_eq!(pairs(&bad), 0, "longer than the sites are apart");
        let mut bad = good.clone();
        bad["pairs"][0]["a"]["node"] = serde_json::json!("<x>");
        assert_eq!(pairs(&bad), 0);
        let mut bad = good.clone();
        bad["problems"] = serde_json::json!(["\u{1b}[2J", "fine"]);
        assert_eq!(Solution::from_payload(&bad).unwrap().problems, ["fine"]);
        let mut long = good;
        long["problems"] = serde_json::json!(vec!["x"; 500]);
        assert_eq!(
            Solution::from_payload(&long).unwrap().problems.len(),
            MAX_PAIRS,
            "cut short, not refused"
        );
    }

    #[test]
    fn only_pairs_between_published_snippets_count() {
        let (mut c, id) = (crate::testing::init(), "c".repeat(64));
        let note = |rx: &str, lat: f64| {
            serde_json::json!({"capture": id, "receiver": rx, "node": "tst-a",
                "lat": lat, "lon": -120.0, "root": "d".repeat(64), "clipped": 0.0})
        };
        crate::testing::ev(&mut c, "k", "sdr.snippet", note("rx1", 35.0), 1);
        crate::testing::ev(&mut c, "k", "sdr.snippet", note("rx2", 35.1), 1);
        let end = |rx: &str, lat: f64| serde_json::json!({"key": "k", "receiver": rx, "node": "tst-a", "lat": lat, "lon": -120.0});
        let pair = |b: Value| {
            serde_json::json!({"a": end("rx1", 35.0), "b": b, "dd_km": 1.0, "sigma_km": 0.5,
                "psr_target": 12.0, "psr_ref": 20.0})
        };
        let honest =
            serde_json::json!({"capture": id, "snippets": 2, "pairs": [pair(end("rx2", 35.1))]});
        crate::testing::ev(&mut c, "s1", "sdr.tdoa", honest, 2);
        // later, claiming more snippets, with a pair at a position nobody published
        let forged =
            serde_json::json!({"capture": id, "snippets": 9, "pairs": [pair(end("rx2", 35.05))]});
        crate::testing::ev(&mut c, "s2", "sdr.tdoa", forged, 3);
        let (call, sol) = result(&c, &id).unwrap().unwrap();
        assert_eq!(call, "N0S1");
        assert_eq!(sol.pairs.len(), 1);
    }
}
