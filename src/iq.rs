//! IQ snippets for time differences of arrival. A site records 2 s of raw I/Q with
//! `rtl_sdr` (unsigned 8-bit interleaved I/Q, osmocom rtl-sdr) tuned between the target and
//! a reference transmitter, then cuts out just those two channels, decimated to 51.2 kHz.
//! That snippet, about 800 kB, is what crosses the mesh as a blob; the raw recording
//! (10 MB) never leaves the site.
//!
//! A snippet file is `RWSDRIQ1`, a little-endian u32 header length, the JSON header, then
//! the target channel and the reference channel as interleaved i16 I/Q.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::dsp::{C, channelize, clipped, lowpass};
use ridgewarden_client::util::now_ms;

/// rtl_sdr's default sample rate, and an even multiple of the snippet rate.
pub const FS: f64 = 2_048_000.0;
pub const DECIM: usize = 40;
/// Samples per second in a snippet.
pub const RATE: f64 = FS / DECIM as f64;
pub const SNIPPET_S: f64 = 2.0;
/// rtl_sdr's first samples come while the tuner settles; they are dropped.
pub const SETTLE_S: f64 = 0.25;
/// Target and reference have to share one 2 MHz recording, clear of the DC spike between them.
pub const MIN_SEP: u64 = 100_000;
pub const MAX_SEP: u64 = 1_700_000;
/// Widest channel a snippet carries.
pub const MAX_CHANNEL: u64 = 40_000;
const MAGIC: &[u8; 8] = b"RWSDRIQ1";
const MAX_SAMPLES: usize = (RATE * SNIPPET_S * 1.1) as usize;
const MAX_HEADER: usize = 4096;
/// The largest a snippet file can be.
pub const MAX_BYTES: usize = 12 + MAX_HEADER + MAX_SAMPLES * 8;

/// Where to tune: halfway between the two channels, so the DC spike falls between them.
pub fn center(target: u64, reference: u64) -> u64 {
    target.min(reference) + target.abs_diff(reference) / 2
}

pub fn args(center: u64, gain_db: Option<f64>, ppm: Option<i32>) -> Vec<String> {
    let n = ((SNIPPET_S + SETTLE_S) * FS) as u64;
    let mut a = vec![
        "-f".into(),
        center.to_string(),
        "-s".into(),
        (FS as u64).to_string(),
        "-n".into(),
        n.to_string(),
    ];
    if let Some(g) = gain_db {
        a.extend(["-g".into(), g.to_string()]);
    }
    if let Some(p) = ppm {
        a.extend(["-p".into(), p.to_string()]);
    }
    a.push("-".into());
    a
}

pub struct Raw {
    pub bytes: Vec<u8>,
    /// wall clock when the first bytes arrived: the recording's start, give or take USB
    /// latency, which the reference transmitter takes out later
    pub first_ms: i64,
}

/// Run the configured rtl_sdr command and read the recording from its output.
pub async fn record(cmd: &[String], args: &[String], timeout: Duration) -> Result<Raw> {
    let (prog, pre) = cmd.split_first().context("iq_command is empty")?;
    let mut child = tokio::process::Command::new(prog)
        .args(pre)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {prog}"))?;
    let mut out = child.stdout.take().context("no stdout from rtl_sdr")?;
    let mut err = child.stderr.take().context("no stderr from rtl_sdr")?;
    let read = async {
        let mut bytes = Vec::with_capacity(((SNIPPET_S + SETTLE_S) * FS * 2.0) as usize);
        let mut buf = vec![0u8; 1 << 16];
        let mut first_ms = None;
        loop {
            let n = out.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            first_ms.get_or_insert_with(now_ms);
            bytes.extend_from_slice(&buf[..n]);
        }
        let mut msg = vec![];
        err.read_to_end(&mut msg).await?;
        let msg = String::from_utf8_lossy(&msg).into_owned();
        let status = child.wait().await?;
        anyhow::Ok((bytes, first_ms, status, msg))
    };
    let (bytes, first_ms, status, msg) = tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| anyhow!("{prog} did not finish within {} s", timeout.as_secs()))??;
    if !status.success() {
        let last = msg
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        bail!(
            "{prog} failed ({status}): {}",
            last.trim().chars().take(200).collect::<String>()
        );
    }
    let first_ms = first_ms.with_context(|| format!("{prog} recorded nothing"))?;
    Ok(Raw { bytes, first_ms })
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Header {
    pub capture: String,
    pub receiver: String,
    pub rate: f64,
    /// wall clock of the first sample kept, ms
    pub first_ms: i64,
    pub target_hz: u64,
    pub target_bw: u64,
    pub ref_hz: u64,
    pub ref_bw: u64,
    /// share of raw samples at the ADC's limits
    pub clipped: f64,
    pub(crate) scale_t: f64,
    pub(crate) scale_r: f64,
    pub(crate) n: usize,
}

#[derive(Clone, Debug)]
pub struct Snippet {
    pub header: Header,
    pub target: Vec<C>,
    pub reference: Vec<C>,
}

fn channel_taps(bw: u64) -> Vec<f64> {
    // passband to half the channel, transition over the next 10 kHz
    lowpass(
        (bw as f64 / 2.0 + 5_000.0) / FS,
        (4.0 * FS / 10_000.0) as usize | 1,
    )
}

/// Cut the target and reference channels out of a raw recording tuned to `center`.
pub fn snippet(
    raw: &Raw,
    capture: &str,
    receiver: &str,
    center: u64,
    (target_hz, target_bw): (u64, u64),
    (ref_hz, ref_bw): (u64, u64),
) -> Snippet {
    let skip = ((SETTLE_S * FS) as usize * 2).min(raw.bytes.len());
    let bytes = &raw.bytes[skip..];
    let chan = |hz: u64, bw: u64| {
        let h = channel_taps(bw);
        channelize(bytes, FS, hz as f64 - center as f64, &h, DECIM)
    };
    let target = chan(target_hz, target_bw);
    let reference = chan(ref_hz, ref_bw);
    Snippet {
        header: Header {
            capture: capture.into(),
            receiver: receiver.into(),
            rate: RATE,
            // the channel filter's delay is the same at every site, so it cancels
            first_ms: raw.first_ms + (SETTLE_S * 1000.0) as i64,
            target_hz,
            target_bw,
            ref_hz,
            ref_bw,
            clipped: clipped(bytes),
            scale_t: 0.0,
            scale_r: 0.0,
            n: target.len().min(reference.len()),
        },
        target,
        reference,
    }
}

fn pack(out: &mut Vec<u8>, x: &[C], n: usize) -> f64 {
    let peak = x[..n]
        .iter()
        .map(|c| c.re.abs().max(c.im.abs()))
        .fold(0.0, f64::max)
        .max(1e-12);
    let k = 32767.0 / peak;
    for c in &x[..n] {
        out.extend_from_slice(&((c.re * k).round() as i16).to_le_bytes());
        out.extend_from_slice(&((c.im * k).round() as i16).to_le_bytes());
    }
    1.0 / k
}

pub fn encode(s: &Snippet) -> Vec<u8> {
    let n = s.target.len().min(s.reference.len()).min(MAX_SAMPLES);
    let mut body = vec![];
    let scale_t = pack(&mut body, &s.target, n);
    let scale_r = pack(&mut body, &s.reference, n);
    let header = Header {
        scale_t,
        scale_r,
        n,
        ..s.header.clone()
    };
    let json = serde_json::to_vec(&header).expect("header serializes");
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(&body);
    out
}

pub fn decode(b: &[u8]) -> Result<Snippet> {
    if b.len() < 12 || &b[..8] != MAGIC {
        bail!("not an IQ snippet");
    }
    let hlen = u32::from_le_bytes(b[8..12].try_into().expect("4 bytes")) as usize;
    if hlen > MAX_HEADER || b.len() < 12 + hlen {
        bail!("IQ snippet header is damaged");
    }
    let h: Header = serde_json::from_slice(&b[12..12 + hlen]).context("IQ snippet header")?;
    let body = &b[12 + hlen..];
    let ok = h.n <= MAX_SAMPLES
        && body.len() == h.n * 8
        && h.rate == RATE
        && h.scale_t.is_finite()
        && h.scale_r.is_finite()
        && (0.0..=1.0).contains(&h.clipped);
    if !ok {
        bail!("IQ snippet is the wrong size or rate");
    }
    let unpack = |part: &[u8], k: f64| -> Vec<C> {
        part.as_chunks::<4>()
            .0
            .iter()
            .map(|q| {
                let i = i16::from_le_bytes([q[0], q[1]]) as f64;
                let v = i16::from_le_bytes([q[2], q[3]]) as f64;
                C::new(i * k, v * k)
            })
            .collect()
    };
    Ok(Snippet {
        target: unpack(&body[..h.n * 4], h.scale_t),
        reference: unpack(&body[h.n * 4..], h.scale_r),
        header: h,
    })
}

/// Made-up transmitters for tests and `sim-rtl-sdr`: 4-level FSK at 4800 baud, the shape
/// of DMR or P25 phase 1, from a fixed pseudo-random symbol stream, so every simulated
/// site hears the same waveform delayed by its own distance.
#[derive(Clone, Copy, Debug)]
pub struct Emitter {
    pub seed: u64,
    /// Hz of deviation per symbol step; 0 is a plain carrier
    pub dev: f64,
    pub amp: f64,
}

pub const BAUD: f64 = 4800.0;

fn symbol(seed: u64, k: i64) -> f64 {
    let mut z = seed ^ (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    [-3.0, -1.0, 1.0, 3.0][(z % 4) as usize]
}

impl Emitter {
    /// The baseband signal at `rate` from absolute time `t0` (s), as heard `delay` s late,
    /// with the receiver's clock running `ppm` fast and its tuning `off_hz` away.
    pub fn stream(
        &self,
        t0: f64,
        rate: f64,
        delay: f64,
        ppm: f64,
        off_hz: f64,
    ) -> impl Iterator<Item = C> + use<> {
        let e = *self;
        let dt = 1.0 / (rate * (1.0 + ppm * 1e-6));
        let mut phase = 0.0;
        (0u64..).map(move |i| {
            // integrate the frequency over the sample, splitting at a symbol change, so
            // transitions fall between samples where they really happen
            let (a, z) = (t0 + i as f64 * dt - delay, t0 + (i + 1) as f64 * dt - delay);
            let (ka, kz) = ((a * BAUD).floor() as i64, (z * BAUD).floor() as i64);
            let cycles = if ka == kz {
                e.dev * symbol(e.seed, ka) * dt
            } else {
                let edge = kz as f64 / BAUD;
                e.dev * (symbol(e.seed, ka) * (edge - a) + symbol(e.seed, kz) * (z - edge))
            };
            phase =
                (phase + std::f64::consts::TAU * (cycles + off_hz * dt)) % std::f64::consts::TAU;
            C::cis(phase).scale(e.amp)
        })
    }

    pub fn samples(
        &self,
        t0: f64,
        n: usize,
        rate: f64,
        delay: f64,
        ppm: f64,
        off_hz: f64,
    ) -> Vec<C> {
        self.stream(t0, rate, delay, ppm, off_hz).take(n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_round_trip_and_reject_damage() {
        let e = Emitter {
            seed: 1,
            dev: 600.0,
            amp: 0.5,
        };
        let s = Snippet {
            header: Header {
                capture: "c".into(),
                receiver: "rx1".into(),
                rate: RATE,
                first_ms: 5,
                target_hz: 146_520_000,
                target_bw: 16_000,
                ref_hz: 147_000_000,
                ref_bw: 16_000,
                clipped: 0.0,
                scale_t: 0.0,
                scale_r: 0.0,
                n: 1000,
            },
            target: e.samples(0.0, 1000, RATE, 0.0, 0.0, 0.0),
            reference: e.samples(1.0, 1000, RATE, 0.0, 0.0, 0.0),
        };
        let b = encode(&s);
        assert!(b.len() > 8000 && b.len() < 8400, "{}", b.len());
        let back = decode(&b).unwrap();
        assert_eq!(back.header.first_ms, 5);
        assert!(
            back.target
                .iter()
                .zip(&s.target)
                .all(|(a, x)| (*a - *x).norm_sqr() < 1e-8)
        );
        assert!(decode(&b[..b.len() - 4]).is_err());
        assert!(decode(b"RWSDRIQ1\xff\xff\xff\x7f").is_err());
        assert!(decode(b"hello").is_err());
    }

    #[test]
    fn tuning_and_rtl_sdr_arguments() {
        assert_eq!(center(146_520_000, 147_000_000), 146_760_000);
        let a = args(146_760_000, None, Some(1));
        assert_eq!(
            a,
            [
                "-f",
                "146760000",
                "-s",
                "2048000",
                "-n",
                "4608000",
                "-p",
                "1",
                "-"
            ]
        );
    }
}
