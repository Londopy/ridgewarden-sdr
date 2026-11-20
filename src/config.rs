//! `sdr.toml`.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::bands::{band_of, mhz};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub core: CoreCfg,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// how often each receiver's live status is published
    #[serde(default = "d_300")]
    pub status_interval_s: u64,
    #[serde(default)]
    pub capture: CaptureCfg,
    /// friendly site names: node name -> label, e.g. "tst-cuesta" = "Cuesta Peak"
    #[serde(default)]
    pub site_names: BTreeMap<String, String>,
    #[serde(default, rename = "receiver")]
    pub receivers: Vec<ReceiverCfg>,
    /// KrakenSDRs at this site, for bearings
    #[serde(default, rename = "kraken")]
    pub krakens: Vec<KrakenCfg>,
    /// reference transmitters at surveyed spots, for time differences
    #[serde(default, rename = "reference")]
    pub references: Vec<ReferenceCfg>,
    #[serde(default)]
    pub tdoa: TdoaCfg,
    #[serde(default)]
    pub fix: FixCfg,
    /// the weekly accuracy test against a beacon at a surveyed spot
    #[serde(default)]
    pub beacon: Option<BeaconCfg>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CoreCfg {
    #[serde(default = "d_core_url")]
    pub url: String,
    /// the core's `local.token`; needed to publish receivers and readings
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
    /// the core's import folder, where IQ snippets are handed over to be shared as blobs
    #[serde(default = "d_imports")]
    pub imports_dir: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct CaptureCfg {
    /// passphrase for the capture form on the page; empty turns the form off
    #[serde(default)]
    pub form_key: String,
    /// addresses allowed to use the form, e.g. "10.54.12.0/29"; empty means any
    #[serde(default)]
    pub form_allow: Vec<String>,
    /// non-amateur ranges this site may measure anyway, as [low, high] in MHz
    #[serde(default)]
    pub local_bands: Vec<[f64; 2]>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct KrakenCfg {
    pub name: String,
    /// its Kraken App output, e.g. "http://10.54.12.9:8081/DOA_value.html"
    pub url: String,
    /// which way the array's zero points; leave out to use the heading it reports
    #[serde(default)]
    pub heading_deg: Option<f64>,
    /// leave out to use the position it reports, or the node's
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    /// the smallest bearing error claimed, degrees (1 sigma)
    #[serde(default = "d_sigma_deg")]
    pub sigma_deg: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCfg {
    pub name: String,
    pub freq_mhz: f64,
    #[serde(default = "d_ref_bw")]
    pub bw_khz: f64,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct TdoaCfg {
    /// fetch every site's snippets and work out time differences; one or two sites, not all
    #[serde(default)]
    pub solve: bool,
    /// how far apart sites' recordings may start; their clocks must be at least this close
    #[serde(default = "d_skew")]
    pub max_skew_ms: u64,
    /// timing error of a clean measurement, microseconds (1 sigma)
    #[serde(default = "d_sigma_us")]
    pub sigma_us: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FixCfg {
    /// how fast signal strength falls with distance: 2 in free space, 3 to 4 over hills
    #[serde(default = "d_path_exp")]
    pub path_loss_exp: f64,
    /// error of one signal-strength reading, dB (1 sigma)
    #[serde(default = "d_level_sigma")]
    pub level_sigma_db: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct BeaconCfg {
    pub name: String,
    pub freq_mhz: f64,
    #[serde(default = "d_ref_bw")]
    pub bw_khz: f64,
    /// the surveyed spot
    pub lat: f64,
    pub lon: f64,
    /// "mon" to "sun", UTC
    pub weekday: String,
    pub hour_utc: u32,
    #[serde(default)]
    pub minute: u32,
    #[serde(default = "d_10")]
    pub secs: u32,
    /// ask for the test from this site; set it on one site only
    #[serde(default)]
    pub schedule: bool,
    /// a [[reference]] by name, to include time differences in the test
    #[serde(default)]
    pub reference: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ReceiverCfg {
    /// short id, unique on this site: letters, digits and hyphens
    pub name: String,
    #[serde(default)]
    pub antenna: String,
    /// what the receiver can tune, in MHz
    pub min_mhz: f64,
    pub max_mhz: f64,
    /// leave out to use the node's own position
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    /// the mesh-facing OpenWebRX+ instance; published only if its settings pass the band check
    #[serde(default)]
    pub openwebrx_url: Option<String>,
    /// that instance's settings.json
    #[serde(default)]
    pub openwebrx_settings: Option<PathBuf>,
    #[serde(default)]
    pub capture: Option<RxCapture>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct RxCapture {
    /// rtl_power and any leading arguments, e.g. ["rtl_power", "-d", "1"]
    pub command: Vec<String>,
    /// tuner gain; leave out for automatic gain
    #[serde(default)]
    pub gain_db: Option<f64>,
    #[serde(default)]
    pub ppm: Option<i32>,
    /// add to rtl_power's dB to get dBm at the antenna, if measured with a signal generator
    #[serde(default)]
    pub calibration_db: Option<f64>,
    /// rtl_sdr and any leading arguments, for time-difference snippets
    #[serde(default)]
    pub iq_command: Option<Vec<String>>,
}

fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-sdr")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8798".parse().unwrap()
}
fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}
fn d_imports() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/imports")
}
fn d_300() -> u64 {
    300
}
fn d_10() -> u32 {
    10
}
fn d_sigma_deg() -> f64 {
    5.0
}
fn d_ref_bw() -> f64 {
    16.0
}
fn d_skew() -> u64 {
    250
}
fn d_sigma_us() -> f64 {
    3.0
}
fn d_path_exp() -> f64 {
    3.0
}
fn d_level_sigma() -> f64 {
    8.0
}

impl Default for CoreCfg {
    fn default() -> Self {
        Self {
            url: d_core_url(),
            token_file: d_token(),
            imports_dir: d_imports(),
        }
    }
}

impl Default for TdoaCfg {
    fn default() -> Self {
        Self {
            solve: false,
            max_skew_ms: d_skew(),
            sigma_us: d_sigma_us(),
        }
    }
}

impl Default for FixCfg {
    fn default() -> Self {
        Self {
            path_loss_exp: d_path_exp(),
            level_sigma_db: d_level_sigma(),
        }
    }
}

/// An address range like "10.54.12.0/29" or "fd00::/8"; a bare address is a range of one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Net {
    addr: IpAddr,
    bits: u8,
}

impl Net {
    pub fn parse(s: &str) -> Result<Net> {
        let (a, b) = s.split_once('/').unwrap_or((s, ""));
        let addr: IpAddr = a
            .trim()
            .parse()
            .with_context(|| format!("{s:?} is not an address"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let bits = if b.is_empty() {
            max
        } else {
            b.trim()
                .parse()
                .with_context(|| format!("{s:?} has a bad prefix length"))?
        };
        if bits > max {
            bail!("{s:?} has a prefix longer than the address");
        }
        Ok(Net { addr, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        let mask = |len: u32| {
            if self.bits == 0 {
                0
            } else {
                u128::MAX << (len - self.bits as u32)
            }
        };
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let m = mask(32) as u32;
                u32::from(n) & m == u32::from(a) & m
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let m = mask(128);
                u128::from(n) & m == u128::from(a) & m
            }
            _ => false,
        }
    }
}

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

impl BeaconCfg {
    /// Monday is 0.
    pub fn weekday_index(&self) -> Option<i64> {
        WEEKDAYS
            .iter()
            .position(|d| d.eq_ignore_ascii_case(self.weekday.get(..3).unwrap_or("")))
            .map(|i| i as i64)
    }
}

fn hz(mhz: f64) -> u64 {
    (mhz * 1e6).round() as u64
}

impl ReferenceCfg {
    pub fn freq_hz(&self) -> u64 {
        hz(self.freq_mhz)
    }

    pub fn bw_hz(&self) -> u64 {
        hz(self.bw_khz / 1000.0)
    }
}

impl BeaconCfg {
    pub fn freq_hz(&self) -> u64 {
        hz(self.freq_mhz)
    }

    pub fn bw_hz(&self) -> u64 {
        hz(self.bw_khz / 1000.0)
    }
}

fn pos_ok(lat: Option<f64>, lon: Option<f64>) -> bool {
    crate::directory::valid_pos(lat, lon)
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 32
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl ReceiverCfg {
    pub fn min_hz(&self) -> u64 {
        (self.min_mhz * 1e6).round() as u64
    }

    pub fn max_hz(&self) -> u64 {
        (self.max_mhz * 1e6).round() as u64
    }

    fn validate(&self) -> Result<()> {
        let n = &self.name;
        if !valid_name(n) {
            bail!("receiver name {n:?} must be 1-32 lowercase letters, digits or hyphens");
        }
        if !(self.min_mhz > 0.0 && self.min_mhz < self.max_mhz) {
            bail!("receiver {n}: min_mhz must be above 0 and below max_mhz");
        }
        match (self.lat, self.lon) {
            (Some(la), Some(lo))
                if (-90.0..=90.0).contains(&la) && (-180.0..=180.0).contains(&lo) => {}
            (None, None) => {}
            _ => bail!("receiver {n}: give both lat and lon, in degrees, or neither"),
        }
        if let Some(u) = &self.openwebrx_url {
            if !u.starts_with("http://") || u.len() > 200 {
                bail!("receiver {n}: openwebrx_url must be an http:// address on the mesh");
            }
            if self.openwebrx_settings.is_none() {
                bail!(
                    "receiver {n}: openwebrx_url needs openwebrx_settings, so the profiles it \
                     offers over RF can be checked against the amateur bands"
                );
            }
        }
        if self.antenna.chars().count() > 100 {
            bail!("receiver {n}: antenna is limited to 100 characters");
        }
        if let Some(c) = &self.capture {
            if c.command.is_empty() {
                bail!("receiver {n}: capture.command must name the rtl_power program");
            }
            if c.iq_command.as_ref().is_some_and(|v| v.is_empty()) {
                bail!("receiver {n}: capture.iq_command must name the rtl_sdr program");
            }
            if c.gain_db.is_some_and(|g| !(0.0..=60.0).contains(&g)) {
                bail!("receiver {n}: capture.gain_db must be 0 to 60");
            }
        }
        Ok(())
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.status_interval_s < 60 {
            bail!("status_interval_s must be at least 60");
        }
        for (i, r) in self.receivers.iter().enumerate() {
            r.validate()?;
            if self.receivers[..i].iter().any(|o| o.name == r.name) {
                bail!("two receivers are named {:?}", r.name);
            }
        }
        for [lo, hi] in &self.capture.local_bands {
            if !(*lo > 0.0 && lo < hi) {
                bail!("capture.local_bands entries are [low, high] in MHz, low below high");
            }
        }
        let key = &self.capture.form_key;
        if !key.is_empty() && key.chars().count() < 12 {
            bail!("capture.form_key must be at least 12 characters, or empty to turn the form off");
        }
        for n in &self.capture.form_allow {
            Net::parse(n).context("capture.form_allow")?;
        }
        for (i, k) in self.krakens.iter().enumerate() {
            if !valid_name(&k.name) || self.krakens[..i].iter().any(|o| o.name == k.name) {
                bail!("kraken names must be unique, lowercase letters, digits or hyphens");
            }
            if !k.url.starts_with("http://") {
                bail!(
                    "kraken {}: url must be http://, e.g. http://10.54.12.9:8081/DOA_value.html",
                    k.name
                );
            }
            if !(k.sigma_deg > 0.0 && k.sigma_deg <= 90.0) || !pos_ok(k.lat, k.lon) {
                bail!(
                    "kraken {}: sigma_deg must be 0 to 90, and lat and lon given together",
                    k.name
                );
            }
        }
        for r in &self.references {
            let (lo, hi) = (r.freq_hz() - r.bw_hz() / 2, r.freq_hz() + r.bw_hz() / 2);
            if band_of(lo, hi).is_none() {
                bail!(
                    "reference {:?}: its snippets cross the mesh as recordings, so it must be an \
                     amateur transmitter (a club beacon or repeater), not NOAA or broadcast",
                    r.name
                );
            }
            if !pos_ok(Some(r.lat), Some(r.lon)) || r.name.is_empty() || r.name.chars().count() > 40
            {
                bail!(
                    "reference {:?}: give a name of up to 40 characters and its surveyed lat and lon",
                    r.name
                );
            }
        }
        if !(1.5..=6.0).contains(&self.fix.path_loss_exp) || self.fix.level_sigma_db <= 0.0 {
            bail!("fix.path_loss_exp must be 1.5 to 6 and fix.level_sigma_db above 0");
        }
        if self.tdoa.sigma_us.is_nan()
            || self.tdoa.sigma_us <= 0.0
            || !(10..=2000).contains(&self.tdoa.max_skew_ms)
        {
            bail!("tdoa.sigma_us must be above 0 and tdoa.max_skew_ms 10 to 2000");
        }
        if let Some(b) = &self.beacon {
            if b.weekday_index().is_none() || b.hour_utc > 23 || b.minute > 59 {
                bail!("beacon: weekday is mon to sun, hour_utc 0 to 23, minute 0 to 59");
            }
            let (lo, hi) = (b.freq_hz() - b.bw_hz() / 2, b.freq_hz() + b.bw_hz() / 2);
            if band_of(lo, hi).is_none() || !pos_ok(Some(b.lat), Some(b.lon)) {
                bail!("beacon: it must be on an amateur frequency, with its surveyed lat and lon");
            }
            if let Some(r) = &b.reference
                && !self.references.iter().any(|x| &x.name == r)
            {
                bail!("beacon: reference {r:?} is not one of the [[reference]] entries");
            }
            if !(1..=300).contains(&b.secs) || b.name.is_empty() || b.name.chars().count() > 40 {
                bail!("beacon: secs is 1 to 300, and the name up to 40 characters");
            }
        }
        Ok(())
    }

    /// Whether the capture form may be used from `ip`.
    pub fn form_allowed(&self, ip: IpAddr) -> bool {
        let nets = &self.capture.form_allow;
        nets.is_empty()
            || nets
                .iter()
                .filter_map(|n| Net::parse(n).ok())
                .any(|n| n.contains(ip))
    }

    pub fn reference(&self, name: &str) -> Option<&ReferenceCfg> {
        self.references
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(name))
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("sdr.db")
    }

    pub fn label(&self, node: &str) -> String {
        self.site_names
            .iter()
            .find(|(k, _)| ridgewarden_client::util::norm_node(k) == node)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| node.to_string())
    }

    fn local_bands_hz(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.capture
            .local_bands
            .iter()
            .map(|[lo, hi]| ((lo * 1e6).round() as u64, (hi * 1e6).round() as u64))
    }

    /// Whether this site may measure `lo..=hi`: the amateur band it is in, or the local
    /// band that allows it; otherwise why not.
    pub fn may_measure(&self, lo: u64, hi: u64) -> Result<String, String> {
        if let Some(b) = band_of(lo, hi) {
            return Ok(format!("amateur {}", b.name));
        }
        if let Some((a, b)) = self.local_bands_hz().find(|(a, b)| *a <= lo && hi <= *b) {
            return Ok(format!("local band {} to {}", mhz(a), mhz(b)));
        }
        Err(format!(
            "{} to {} is not inside an amateur band, and this site's capture.local_bands \
             doesn't allow it",
            mhz(lo),
            mhz(hi)
        ))
    }

    pub fn may_measure_at(&self, hz: f64) -> bool {
        let f = hz.max(0.0) as u64;
        self.may_measure(f, f).is_ok()
    }
}

pub const EXAMPLE: &str = r#"# ridgewarden-sdr configuration
data_dir = "/var/lib/ridgewarden-sdr"
listen = "0.0.0.0:8798"
# how often each receiver's live status (web UI answering, last capture) is published
status_interval_s = 300

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"
# where IQ snippets are handed to the core to share as blobs
imports_dir = "/var/lib/ridgewarden/imports"

[capture]
# a passphrase people type into the capture form on the page, at least 12 characters.
# Anyone who has it can make this station sign capture requests. It crosses the mesh in the
# clear like everything on amateur RF, so treat it as a spam gate, not a password. Empty
# turns the form off; the measure command still works.
form_key = ""
# only take form posts from these addresses, e.g. this site's LAN; empty means any
form_allow = []
# non-amateur ranges this site may measure anyway, [low, high] in MHz. Readings are a few
# numbers, never audio or a waterfall; see the README before adding any.
# local_bands = [[162.4, 162.55]]

[site_names]
# "tst-cuesta" = "Cuesta Peak"

# One [[receiver]] per SDR at this site.
# [[receiver]]
# name = "rx1"
# antenna = "discone, 30 ft above the roof"
# min_mhz = 24
# max_mhz = 1766
# # position; leave out to use the node's own (from its sysinfo)
# # lat = 35.33
# # lon = -120.62
# # the mesh-facing OpenWebRX+ instance (amateur profiles only) and its settings.json.
# # The link is withheld from the directory if any profile there is outside the amateur bands.
# openwebrx_url = "http://tst-pi.local.mesh:8074/"
# openwebrx_settings = "/var/lib/openwebrx-rf/settings.json"
#
# # for captures; rtl_power and rtl_sdr need a dongle nothing else holds
# [receiver.capture]
# command = ["rtl_power", "-d", "1"]
# # records IQ for time differences when a capture names a reference
# iq_command = ["rtl_sdr", "-d", "1"]
# gain_db = 30           # leave out for automatic gain
# ppm = 0
# calibration_db = -97.5 # add to rtl_power's dB for dBm; leave out unless you measured it

# A KrakenSDR at this site, with krakensdr_doa's output format set to "Kraken App".
# [[kraken]]
# name = "kraken"
# url = "http://10.54.12.9:8081/DOA_value.html"
# heading_deg = 0      # which way the array's zero points; leave out to use what it reports
# sigma_deg = 5        # the least bearing error to claim
# # lat = 35.33        # leave out to use its GPS, or the node's position
# # lon = -120.62

# Reference transmitters for time differences: amateur transmitters at surveyed spots that
# are on the air with modulation (a beacon sending data, a busy digipeater). The site asking
# for a capture puts the reference's details in the request; other sites take them from there.
# [[reference]]
# name = "TST digipeater"
# freq_mhz = 144.39
# bw_khz = 16
# lat = 35.4044
# lon = -120.5981

[tdoa]
# fetch every site's snippets and work out the time differences; turn on at one or two sites
solve = false
# how far apart sites' recordings may start; keep clocks on NTP or GPS time
max_skew_ms = 250
# timing error of a clean measurement, microseconds; the weekly beacon test shows if it's right
sigma_us = 3.0

[fix]
# how fast signal strength falls with distance: 2 in free space, 3 to 4 over hills
path_loss_exp = 3.0
# error of one signal-strength reading, dB
level_sigma_db = 8.0

# The weekly accuracy test: a beacon at a surveyed spot. Set it up the same way at every
# site, with schedule = true at exactly one.
# [beacon]
# name = "TST beacon"
# freq_mhz = 144.285
# bw_khz = 16
# lat = 35.3611
# lon = -120.6522
# weekday = "sun"
# hour_utc = 18
# minute = 30
# secs = 10
# schedule = false
# reference = "TST digipeater"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn with_receiver(extra: &str) -> Result<Config> {
        let c: Config = toml::from_str(&format!(
            "{EXAMPLE}\n[[receiver]]\nname = \"rx1\"\nmin_mhz = 24\nmax_mhz = 1766\n{extra}"
        ))?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn example_parses() {
        let c: Config = toml::from_str(EXAMPLE).unwrap();
        c.validate().unwrap();
        assert_eq!(c.listen.port(), 8798);
        let c = with_receiver("[receiver.capture]\ncommand = [\"rtl_power\"]").unwrap();
        assert_eq!(c.receivers[0].max_hz(), 1_766_000_000);
    }

    #[test]
    fn a_web_link_needs_its_settings_file() {
        let e = with_receiver("openwebrx_url = \"http://tst-pi.local.mesh:8074/\"").unwrap_err();
        assert!(e.to_string().contains("openwebrx_settings"), "{e}");
        assert!(with_receiver("lat = 35.3").is_err(), "lat without lon");
        assert!(with_receiver("[receiver.capture]\ncommand = []").is_err());
    }

    #[test]
    fn local_bands_allow_measuring_outside_the_amateur_bands() {
        let mut c: Config = toml::from_str(EXAMPLE).unwrap();
        assert!(
            c.may_measure(146_512_000, 146_528_000)
                .unwrap()
                .contains("2 m")
        );
        let e = c.may_measure(162_542_500, 162_557_500).unwrap_err();
        assert!(e.contains("162.5425 MHz"), "{e}");
        c.capture.local_bands = vec![[162.4, 162.56]];
        assert!(
            c.may_measure(162_542_500, 162_557_500)
                .unwrap()
                .starts_with("local band")
        );
        assert!(
            c.may_measure(162_550_000, 162_570_000).is_err(),
            "runs past its edge"
        );
    }

    #[test]
    fn address_ranges() {
        let n = Net::parse("10.54.12.0/29").unwrap();
        assert!(n.contains("10.54.12.5".parse().unwrap()));
        assert!(!n.contains("10.54.12.9".parse().unwrap()));
        assert!(
            n.contains("::ffff:10.54.12.2".parse().unwrap()),
            "v4 seen through a v6 socket"
        );
        assert!(
            Net::parse("fd00::/8")
                .unwrap()
                .contains("fd12::1".parse().unwrap())
        );
        assert!(
            Net::parse("10.0.0.1")
                .unwrap()
                .contains("10.0.0.1".parse().unwrap())
        );
        assert!(
            Net::parse("0.0.0.0/0")
                .unwrap()
                .contains("192.0.2.1".parse().unwrap())
        );
        assert!(Net::parse("10.0.0.0/33").is_err());
        assert!(Net::parse("mesh").is_err());
    }

    #[test]
    fn form_and_hunting_settings_are_checked() {
        let base: Config = toml::from_str(EXAMPLE).unwrap();
        let mut c = base.clone();
        c.capture.form_key = "short".into();
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("12 characters")
        );
        c.capture.form_key = "a long enough passphrase".into();
        c.capture.form_allow = vec!["10.54.12.0/29".into()];
        c.validate().unwrap();
        assert!(c.form_allowed("10.54.12.3".parse().unwrap()));
        assert!(!c.form_allowed("10.99.0.3".parse().unwrap()));
        let r: Config = toml::from_str(&format!(
            "{EXAMPLE}\n[[reference]]\nname = \"wx\"\nfreq_mhz = 162.55\nlat = 35.3\nlon = -120.6\n"
        ))
        .unwrap();
        let e = r.validate().unwrap_err().to_string();
        assert!(e.contains("amateur transmitter"), "{e}");
        let b: Config = toml::from_str(&format!(
            "{EXAMPLE}\n[beacon]\nname = \"b\"\nfreq_mhz = 144.285\nlat = 35.3\nlon = -120.6\nweekday = \"someday\"\nhour_utc = 18\n"
        ))
        .unwrap();
        assert!(b.validate().is_err());
    }
}
