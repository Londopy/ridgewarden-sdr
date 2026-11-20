//! Signal strength from `rtl_power` (part of rtl-sdr, osmocom).
//!
//! rtl_power writes one CSV line per hop: `date, time, Hz low, Hz high, Hz step, samples,
//! dB, dB, ...`. Each dB value is 10*log10 of the bin's averaged power divided by the
//! sample rate and count, which is relative to the dongle's ADC, not calibrated to dBm.
//! We run it once (`-1`) for the requested time (`-i`) and read the bins from stdout.
//!
//! Two quirks of its `csv_dbm`: the last bin of each line is printed twice, and "Hz low" is
//! rounded, so a bin's real center can sit up to a step below `low + i * step`. Bins are
//! only counted where that uncertainty still keeps them inside the span asked about.

use std::fmt::Write as _;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ridgewarden_client::util::{median, ms_to_rfc3339, now_ms};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bin {
    /// nominal center, `low + i * step`
    pub hz: f64,
    pub db: f64,
    pub step: f64,
}

impl Bin {
    /// The most the bin could cover, given where its center might really be.
    fn extent(&self) -> (f64, f64) {
        (self.hz - 1.5 * self.step, self.hz + 0.5 * self.step)
    }
}

pub fn parse(csv: &str) -> Result<Vec<Bin>> {
    let mut bins = vec![];
    for (n, line) in csv.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        if f.len() < 7 {
            bail!(
                "rtl_power line {} has {} fields, expected at least 7",
                n + 1,
                f.len()
            );
        }
        let bad = |what: &str| anyhow!("rtl_power line {}: bad {what}", n + 1);
        let low: f64 = f[2].parse().map_err(|_| bad("low frequency"))?;
        let step: f64 = f[4].parse().map_err(|_| bad("bin step"))?;
        if step.is_nan() || step <= 0.0 {
            return Err(bad("bin step"));
        }
        // the last value repeats the one before it
        let values = &f[6..f.len() - 1];
        for (i, v) in values.iter().enumerate() {
            let db: f64 = v.parse().map_err(|_| bad("dB value"))?;
            // an empty bin comes out as -inf or nan
            if db.is_finite() {
                bins.push(Bin {
                    hz: low + i as f64 * step,
                    db,
                    step,
                });
            }
        }
    }
    Ok(bins)
}

/// The rtl_power run for one request: a span around the signal, with the signal left of
/// center so the dongle's DC spike lands outside it, and room either side for a noise floor.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub lo: u64,
    pub hi: u64,
    pub bin: u64,
}

pub fn plan(freq: u64, bw: u64) -> Plan {
    Plan {
        lo: freq.saturating_sub(2 * bw),
        hi: freq.saturating_add(4 * bw),
        bin: (bw / 8).max(1),
    }
}

pub fn args(p: &Plan, secs: u32, gain_db: Option<f64>, ppm: Option<i32>) -> Vec<String> {
    let mut a = vec![
        "-f".into(),
        format!("{}:{}:{}", p.lo, p.hi, p.bin),
        "-i".into(),
        secs.to_string(),
        "-1".into(),
        // rtl_power's help recommends 20-50% crop: the hop's filtered edges read low
        "-c".into(),
        "20%".into(),
    ];
    if let Some(g) = gain_db {
        a.extend(["-g".into(), g.to_string()]);
    }
    if let Some(p) = ppm {
        a.extend(["-p".into(), p.to_string()]);
    }
    a
}

#[derive(Debug, Clone, PartialEq)]
pub struct Level {
    /// total power in the requested band, rtl_power's dB scale
    pub level_db: f64,
    /// the loudest single bin in the band
    pub peak_db: f64,
    /// median bin outside the band, if enough bins were measurable there
    pub floor_db: Option<f64>,
    /// band power over the same number of floor-level bins: ~0 dB means nothing there
    pub snr_db: Option<f64>,
    pub bins: usize,
}

fn db_sum(dbs: impl Iterator<Item = f64>) -> f64 {
    10.0 * dbs.map(|d| 10f64.powf(d / 10.0)).sum::<f64>().log10()
}

/// Band power for `lo..=hi`, from bins wholly inside it. Floor bins are taken a half
/// bandwidth clear of the band, and only where `floor_ok` says this site may measure.
pub fn level(bins: &[Bin], lo: u64, hi: u64, floor_ok: impl Fn(f64) -> bool) -> Option<Level> {
    let (lo, hi) = (lo as f64, hi as f64);
    let inside: Vec<f64> = bins
        .iter()
        .filter(|b| {
            let (a, z) = b.extent();
            a >= lo && z <= hi
        })
        .map(|b| b.db)
        .collect();
    if inside.is_empty() {
        return None;
    }
    let guard = (hi - lo) / 2.0;
    let mut out: Vec<f64> = bins
        .iter()
        .filter(|b| {
            let (a, z) = b.extent();
            (z < lo - guard || a > hi + guard) && floor_ok(a) && floor_ok(z)
        })
        .map(|b| b.db)
        .collect();
    let level_db = db_sum(inside.iter().copied());
    let floor_db = if out.len() >= 4 {
        median(&mut out)
    } else {
        None
    };
    Some(Level {
        level_db,
        peak_db: inside.iter().copied().fold(f64::MIN, f64::max),
        floor_db,
        snr_db: floor_db.map(|f| level_db - (f + 10.0 * (inside.len() as f64).log10())),
        bins: inside.len(),
    })
}

/// Run the configured rtl_power command with `args` and return what it printed.
pub async fn run(cmd: &[String], args: &[String], timeout: Duration) -> Result<String> {
    let (prog, pre) = cmd.split_first().context("capture command is empty")?;
    let child = tokio::process::Command::new(prog)
        .args(pre)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {prog}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("{prog} did not finish within {} s", timeout.as_secs()))??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        bail!(
            "{prog} failed ({}): {}",
            out.status,
            last.trim().chars().take(200).collect::<String>()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A carrier the simulator puts on the air: frequency, level in dB per bin, width.
#[derive(Clone, Debug, PartialEq)]
pub struct SimSignal {
    pub hz: u64,
    pub db: f64,
    pub width: u64,
}

impl std::str::FromStr for SimSignal {
    type Err = anyhow::Error;

    /// "146.52M:-20" or "146.52M:-20:12k"
    fn from_str(s: &str) -> Result<Self> {
        let mut it = s.split(':');
        let hz = crate::bands::parse_hz(it.next().unwrap_or(""))?;
        let db = it
            .next()
            .and_then(|d| d.parse().ok())
            .context("a signal is FREQ:DB[:WIDTH], e.g. 146.52M:-20:12k")?;
        let width = it.next().map(crate::bands::parse_hz).transpose()?;
        Ok(SimSignal {
            hz,
            db,
            width: width.unwrap_or(12_000),
        })
    }
}

/// rtl_power-format CSV for `-f lo:hi:bin`, with a noise floor and the given signals, in
/// hops of up to 2 MHz like the real thing. The noise is a fixed pattern, so runs repeat.
pub fn simulate(f: &str, floor_db: f64, signals: &[SimSignal]) -> Result<String> {
    let mut parts = f.split(':').map(crate::bands::parse_hz);
    let (Some(lo), Some(hi), Some(bin)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("-f takes lower:upper:bin_size");
    };
    let (lo, hi, bin) = (lo?, hi?, bin?);
    if hi <= lo {
        bail!("-f upper must be above lower");
    }
    let t = ms_to_rfc3339(now_ms());
    let stamp = format!("{}, {}", &t[..10], &t[11..19]);
    let mut out = String::new();
    let mut hop = lo;
    while hop < hi {
        let end = (hop + 2_000_000).min(hi);
        let n = (end - hop).div_ceil(bin);
        let _ = write!(out, "{stamp}, {hop}, {end}, {bin:.2}, 4096");
        let mut last = 0.0;
        for i in 0..n {
            let f = hop + i * bin;
            let noise = blake3::hash(&f.to_le_bytes()).as_bytes()[0] as f64 / 255.0 - 0.5;
            let mut lin = 10f64.powf((floor_db + noise) / 10.0);
            for s in signals {
                if f.abs_diff(s.hz) <= s.width / 2 {
                    lin += 10f64.powf(s.db / 10.0);
                }
            }
            last = 10.0 * lin.log10();
            let _ = write!(out, ", {last:.2}");
        }
        // as rtl_power does
        let _ = writeln!(out, ", {last:.2}");
        hop = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // two hops as rtl_power prints them (osmocom rtl-sdr, csv_dbm), last value repeated
    const SAMPLE: &str = "2026-11-19, 18:00:02, 146400000, 146500000, 12500.00, 160, -41.20, -40.95, -41.60, -40.70, -41.05, -40.88, -41.30, -40.90, -40.90\n\
2026-11-19, 18:00:02, 146500000, 146600000, 12500.00, 160, -41.00, -40.90, -22.10, -21.80, -41.10, -nan, -40.60, -41.00, -41.00\n";

    #[test]
    fn parses_hops_drops_the_repeat_and_skips_empty_bins() {
        let bins = parse(SAMPLE).unwrap();
        assert_eq!(bins.len(), 15);
        assert_eq!((bins[0].hz, bins[0].db), (146_400_000.0, -41.2));
        assert_eq!((bins[7].hz, bins[7].db), (146_487_500.0, -40.9));
        assert_eq!((bins[8].hz, bins[8].db), (146_500_000.0, -41.0));
        assert_eq!((bins[14].hz, bins[14].db), (146_587_500.0, -41.0));
        assert!(parse("2026-11-19, 18:00:02, 1, 2\n").is_err());
        assert!(parse("2026-11-19, 18:00:02, x, 2, 1, 1, -3, -3\n").is_err());
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn band_level_floor_and_snr() {
        let bins = parse(SAMPLE).unwrap();
        let l = level(&bins, 146_500_000, 146_550_000, |_| true).unwrap();
        assert_eq!(l.bins, 2, "only bins that can't reach past the band");
        assert!((l.peak_db - -21.8).abs() < 1e-9);
        // two bins near -22 dB add to about -18.9 dB
        assert!((l.level_db - -18.94).abs() < 0.02, "{l:?}");
        let floor = l.floor_db.unwrap();
        assert!((-41.3..-40.8).contains(&floor), "{floor}");
        // 2 bins of floor would be floor + 3 dB; the signal is ~19 dB above that
        assert!((l.snr_db.unwrap() - 19.0).abs() < 0.6, "{l:?}");
        let none = level(&bins, 146_500_000, 146_550_000, |_| false).unwrap();
        assert_eq!(
            none.floor_db, None,
            "floor bins outside what may be measured"
        );
        assert!(level(&bins, 147_000_000, 147_100_000, |_| true).is_none());
    }

    #[test]
    fn a_bin_straddling_the_band_edge_is_left_out() {
        // the bin printed at 146.5125 MHz may really be centered at 146.5 MHz
        let bins = parse(SAMPLE).unwrap();
        let wide = level(&bins, 146_481_250, 146_550_000, |_| true).unwrap();
        assert_eq!(wide.bins, 4);
        let tight = level(&bins, 146_487_500, 146_550_000, |_| true).unwrap();
        assert_eq!(
            tight.bins, 3,
            "the 146.5 MHz bin might reach below 146.4875"
        );
    }

    #[test]
    fn plan_keeps_the_signal_off_center() {
        let p = plan(146_520_000, 16_000);
        assert_eq!(
            p,
            Plan {
                lo: 146_488_000,
                hi: 146_584_000,
                bin: 2000
            }
        );
        let center = (p.lo + p.hi) / 2;
        assert!(center > 146_520_000 + 8_000, "DC spike outside the band");
        let a = args(&p, 10, Some(30.0), None);
        assert_eq!(
            a,
            [
                "-f",
                "146488000:146584000:2000",
                "-i",
                "10",
                "-1",
                "-c",
                "20%",
                "-g",
                "30"
            ]
        );
    }

    #[test]
    fn simulator_output_reads_back() {
        let sig: SimSignal = "146.52M:-20:8k".parse().unwrap();
        let csv = simulate("146488000:146584000:2000", -45.0, &[sig]).unwrap();
        assert_eq!(csv.lines().count(), 1);
        let bins = parse(&csv).unwrap();
        assert_eq!(bins.len(), 48);
        assert_eq!(
            csv.trim_end().split(", ").count(),
            6 + 48 + 1,
            "last value repeated"
        );
        let l = level(&bins, 146_512_000, 146_528_000, |_| true).unwrap();
        assert!((l.floor_db.unwrap() - -45.0).abs() < 0.6, "{l:?}");
        assert!(l.snr_db.unwrap() > 15.0, "{l:?}");
        let quiet = simulate("146488000:146584000:2000", -45.0, &[]).unwrap();
        let q = level(&parse(&quiet).unwrap(), 146_512_000, 146_528_000, |_| true).unwrap();
        assert!(q.snr_db.unwrap().abs() < 1.0, "{q:?}");
        let wide = simulate("144000000:148000000:100000", -45.0, &[]).unwrap();
        assert_eq!(wide.lines().count(), 2, "2 MHz hops");
        assert!("146.52M".parse::<SimSignal>().is_err());
    }
}
