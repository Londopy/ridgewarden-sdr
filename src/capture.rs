//! Coordinated signal-strength capture. Someone asks for "measure <frequency> <bandwidth>
//! for N seconds at <time>" as a signed `sdr.capture` event; at that time every site with
//! a receiver that covers it runs rtl_power and logs an `sdr.reading` as telemetry. The
//! page ranks the readings: which site hears this loudest, and how loud.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::util::{ms_to_rfc3339, rfc3339_to_ms, valid_node};

use crate::bands::mhz;
use crate::config::{Config, valid_name};
use crate::directory::{self, MAX_PER_STATION, clean, station_node, valid_pos};
use crate::iq::{MAX_CHANNEL, MAX_SEP, MIN_SEP};
use crate::rtlpower;

/// A site that gets a request this late still runs it, to allow for clocks a little off.
pub const GRACE_MS: i64 = 60_000;
pub const MAX_SECS: u32 = 300;
pub const MIN_BW: u64 = 1_000;
pub const MAX_BW: u64 = 2_000_000;
pub const MAX_AHEAD_MS: i64 = 7 * 86_400_000;
/// Nothing an SDR on the mesh tunes is above this, and it keeps sums far from overflow.
pub const MAX_FREQ: u64 = 100_000_000_000;
/// Sites pick up captures this far ahead and wait, so they all start together.
pub const LOOKAHEAD_MS: i64 = 10_000;
/// Captures started per round, and per requesting station, so a flood can't pile up.
const MAX_DUE: usize = 32;
const MAX_DUE_PER_STATION: usize = 4;
/// Readings kept per capture; a mesh has nowhere near this many receivers.
const MAX_READINGS: i64 = 200;
/// Requests kept per station within a day of each other: a flood can't crowd others out.
const MAX_PER_STATION_DAY: i64 = 50;
const NOTE_MAX: usize = 200;

/// What every reading's numbers are, written into each event so the raw log explains itself.
pub const UNIT: &str =
    "dB relative to the receiver's ADC (rtl_power), uncalibrated unless calibration_db is set";
pub const UNIT_IQ: &str = "dB relative to the receiver's ADC (rtl_sdr spectrum), uncalibrated unless calibration_db is set";

/// A reference transmitter at a surveyed spot, for lining up sites' clocks.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RefTx {
    pub name: String,
    pub freq: u64,
    pub bw: u64,
    pub lat: f64,
    pub lon: f64,
}

/// A transmitter whose true position is known: the weekly accuracy test.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Spot {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

/// The `sdr.capture` payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Request {
    /// center, Hz
    pub freq: u64,
    /// bandwidth, Hz
    pub bw: u64,
    pub secs: u32,
    /// start time, RFC 3339 UTC
    pub at: String,
    #[serde(default)]
    pub note: String,
    /// asks sites with an rtl_sdr to record IQ snippets for time differences
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<RefTx>,
    /// set on the weekly beacon test
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beacon: Option<Spot>,
}

impl RefTx {
    pub fn lo(&self) -> u64 {
        self.freq - self.bw / 2
    }

    pub fn hi(&self) -> u64 {
        self.freq + (self.bw - self.bw / 2)
    }
}

impl Request {
    pub fn lo(&self) -> u64 {
        self.freq - self.bw / 2
    }

    pub fn hi(&self) -> u64 {
        self.freq + (self.bw - self.bw / 2)
    }

    pub fn at_ms(&self) -> i64 {
        rfc3339_to_ms(&self.at).unwrap_or(0)
    }

    pub fn summary(&self) -> String {
        format!(
            "{} wide at {}, {} s",
            khz(self.bw),
            mhz(self.freq),
            self.secs
        )
    }

    fn limits(&self) -> Result<(), String> {
        if !(MIN_BW..=MAX_BW).contains(&self.bw) {
            return Err(format!(
                "bandwidth must be {} to {}",
                khz(MIN_BW),
                khz(MAX_BW)
            ));
        }
        if self.freq <= self.bw || self.freq > MAX_FREQ {
            return Err("the frequency must be above the bandwidth and below 100 GHz".into());
        }
        if !(1..=MAX_SECS).contains(&self.secs) {
            return Err(format!("a capture lasts 1 to {MAX_SECS} seconds"));
        }
        if !clean(&self.note, NOTE_MAX) {
            return Err(format!(
                "the note is limited to {NOTE_MAX} characters, without control characters"
            ));
        }
        if let Some(r) = &self.reference {
            if !clean(&r.name, 40) || !valid_pos(Some(r.lat), Some(r.lon)) || r.freq <= r.bw {
                return Err("the reference needs a short name and a valid position".into());
            }
            if !(MIN_BW..=MAX_CHANNEL).contains(&r.bw) || self.bw > MAX_CHANNEL {
                return Err(format!(
                    "with a reference, both channels must be {} to {} wide",
                    khz(MIN_BW),
                    khz(MAX_CHANNEL)
                ));
            }
            let sep = self.freq.abs_diff(r.freq);
            if !(MIN_SEP..=MAX_SEP).contains(&sep) || r.freq > MAX_FREQ {
                return Err(format!(
                    "the reference has to be {} to {} from the target, to share one recording",
                    khz(MIN_SEP),
                    khz(MAX_SEP)
                ));
            }
        }
        if let Some(b) = &self.beacon
            && (!clean(&b.name, 40) || !valid_pos(Some(b.lat), Some(b.lon)))
        {
            return Err("the beacon needs a short name and a valid position".into());
        }
        if rfc3339_to_ms(&self.at).is_none() {
            return Err("the start time is not an RFC 3339 time".into());
        }
        Ok(())
    }

    pub fn from_payload(v: &Value) -> Option<Self> {
        let r: Request = serde_json::from_value(v.clone()).ok()?;
        r.limits().is_ok().then_some(r)
    }

    /// A new request from this site, which has to be allowed to measure it itself.
    pub fn new(
        cfg: &Config,
        freq: u64,
        bw: u64,
        secs: u32,
        at_ms: i64,
        note: &str,
        now: i64,
    ) -> Result<Self, String> {
        let r = Request {
            freq,
            bw,
            secs,
            at: ms_to_rfc3339(at_ms - at_ms.rem_euclid(1000)),
            note: note.trim().to_string(),
            reference: None,
            beacon: None,
        };
        r.limits()?;
        if at_ms < now - 5_000 {
            return Err("the start time has already passed".into());
        }
        if at_ms > now + MAX_AHEAD_MS {
            return Err("the start time is more than 7 days away".into());
        }
        cfg.may_measure(r.lo(), r.hi())?;
        Ok(r)
    }

    /// Add a reference transmitter, which this site must also be allowed to measure.
    pub fn with_reference(mut self, cfg: &Config, r: RefTx) -> Result<Self, String> {
        cfg.may_measure(r.lo(), r.hi())
            .map_err(|e| format!("reference {}: {e}", r.name))?;
        self.reference = Some(r);
        self.limits()?;
        Ok(self)
    }
}

pub fn khz(hz: u64) -> String {
    if hz >= 1_000_000 {
        mhz(hz)
    } else {
        format!("{} kHz", hz as f64 / 1000.0)
    }
}

/// "10" (minutes from now) or an RFC 3339 time.
pub fn parse_when(s: &str, now: i64) -> Result<i64, String> {
    let s = s.trim();
    if let Ok(m) = s.parse::<f64>() {
        if !(0.0..=MAX_AHEAD_MS as f64 / 60_000.0).contains(&m) {
            return Err("minutes from now must be 0 to 10080".into());
        }
        return Ok(now + (m * 60_000.0) as i64);
    }
    rfc3339_to_ms(s).ok_or_else(|| {
        format!("{s:?} is neither minutes from now nor a time like 2026-11-20T18:00:00Z")
    })
}

/// The `sdr.reading` payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Reading {
    pub capture: String,
    pub receiver: String,
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
    pub level_db: Option<f64>,
    #[serde(default)]
    pub peak_db: Option<f64>,
    #[serde(default)]
    pub floor_db: Option<f64>,
    #[serde(default)]
    pub snr_db: Option<f64>,
    #[serde(default)]
    pub bins: Option<u32>,
    /// None means automatic gain
    #[serde(default)]
    pub gain_db: Option<f64>,
    #[serde(default)]
    pub calibration_db: Option<f64>,
    #[serde(default)]
    pub unit: String,
}

fn sane(x: Option<f64>) -> bool {
    x.is_none_or(|v| v.is_finite() && v.abs() <= 150.0)
}

impl Reading {
    pub fn from_payload(v: &Value) -> Option<Self> {
        let r: Reading = serde_json::from_value(v.clone()).ok()?;
        let ok = r.capture.len() == 64
            && r.capture
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && valid_name(&r.receiver)
            && valid_node(&r.node)
            && valid_pos(r.lat, r.lon)
            && matches!(r.status.as_str(), "ok" | "refused" | "failed")
            && (r.status != "ok" || r.level_db.is_some())
            && clean(&r.detail, 300)
            && clean(&r.unit, 200)
            && r.bins.is_none_or(|b| (1..=100_000).contains(&b))
            && [
                r.level_db,
                r.peak_db,
                r.floor_db,
                r.snr_db,
                r.gain_db,
                r.calibration_db,
            ]
            .into_iter()
            .all(sane);
        if !ok {
            return None;
        }
        // worked out here from the parts, not taken on trust
        let mut r = r;
        r.snr_db = match (r.level_db, r.floor_db, r.bins) {
            (Some(l), Some(f), Some(b)) => Some(l - (f + 10.0 * (b as f64).log10())),
            _ => None,
        };
        Some(r)
    }

    pub fn dbm(&self) -> Option<f64> {
        Some(self.level_db? + self.calibration_db?)
    }
}

pub struct Captures;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS captures (
    id         TEXT PRIMARY KEY,
    author_key TEXT NOT NULL,
    call       TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    at_ms      INTEGER NOT NULL,
    json       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS captures_at ON captures(at_ms);
CREATE TABLE IF NOT EXISTS readings (
    capture    TEXT NOT NULL,
    author_key TEXT NOT NULL,
    call       TEXT NOT NULL,
    receiver   TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    json       TEXT NOT NULL,
    PRIMARY KEY (capture, author_key, receiver)
);";

impl Projection for Captures {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch("DELETE FROM captures; DELETE FROM readings;")?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        match e.kind.as_str() {
            "sdr.capture" => {
                // the start time is checked against when this site got it, not its author's clock
                let Some(r) =
                    Request::from_payload(&e.payload).filter(|r| r.at_ms() <= ts + MAX_AHEAD_MS)
                else {
                    return Ok(());
                };
                let busy: i64 = tx.query_row(
                    "SELECT count(*) FROM captures WHERE author_key = ?1 AND abs(at_ms - ?2) < 86400000",
                    params![e.author.key, r.at_ms()],
                    |row| row.get(0),
                )?;
                if busy < MAX_PER_STATION_DAY {
                    tx.execute(
                        "INSERT OR IGNORE INTO captures (id, author_key, call, ts, at_ms, json)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            e.id,
                            e.author.key,
                            e.author.call,
                            ts,
                            r.at_ms(),
                            serde_json::to_string(&r)?
                        ],
                    )?;
                }
            }
            "sdr.reading" => {
                if let Some(r) = Reading::from_payload(&e.payload) {
                    let (n, mine): (i64, i64) = tx.query_row(
                        "SELECT count(*), count(*) FILTER (WHERE author_key = ?2) FROM readings WHERE capture = ?1",
                        params![r.capture, e.author.key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                    if n >= MAX_READINGS || mine >= MAX_PER_STATION {
                        return Ok(());
                    }
                    // one reading per receiver per capture; a second one is a replay or a bug
                    tx.execute(
                        "INSERT OR IGNORE INTO readings (capture, author_key, call, receiver, ts, json)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![r.capture, e.author.key, e.author.call, r.receiver, ts, serde_json::to_string(&r)?],
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Cap {
    pub id: String,
    /// the requesting station's key
    pub key: String,
    pub call: String,
    pub ts: i64,
    pub req: Request,
}

fn caps(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Cap>> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map(p, |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = vec![];
    for row in rows {
        let (id, call, ts, json, key) = row?;
        if let Ok(req) = serde_json::from_str(&json) {
            out.push(Cap {
                id,
                key,
                call,
                ts,
                req,
            });
        }
    }
    Ok(out)
}

/// Captures starting between `from` and `to`, ms.
pub fn between(conn: &Connection, from: i64, to: i64) -> Result<Vec<Cap>> {
    caps(
        conn,
        "SELECT id, call, ts, json, author_key FROM captures WHERE at_ms >= ?1 AND at_ms < ?2
         ORDER BY at_ms LIMIT 500",
        params![from, to],
    )
}

/// Captures from the last 30 days and the next 7, newest start time first.
pub fn recent(conn: &Connection, now: i64) -> Result<Vec<Cap>> {
    caps(
        conn,
        "SELECT id, call, ts, json, author_key FROM captures WHERE at_ms > ?1 AND at_ms < ?2
         ORDER BY at_ms DESC LIMIT 200",
        params![now - directory::FORGET_MS, now + MAX_AHEAD_MS],
    )
}

/// A capture by its id or the first 8 or more characters of it.
pub fn find(conn: &Connection, prefix: &str) -> Result<Option<Cap>> {
    if prefix.len() < 8 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let found = caps(
        conn,
        "SELECT id, call, ts, json, author_key FROM captures WHERE substr(id, 1, ?2) = ?1 LIMIT 2",
        params![prefix.to_ascii_lowercase(), prefix.len() as i64],
    )?;
    Ok(if found.len() == 1 {
        found.into_iter().next()
    } else {
        None
    })
}

/// Captures starting within the next few seconds or started within the grace period,
/// earliest first, a few per requesting station.
pub fn due(conn: &Connection, now: i64) -> Result<Vec<Cap>> {
    let all = caps(
        conn,
        "SELECT id, call, ts, json, author_key FROM captures WHERE at_ms <= ?1 AND at_ms > ?2
         ORDER BY at_ms LIMIT 500",
        params![now + LOOKAHEAD_MS, now - GRACE_MS],
    )?;
    let mut per: std::collections::HashMap<String, usize> = Default::default();
    Ok(all
        .into_iter()
        .filter(|c| {
            let n = per.entry(c.key.clone()).or_default();
            *n += 1;
            *n <= MAX_DUE_PER_STATION
        })
        .take(MAX_DUE)
        .collect())
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: String,
    pub call: String,
    pub ts: i64,
    pub r: Reading,
}

pub fn readings(conn: &Connection, id: &str) -> Result<Vec<Row>> {
    let mut st =
        conn.prepare("SELECT author_key, call, ts, json FROM readings WHERE capture = ?1")?;
    let rows = st.query_map([id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let rows: Vec<(String, String, i64, String)> = rows.collect::<Result<_, _>>()?;
    let mut out = vec![];
    for (key, call, ts, json) in rows {
        let Ok(r) = serde_json::from_str::<Reading>(&json) else {
            continue;
        };
        // a station measures where its core's node is, whatever the reading says
        if station_node(conn, &key)?.is_none_or(|n| n == r.node) {
            out.push(Row { key, call, ts, r });
        }
    }
    Ok(out)
}

/// What the ranking compares. Receivers aren't calibrated against each other, so raw
/// levels only compare when every one has a measured calibration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scale {
    Dbm,
    AboveFloor,
    Relative,
}

impl Scale {
    pub fn unit(self) -> &'static str {
        match self {
            Scale::Dbm => "dBm",
            Scale::AboveFloor => "dB above floor",
            Scale::Relative => "dB, uncalibrated",
        }
    }

    pub fn explain(self) -> &'static str {
        match self {
            Scale::Dbm => {
                "Every receiver here has a calibration offset its site measured, so levels are \
                 shown as dBm at the antenna. They are only as good as those calibrations."
            }
            Scale::AboveFloor => {
                "These receivers aren't calibrated against each other, so they are ranked by how \
                 far the signal stands above each receiver's own noise floor. That cancels out \
                 gain settings but not antennas, cable loss or local noise: treat it as relative."
            }
            Scale::Relative => {
                "Not every reading has a noise floor or a calibration, so these are raw rtl_power \
                 levels, which depend on each receiver's gain. They only compare between \
                 receivers with the same gain, dongle and antenna."
            }
        }
    }

    fn value(self, r: &Reading) -> Option<f64> {
        match self {
            Scale::Dbm => r.dbm(),
            Scale::AboveFloor => r.snr_db,
            Scale::Relative => r.level_db,
        }
    }
}

pub struct Ranking {
    pub scale: Scale,
    /// loudest first
    pub ranked: Vec<(Row, f64)>,
    /// refused, failed, or without the number the scale needs
    pub other: Vec<Row>,
}

pub fn rank(rows: Vec<Row>) -> Ranking {
    let ok: Vec<&Row> = rows.iter().filter(|r| r.r.status == "ok").collect();
    // rtl_power and the IQ spectrum put dB on different scales: only heights above each
    // receiver's own floor compare across them
    let one_scale = ok.windows(2).all(|w| w[0].r.unit == w[1].r.unit);
    let scale = if !ok.is_empty() && one_scale && ok.iter().all(|r| r.r.dbm().is_some()) {
        Scale::Dbm
    } else if !one_scale || ok.iter().all(|r| r.r.snr_db.is_some()) {
        Scale::AboveFloor
    } else {
        Scale::Relative
    };
    let (mut ranked, mut other) = (vec![], vec![]);
    for row in rows {
        match scale.value(&row.r).filter(|_| row.r.status == "ok") {
            Some(v) => ranked.push((row, v)),
            None => other.push(row),
        }
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    other.sort_by(|a, b| (&a.r.node, &a.r.receiver).cmp(&(&b.r.node, &b.r.receiver)));
    Ranking {
        scale,
        ranked,
        other,
    }
}

/// Turn rtl_power's output into a reading's numbers, or say why it can't be.
pub fn measure_csv(cfg: &Config, req: &Request, csv: &str) -> Result<rtlpower::Level, String> {
    let bins = rtlpower::parse(csv).map_err(|e| format!("{e:#}"))?;
    rtlpower::level(&bins, req.lo(), req.hi(), |f| cfg.may_measure_at(f)).ok_or_else(|| {
        if bins.is_empty() {
            "rtl_power printed no measurements".into()
        } else {
            "rtl_power gave no bins inside the requested band".into()
        }
    })
}

/// The last time a local receiver measured something.
pub fn last_capture(conn: &Connection, receiver: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT max(started_ms) FROM runs WHERE receiver = ?1 AND ok = 1",
            [receiver],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;
    use crate::testing::{ev, init};
    use serde_json::json;

    const NOW: i64 = 1_795_000_000_000;

    fn cfg() -> Config {
        toml::from_str(EXAMPLE).unwrap()
    }

    #[test]
    fn requests_must_be_measurable_here_and_sensible() {
        let c = cfg();
        let r = Request::new(&c, 146_520_000, 16_000, 10, NOW + 600_000, " fox ", NOW).unwrap();
        assert_eq!((r.lo(), r.hi()), (146_512_000, 146_528_000));
        assert_eq!(r.note, "fox");
        assert_eq!(r.at_ms(), NOW + 600_000);
        let e = Request::new(&c, 162_550_000, 25_000, 10, NOW, "", NOW).unwrap_err();
        assert!(e.contains("not inside an amateur band"), "{e}");
        let e = Request::new(&c, 147_999_000, 16_000, 10, NOW, "", NOW).unwrap_err();
        assert!(e.contains("not inside"), "runs off the top of 2 m: {e}");
        assert!(Request::new(&c, 146_520_000, 500, 10, NOW, "", NOW).is_err());
        assert!(Request::new(&c, 146_520_000, 16_000, 301, NOW, "", NOW).is_err());
        assert!(Request::new(&c, 146_520_000, 16_000, 10, NOW - 60_000, "", NOW).is_err());
        assert!(Request::new(&c, 146_520_000, 16_000, 10, NOW + 8 * 86_400_000, "", NOW).is_err());
        let mut local = cfg();
        local.capture.local_bands = vec![[162.4, 162.56]];
        assert!(Request::new(&local, 162_550_000, 15_000, 10, NOW, "", NOW).is_ok());
    }

    #[test]
    fn when_is_minutes_or_a_time() {
        assert_eq!(parse_when("10", NOW).unwrap(), NOW + 600_000);
        assert_eq!(parse_when("0", NOW).unwrap(), NOW);
        assert_eq!(
            parse_when("2026-11-20T18:00:00Z", NOW).unwrap(),
            rfc3339_to_ms("2026-11-20T18:00:00Z").unwrap()
        );
        assert!(parse_when("soon", NOW).is_err());
        assert!(parse_when("-5", NOW).is_err());
    }

    fn reading(id: &str, rx: &str, level: f64, snr: Option<f64>, cal: Option<f64>) -> Value {
        json!({"capture": id, "receiver": rx, "node": format!("tst-{rx}"), "status": "ok",
               "level_db": level, "floor_db": snr.map(|s| level - s), "bins": 1,
               "calibration_db": cal, "lat": 35.3, "lon": -120.6})
    }

    fn capture(c: &mut Connection, at: i64) -> String {
        let req =
            json!({"freq": 146_520_000u64, "bw": 16_000u64, "secs": 10, "at": ms_to_rfc3339(at)});
        ev(c, "aa", "sdr.capture", req, NOW).id
    }

    #[test]
    fn uncalibrated_receivers_rank_by_height_above_their_floor() {
        let mut c = init();
        let id = capture(&mut c, NOW);
        // cuesta has its gain turned up, so its raw level is highest, but it hears less
        ev(
            &mut c,
            "aa",
            "sdr.reading",
            reading(&id, "cuesta", -10.0, Some(8.0), None),
            NOW,
        );
        ev(
            &mut c,
            "bb",
            "sdr.reading",
            reading(&id, "north", -25.0, Some(21.0), Some(-90.0)),
            NOW,
        );
        ev(
            &mut c,
            "cc",
            "sdr.reading",
            reading(&id, "paso", -30.0, Some(3.0), None),
            NOW,
        );
        ev(
            &mut c,
            "dd",
            "sdr.reading",
            json!({"capture": id, "receiver": "lopez", "node": "tst-lopez", "status": "refused",
                  "detail": "not amateur here"}),
            NOW,
        );
        let r = rank(readings(&c, &id).unwrap());
        assert_eq!(r.scale, Scale::AboveFloor);
        let order: Vec<&str> = r
            .ranked
            .iter()
            .map(|(x, _)| x.r.receiver.as_str())
            .collect();
        assert_eq!(order, ["north", "cuesta", "paso"]);
        assert_eq!(r.ranked[0].1, 21.0);
        assert_eq!(r.other.len(), 1);
        assert_eq!(r.other[0].r.status, "refused");
    }

    #[test]
    fn calibrated_receivers_rank_by_dbm() {
        let mut c = init();
        let id = capture(&mut c, NOW);
        ev(
            &mut c,
            "aa",
            "sdr.reading",
            reading(&id, "cuesta", -10.0, Some(8.0), Some(-100.0)),
            NOW,
        );
        ev(
            &mut c,
            "bb",
            "sdr.reading",
            reading(&id, "north", -25.0, Some(21.0), Some(-80.0)),
            NOW,
        );
        let r = rank(readings(&c, &id).unwrap());
        assert_eq!(r.scale, Scale::Dbm);
        assert_eq!(r.ranked[0].0.r.receiver, "north");
        assert_eq!(r.ranked[0].1, -105.0);
        assert_eq!(r.ranked[1].1, -110.0);
    }

    #[test]
    fn without_floors_raw_levels_are_used_and_labelled_uncalibrated() {
        let mut c = init();
        let id = capture(&mut c, NOW);
        ev(
            &mut c,
            "aa",
            "sdr.reading",
            reading(&id, "cuesta", -10.0, None, None),
            NOW,
        );
        ev(
            &mut c,
            "bb",
            "sdr.reading",
            reading(&id, "north", -25.0, Some(21.0), None),
            NOW,
        );
        let r = rank(readings(&c, &id).unwrap());
        assert_eq!(r.scale, Scale::Relative);
        assert_eq!(r.ranked[0].0.r.receiver, "cuesta");
        assert!(r.scale.unit().contains("uncalibrated"));
    }

    #[test]
    fn due_captures_and_bad_events() {
        let mut c = init();
        let id = capture(&mut c, NOW);
        assert!(
            due(&c, NOW - LOOKAHEAD_MS - 1).unwrap().is_empty(),
            "not yet"
        );
        assert_eq!(
            due(&c, NOW - 5_000).unwrap().len(),
            1,
            "picked up a few seconds early"
        );
        assert_eq!(due(&c, NOW + 30_000).unwrap()[0].id, id);
        assert!(due(&c, NOW + GRACE_MS + 1).unwrap().is_empty(), "too late");
        assert_eq!(find(&c, &id[..8]).unwrap().unwrap().id, id);
        assert!(find(&c, &id[..4]).unwrap().is_none());

        let bad = [
            json!({"freq": 146_520_000u64, "bw": 16_000u64, "secs": 9999, "at": ms_to_rfc3339(NOW)}),
            json!({"freq": 146_520_000u64, "bw": 16_000u64, "secs": 10, "at": "tomorrow"}),
            json!({"freq": 1u64, "bw": 16_000u64, "secs": 10, "at": ms_to_rfc3339(NOW)}),
        ];
        for b in bad {
            ev(&mut c, "aa", "sdr.capture", b, NOW);
        }
        assert_eq!(recent(&c, NOW).unwrap().len(), 1);

        let mut r = reading(&id, "cuesta", -10.0, Some(8.0), None);
        r["node"] = json!("<b>");
        ev(&mut c, "aa", "sdr.reading", r, NOW);
        let mut r = reading(&id, "north", -10.0, Some(8.0), None);
        r["level_db"] = json!(1e300);
        ev(&mut c, "aa", "sdr.reading", r, NOW);
        let mut r = reading(&id, "paso", -10.0, None, None);
        r["status"] = json!("ok");
        r.as_object_mut().unwrap().remove("level_db");
        ev(&mut c, "aa", "sdr.reading", r, NOW);
        assert!(readings(&c, &id).unwrap().is_empty());

        ev(
            &mut c,
            "aa",
            "sdr.reading",
            reading(&id, "cuesta", -10.0, Some(8.0), None),
            NOW,
        );
        ev(
            &mut c,
            "aa",
            "sdr.reading",
            reading(&id, "cuesta", 50.0, Some(80.0), None),
            NOW + 1,
        );
        let rs = readings(&c, &id).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(
            rs[0].r.level_db,
            Some(-10.0),
            "first reading per receiver stands"
        );
    }

    #[test]
    fn rtl_power_output_becomes_a_reading_within_allowed_bands_only() {
        let c = cfg();
        let req = Request::new(&c, 146_520_000, 16_000, 10, NOW, "", NOW).unwrap();
        let p = rtlpower::plan(req.freq, req.bw);
        let sig = "146.52M:-20:8k".parse().unwrap();
        let csv =
            rtlpower::simulate(&format!("{}:{}:{}", p.lo, p.hi, p.bin), -45.0, &[sig]).unwrap();
        let l = measure_csv(&c, &req, &csv).unwrap();
        assert!(l.snr_db.unwrap() > 15.0, "{l:?}");
        assert_eq!(
            measure_csv(&c, &req, "").unwrap_err(),
            "rtl_power printed no measurements"
        );
        // floor bins only come from spectrum this site may measure
        assert!(c.may_measure_at(144_030_000.0));
        assert!(!c.may_measure_at(143_990_000.0));
    }

    fn tx(freq: u64) -> RefTx {
        RefTx {
            name: "TST repeater".into(),
            freq,
            bw: 16_000,
            lat: 35.3,
            lon: -120.6,
        }
    }

    #[test]
    fn a_reference_has_to_share_the_recording_and_be_measurable() {
        let c = cfg();
        let r = Request::new(&c, 146_520_000, 16_000, 10, NOW, "", NOW).unwrap();
        assert!(r.clone().with_reference(&c, tx(147_000_000)).is_ok());
        assert!(
            r.clone().with_reference(&c, tx(146_540_000)).is_err(),
            "too close"
        );
        assert!(
            r.clone().with_reference(&c, tx(149_000_000)).is_err(),
            "too far, and not amateur"
        );
        let e = r.with_reference(&c, tx(162_550_000)).unwrap_err();
        assert!(e.contains("reference TST repeater"), "{e}");
        assert!(
            Request::new(&c, u64::MAX - 5, 16_000, 10, NOW, "", NOW).is_err(),
            "no wrap-around"
        );
    }

    #[test]
    fn far_future_requests_and_forged_numbers_are_refused() {
        let mut c = init();
        let far = json!({"freq": 146_520_000u64, "bw": 16_000u64, "secs": 10, "at": ms_to_rfc3339(NOW + 8 * 86_400_000)});
        ev(&mut c, "aa", "sdr.capture", far, NOW);
        assert!(recent(&c, NOW).unwrap().is_empty());
        let id = capture(&mut c, NOW);
        let mut r = reading(&id, "cuesta", -10.0, Some(8.0), None);
        r["snr_db"] = json!(99.0);
        ev(&mut c, "aa", "sdr.reading", r, NOW);
        let rs = readings(&c, &id).unwrap();
        assert_eq!(rs[0].r.snr_db, Some(8.0), "worked out from level and floor");
        let mut r = reading(&id, "north", -10.0, Some(8.0), None);
        r["detail"] = json!("\u{1b}]0;owned\u{7}");
        ev(&mut c, "aa", "sdr.reading", r, NOW);
        let mut r = reading(&id, "paso", -10.0, Some(8.0), None);
        r["level_db"] = json!(-151.0);
        ev(&mut c, "aa", "sdr.reading", r, NOW);
        assert_eq!(readings(&c, &id).unwrap().len(), 1);
    }

    #[test]
    fn a_flood_of_requests_is_capped_per_station() {
        let mut c = init();
        for i in 0..10 {
            let req = json!({"freq": 146_520_000u64 + i * 20_000, "bw": 16_000u64, "secs": 10, "at": ms_to_rfc3339(NOW)});
            ev(&mut c, "aa", "sdr.capture", req, NOW);
        }
        let req =
            json!({"freq": 145_000_000u64, "bw": 16_000u64, "secs": 10, "at": ms_to_rfc3339(NOW)});
        ev(&mut c, "bb", "sdr.capture", req, NOW);
        let d = due(&c, NOW).unwrap();
        assert_eq!(
            d.iter().filter(|x| x.key == "aa").count(),
            MAX_DUE_PER_STATION
        );
        assert_eq!(
            d.iter().filter(|x| x.key == "bb").count(),
            1,
            "others still get through"
        );
    }

    #[test]
    fn readings_on_different_scales_rank_by_height_above_floor() {
        let mut c = init();
        let id = capture(&mut c, NOW);
        let mut iq = reading(&id, "cuesta", -10.0, Some(8.0), Some(-90.0));
        iq["unit"] = json!(UNIT_IQ);
        ev(&mut c, "aa", "sdr.reading", iq, NOW);
        let mut rp = reading(&id, "north", -25.0, Some(21.0), Some(-80.0));
        rp["unit"] = json!(UNIT);
        ev(&mut c, "bb", "sdr.reading", rp, NOW);
        let r = rank(readings(&c, &id).unwrap());
        assert_eq!(
            r.scale,
            Scale::AboveFloor,
            "dBm can't compare across scales"
        );
        assert_eq!(r.ranked[0].0.r.receiver, "north");
    }
}
