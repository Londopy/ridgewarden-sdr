//! The receiver directory. Each site publishes a signed `sdr.receiver` event per receiver
//! when its description changes (and daily), plus `sdr.status` telemetry with live status.
//! Every site builds the same directory from the feed, so it works with the internet down.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::util::{norm_node, valid_node};

use crate::bands::offer_band;
use crate::config::{ReceiverCfg, valid_name};
use crate::owrx;

/// Receivers not heard from in this long drop off the directory.
pub const FORGET_MS: i64 = 30 * 86_400_000;
const MAX_PROFILES: usize = 64;
/// One station can't fill the directory: a site with more SDRs than this is unlikely.
pub const MAX_PER_STATION: i64 = 16;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RfProfile {
    pub name: String,
    pub lo: u64,
    pub hi: u64,
}

/// The `sdr.receiver` payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Receiver {
    pub name: String,
    pub node: String,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    /// "config" or "node": where the position came from
    #[serde(default)]
    pub location: String,
    pub min_hz: u64,
    pub max_hz: u64,
    #[serde(default)]
    pub antenna: String,
    /// the mesh-facing OpenWebRX+ instance
    #[serde(default)]
    pub url: Option<String>,
    /// why the site's web link isn't published
    #[serde(default)]
    pub withheld: Option<String>,
    /// what that instance offers over RF
    #[serde(default)]
    pub profiles: Vec<RfProfile>,
    #[serde(default)]
    pub capture: bool,
    #[serde(default)]
    pub calibrated: bool,
}

/// Text from the mesh that is short enough and has no control characters, which could
/// otherwise rewrite a terminal when the CLI prints it.
pub fn clean(s: &str, max: usize) -> bool {
    s.chars().count() <= max && !s.chars().any(char::is_control)
}

pub fn valid_pos(lat: Option<f64>, lon: Option<f64>) -> bool {
    match (lat, lon) {
        (Some(a), Some(o)) => (-90.0..=90.0).contains(&a) && (-180.0..=180.0).contains(&o),
        (None, None) => true,
        _ => false,
    }
}

impl Receiver {
    /// Read a payload from the feed, refusing anything malformed or oversized.
    pub fn from_payload(v: &Value) -> Option<Self> {
        let r: Receiver = serde_json::from_value(v.clone()).ok()?;
        let ok = valid_name(&r.name)
            && valid_node(&r.node)
            && valid_pos(r.lat, r.lon)
            && clean(&r.location, 10)
            && r.min_hz < r.max_hz
            && clean(&r.antenna, 100)
            && r.url
                .as_deref()
                .is_none_or(|u| u.starts_with("http://") && clean(u, 200))
            && r.withheld.as_deref().is_none_or(|w| clean(w, 400))
            && r.profiles.len() <= MAX_PROFILES
            && r.profiles.iter().all(|p| clean(&p.name, 64) && p.lo < p.hi);
        ok.then_some(r)
    }

    /// Profiles a site says it offers over RF that aren't inside one amateur band it may
    /// offer, as checked here: nobody else's word is taken for it.
    pub fn off_band(&self) -> Vec<&RfProfile> {
        self.profiles
            .iter()
            .filter(|p| offer_band(p.lo, p.hi).is_none())
            .collect()
    }

    /// The web link, only if this site's own check passes: nothing withheld, at least
    /// one profile listed, and every profile inside the amateur bands.
    pub fn link(&self) -> Option<&str> {
        let ok = self.withheld.is_none() && !self.profiles.is_empty() && self.off_band().is_empty();
        self.url.as_deref().filter(|_| ok)
    }

    pub fn covers(&self, lo: u64, hi: u64) -> bool {
        self.min_hz <= lo && hi <= self.max_hz
    }
}

/// The `sdr.status` payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Status {
    pub name: String,
    /// whether the web UI answered; None when there is none
    #[serde(default)]
    pub web: Option<bool>,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub last_capture: Option<i64>,
}

pub struct Directory;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS receivers (
    author_key TEXT NOT NULL,
    name       TEXT NOT NULL,
    call       TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    json       TEXT NOT NULL,
    PRIMARY KEY (author_key, name)
);
CREATE TABLE IF NOT EXISTS rx_status (
    author_key   TEXT NOT NULL,
    name         TEXT NOT NULL,
    ts           INTEGER NOT NULL,
    web          INTEGER,
    detail       TEXT NOT NULL,
    last_capture INTEGER,
    PRIMARY KEY (author_key, name)
);
CREATE TABLE IF NOT EXISTS positions (
    node TEXT PRIMARY KEY,
    lat  REAL NOT NULL,
    lon  REAL NOT NULL,
    ts   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS stations (
    author_key TEXT PRIMARY KEY,
    node       TEXT NOT NULL,
    lat        REAL,
    lon        REAL,
    ts         INTEGER NOT NULL
);";

impl Projection for Directory {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch(
            "DELETE FROM receivers; DELETE FROM rx_status; DELETE FROM positions; DELETE FROM stations;",
        )?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        match e.kind.as_str() {
            "sdr.receiver" => {
                let Some(r) = Receiver::from_payload(&e.payload) else {
                    return Ok(());
                };
                let (known, count): (i64, i64) = tx.query_row(
                    "SELECT count(*) FILTER (WHERE name = ?2), count(*) FROM receivers WHERE author_key = ?1",
                    params![e.author.key, r.name],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if known == 0 && count >= MAX_PER_STATION {
                    return Ok(());
                }
                tx.execute(
                    "INSERT INTO receivers (author_key, name, call, ts, json) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(author_key, name) DO UPDATE SET call = excluded.call, ts = excluded.ts,
                         json = excluded.json WHERE excluded.ts >= receivers.ts",
                    params![e.author.key, r.name, e.author.call, ts, serde_json::to_string(&r)?],
                )?;
            }
            "sdr.status" => {
                let Ok(s) = serde_json::from_value::<Status>(e.payload.clone()) else {
                    return Ok(());
                };
                if !valid_name(&s.name) || !clean(&s.detail, 200) {
                    return Ok(());
                }
                let detail = s.detail;
                tx.execute(
                    "INSERT INTO rx_status (author_key, name, ts, web, detail, last_capture)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(author_key, name) DO UPDATE SET ts = excluded.ts, web = excluded.web,
                         detail = excluded.detail, last_capture = excluded.last_capture
                     WHERE excluded.ts >= rx_status.ts",
                    params![e.author.key, s.name, ts, s.web, detail, s.last_capture],
                )?;
            }
            "node.poll" => note_poll(tx, &e.author.key, &e.payload, ts)?,
            _ => {}
        }
        Ok(())
    }
}

/// Keep each node's last position, and which node each station polls on its own LAN,
/// from the core's polls. Only this, not the polls themselves, so pages stay fast.
fn note_poll(tx: &Transaction, key: &str, p: &Value, ts: i64) -> Result<()> {
    let node = norm_node(p.get("node").and_then(Value::as_str).unwrap_or(""));
    if !valid_node(&node) {
        return Ok(());
    }
    let (lat, lon) = (
        p.get("lat").and_then(Value::as_f64),
        p.get("lon").and_then(Value::as_f64),
    );
    let (lat, lon) = if valid_pos(lat, lon) {
        (lat, lon)
    } else {
        (None, None)
    };
    if let (Some(a), Some(o)) = (lat, lon) {
        tx.execute(
            "INSERT INTO positions (node, lat, lon, ts) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(node) DO UPDATE SET lat = excluded.lat, lon = excluded.lon, ts = excluded.ts
             WHERE excluded.ts >= positions.ts",
            params![node, a, o, ts],
        )?;
    }
    if p.get("via").and_then(Value::as_str).unwrap_or("lan") == "lan" {
        // a station's own position comes only from its own polls of its own node
        tx.execute(
            "INSERT INTO stations (author_key, node, lat, lon, ts) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(author_key) DO UPDATE SET node = excluded.node,
                 lat = coalesce(excluded.lat, CASE WHEN stations.node = excluded.node THEN stations.lat END),
                 lon = coalesce(excluded.lon, CASE WHEN stations.node = excluded.node THEN stations.lon END),
                 ts = excluded.ts
             WHERE excluded.ts >= stations.ts",
            params![key, node, lat, lon, ts],
        )?;
    }
    Ok(())
}

/// The node a station's core polls on its own LAN, if it has polled one.
pub fn station_node(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT node FROM stations WHERE author_key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?)
}

#[derive(Clone, Debug)]
pub struct Listed {
    pub key: String,
    pub call: String,
    /// when its description was last published
    pub ts: i64,
    pub rx: Receiver,
    pub status: Option<StatusRow>,
}

#[derive(Clone, Debug)]
pub struct StatusRow {
    pub ts: i64,
    pub web: Option<bool>,
    pub detail: String,
    pub last_capture: Option<i64>,
}

impl Listed {
    pub fn heard(&self) -> i64 {
        self.status.as_ref().map_or(self.ts, |s| s.ts.max(self.ts))
    }
}

/// Every receiver heard from in the last 30 days, by site then name.
pub fn list(conn: &Connection, now: i64) -> Result<Vec<Listed>> {
    // a station's receivers are where its core's own node is, whatever they claim
    let mut st = conn.prepare(
        "SELECT r.author_key, r.call, r.ts, r.json, s.ts, s.web, s.detail, s.last_capture, st.node
         FROM receivers r LEFT JOIN rx_status s ON s.author_key = r.author_key AND s.name = r.name
         LEFT JOIN stations st ON st.author_key = r.author_key
         WHERE max(r.ts, coalesce(s.ts, 0)) > ?1",
    )?;
    let rows = st.query_map(params![now - FORGET_MS], |r| {
        let status = match r.get::<_, Option<i64>>(4)? {
            Some(ts) => Some(StatusRow {
                ts,
                web: r.get(5)?,
                detail: r.get(6)?,
                last_capture: r.get(7)?,
            }),
            None => None,
        };
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            status,
            r.get::<_, Option<String>>(8)?,
        ))
    })?;
    let mut out = vec![];
    for row in rows {
        let (key, call, ts, json, status, polled) = row?;
        let Ok(rx) = serde_json::from_str::<Receiver>(&json) else {
            continue;
        };
        if polled.is_none_or(|n| n == rx.node) {
            out.push(Listed {
                key,
                call,
                ts,
                rx,
                status,
            });
        }
    }
    out.sort_by(|a, b| (&a.rx.node, &a.rx.name).cmp(&(&b.rx.node, &b.rx.name)));
    Ok(out)
}

/// Where a station's own node says it is, from that station's own polls.
pub fn home(conn: &Connection, key: &str) -> Result<Option<(f64, f64)>> {
    let row: Option<(Option<f64>, Option<f64>)> = conn
        .query_row(
            "SELECT lat, lon FROM stations WHERE author_key = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.and_then(|(a, o)| a.zip(o)))
}

/// Every node with a position reported in the last 30 days, for context on the map.
pub fn node_positions(conn: &Connection, now: i64) -> Result<Vec<(String, f64, f64)>> {
    let mut st =
        conn.prepare("SELECT node, lat, lon FROM positions WHERE ts > ?1 ORDER BY node")?;
    let rows = st.query_map([now - FORGET_MS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Where a receiver is: its configured position, else its node's.
pub fn position(
    rx: &ReceiverCfg,
    node_pos: Option<(f64, f64)>,
) -> (Option<f64>, Option<f64>, &'static str) {
    match (rx.lat, rx.lon, node_pos) {
        (Some(a), Some(o), _) => (Some(a), Some(o), "config"),
        (_, _, Some((a, o))) => (Some(a), Some(o), "node"),
        _ => (None, None, ""),
    }
}

/// The web link and profiles to publish for the mesh-facing OpenWebRX+ instance, or why
/// the link is withheld.
pub fn rf_offer(rx: &ReceiverCfg) -> (Option<String>, Vec<RfProfile>, Option<String>) {
    let (Some(url), Some(path)) = (&rx.openwebrx_url, &rx.openwebrx_settings) else {
        return (None, vec![], None);
    };
    let checked = owrx::load(path).and_then(|s| Ok((owrx::profiles(&s)?, s)));
    let (profiles, settings) = match checked {
        Ok(x) => x,
        Err(e) => {
            let why = format!("its settings could not be checked: {e:#}");
            return (None, vec![], Some(why.chars().take(300).collect()));
        }
    };
    if let Some((p, why)) = profiles.iter().find_map(|p| Some((p, p.problem()?))) {
        let why = format!(
            "profile {:?} {why}; run check-owrx --write to make an amateur-only copy",
            p.name
        );
        return (None, vec![], Some(why.chars().take(300).collect()));
    }
    if owrx::center_changes_allowed(&settings) {
        return (
            None,
            vec![],
            Some(format!(
                "{} is on, so listeners could tune off the band",
                owrx::CENTER_CHANGES
            )),
        );
    }
    let offered = profiles
        .iter()
        .take(MAX_PROFILES)
        .filter_map(|p| {
            let (lo, hi) = p.span()?;
            Some(RfProfile {
                name: p.name.chars().take(64).collect(),
                lo,
                hi,
            })
        })
        .collect();
    (Some(url.clone()), offered, None)
}

pub fn describe(rx: &ReceiverCfg, node: &str, node_pos: Option<(f64, f64)>) -> Receiver {
    let (lat, lon, location) = position(rx, node_pos);
    let (url, profiles, withheld) = rf_offer(rx);
    Receiver {
        name: rx.name.clone(),
        node: node.to_string(),
        lat,
        lon,
        location: location.into(),
        min_hz: rx.min_hz(),
        max_hz: rx.max_hz(),
        antenna: rx.antenna.clone(),
        url,
        withheld,
        profiles,
        capture: rx.capture.is_some(),
        calibrated: rx
            .capture
            .as_ref()
            .is_some_and(|c| c.calibration_db.is_some()),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testing::{ev, init};
    use serde_json::json;

    fn rx_cfg(url: bool, settings: Option<std::path::PathBuf>) -> ReceiverCfg {
        ReceiverCfg {
            name: "rx1".into(),
            antenna: "discone".into(),
            min_mhz: 24.0,
            max_mhz: 1766.0,
            lat: None,
            lon: None,
            openwebrx_url: url.then(|| "http://tst-pi.local.mesh:8074/".into()),
            openwebrx_settings: settings,
            capture: None,
        }
    }

    fn write_settings(dir: &tempfile::TempDir, v: &Value) -> std::path::PathBuf {
        let p = dir.path().join("settings.json");
        std::fs::write(&p, serde_json::to_string(v).unwrap()).unwrap();
        p
    }

    #[test]
    fn web_link_is_withheld_until_every_rf_profile_is_amateur() {
        let dir = tempfile::tempdir().unwrap();
        let raw = crate::owrx::tests::sample();
        let p = write_settings(&dir, &raw);
        let r = describe(&rx_cfg(true, Some(p)), "tst-cuesta", Some((35.33, -120.62)));
        assert_eq!(r.url, None);
        assert!(
            r.withheld
                .as_deref()
                .unwrap()
                .contains("not inside one amateur band")
        );
        assert_eq!(
            (r.lat, r.lon, r.location.as_str()),
            (Some(35.33), Some(-120.62), "node")
        );

        let (filtered, _) = crate::owrx::filtered(&raw, true).unwrap();
        let p = write_settings(&dir, &filtered);
        let r = describe(&rx_cfg(true, Some(p)), "tst-cuesta", None);
        assert_eq!(r.url.as_deref(), Some("http://tst-pi.local.mesh:8074/"));
        assert_eq!(r.withheld, None);
        assert_eq!(r.profiles.len(), 2);
        assert!(r.off_band().is_empty());

        let mut open = filtered.clone();
        open["allow_center_freq_changes"] = json!(true);
        let p = write_settings(&dir, &open);
        let r = describe(&rx_cfg(true, Some(p)), "tst-cuesta", None);
        assert!(r.withheld.unwrap().contains("allow_center_freq_changes"));

        let r = describe(
            &rx_cfg(true, Some(dir.path().join("missing.json"))),
            "tst-cuesta",
            None,
        );
        assert!(r.withheld.unwrap().contains("could not be checked"));
        assert_eq!(
            describe(&rx_cfg(false, None), "tst-cuesta", None).withheld,
            None
        );
    }

    fn payload(name: &str) -> Value {
        json!({"name": name, "node": "tst-cuesta", "lat": 35.33, "lon": -120.62,
               "location": "node", "min_hz": 24_000_000u64, "max_hz": 1_766_000_000u64,
               "antenna": "discone", "url": "http://tst-pi.local.mesh:8074/",
               "profiles": [{"name": "2m", "lo": 145_000_000u64, "hi": 147_000_000u64}],
               "capture": true, "calibrated": false})
    }

    #[test]
    fn directory_keeps_the_latest_description_and_status() {
        let mut c = init();
        let k = "aa";
        ev(&mut c, k, "sdr.receiver", payload("rx1"), 1000);
        let mut newer = payload("rx1");
        newer["antenna"] = json!("yagi");
        ev(&mut c, k, "sdr.receiver", newer, 3000);
        ev(&mut c, k, "sdr.receiver", payload("rx1"), 2000);
        ev(&mut c, "bb", "sdr.receiver", payload("rx1"), 2000);
        ev(
            &mut c,
            k,
            "sdr.status",
            json!({"name": "rx1", "web": false, "detail": "could not connect"}),
            4000,
        );
        let l = list(&c, 5000).unwrap();
        assert_eq!(
            l.len(),
            2,
            "same name from another station is another receiver"
        );
        let mine = l.iter().find(|x| x.key == k).unwrap();
        assert_eq!(
            mine.rx.antenna, "yagi",
            "an older event arriving late doesn't win"
        );
        let s = mine.status.as_ref().unwrap();
        assert_eq!(
            (s.web, s.detail.as_str()),
            (Some(false), "could not connect")
        );
        assert_eq!(mine.heard(), 4000);
        assert!(list(&c, 4000 + FORGET_MS + 1).unwrap().is_empty());
    }

    #[test]
    fn hostile_descriptions_are_ignored() {
        let mut c = init();
        let mut bad = vec![];
        for (k, v) in [
            ("node", json!("<script>")),
            ("name", json!("RX ONE")),
            ("lat", json!(95.0)),
            ("url", json!("javascript:alert(1)")),
            ("antenna", json!("x".repeat(500))),
            ("max_hz", json!(1)),
        ] {
            let mut p = payload("rx1");
            p[k] = v;
            bad.push(p);
        }
        let mut lone = payload("rx1");
        lone.as_object_mut().unwrap().remove("lon");
        bad.push(lone);
        for p in bad {
            ev(&mut c, "aa", "sdr.receiver", p, 1000);
        }
        ev(&mut c, "aa", "sdr.receiver", json!("not an object"), 1000);
        assert!(list(&c, 2000).unwrap().is_empty());
    }

    #[test]
    fn profiles_claimed_amateur_are_checked_here() {
        let mut p = payload("rx1");
        p["profiles"] = json!([{"name": "wx", "lo": 162_000_000u64, "hi": 163_000_000u64}]);
        let r = Receiver::from_payload(&p).unwrap();
        assert_eq!(r.off_band().len(), 1);
    }

    #[test]
    fn only_a_link_that_passes_this_sites_check_is_shown() {
        let ok = Receiver::from_payload(&payload("rx1")).unwrap();
        assert_eq!(ok.link(), Some("http://tst-pi.local.mesh:8074/"));
        let mut off = payload("rx1");
        off["profiles"] = json!([{"name": "wx", "lo": 162_000_000u64, "hi": 163_000_000u64}]);
        assert_eq!(Receiver::from_payload(&off).unwrap().link(), None);
        let mut none = payload("rx1");
        none["profiles"] = json!([]);
        assert_eq!(
            Receiver::from_payload(&none).unwrap().link(),
            None,
            "nothing checked"
        );
        let mut held = payload("rx1");
        held["withheld"] = json!("its settings could not be checked");
        assert_eq!(Receiver::from_payload(&held).unwrap().link(), None);
        let mut fixed = payload("rx1");
        fixed["profiles"] = json!([{"name": "220", "lo": 219_100_000u64, "hi": 219_900_000u64}]);
        assert_eq!(
            Receiver::from_payload(&fixed).unwrap().link(),
            None,
            "97.303(l)"
        );
    }

    #[test]
    fn receivers_are_pinned_to_their_stations_own_node() {
        let mut c = init();
        ev(
            &mut c,
            "aa",
            "node.poll",
            json!({"node": "TST-NORTH.local.mesh", "via": "lan", "lat": 35.52, "lon": -120.68}),
            500,
        );
        // a crawl poll moves nobody's home, but does give the node a position
        ev(
            &mut c,
            "aa",
            "node.poll",
            json!({"node": "tst-paso", "via": "crawl", "lat": 35.63, "lon": -120.69}),
            600,
        );
        ev(&mut c, "aa", "sdr.receiver", payload("rx1"), 1000);
        assert!(
            list(&c, 2000).unwrap().is_empty(),
            "claims tst-cuesta, polls tst-north"
        );
        let mut honest = payload("rx2");
        honest["node"] = json!("tst-north");
        ev(&mut c, "aa", "sdr.receiver", honest, 1000);
        assert_eq!(list(&c, 2000).unwrap().len(), 1);
        assert_eq!(
            station_node(&c, "aa").unwrap().as_deref(),
            Some("tst-north")
        );
        assert_eq!(home(&c, "aa").unwrap(), Some((35.52, -120.68)));
        // another station's crawl can't move this station's home
        ev(
            &mut c,
            "bb",
            "node.poll",
            json!({"node": "tst-north", "via": "crawl", "lat": 1.0, "lon": 1.0}),
            700,
        );
        assert_eq!(home(&c, "aa").unwrap(), Some((35.52, -120.68)));
        assert_eq!(node_positions(&c, 2000).unwrap().len(), 2);
    }

    #[test]
    fn one_station_cannot_fill_the_directory() {
        let mut c = init();
        for i in 0..MAX_PER_STATION + 5 {
            ev(
                &mut c,
                "aa",
                "sdr.receiver",
                payload(&format!("rx{i}")),
                1000 + i,
            );
        }
        assert_eq!(list(&c, 5000).unwrap().len() as i64, MAX_PER_STATION);
        // an existing receiver can still be updated
        let mut newer = payload("rx0");
        newer["antenna"] = json!("yagi");
        ev(&mut c, "aa", "sdr.receiver", newer, 3000);
        assert!(
            list(&c, 5000)
                .unwrap()
                .iter()
                .any(|l| l.rx.antenna == "yagi")
        );
    }
}
