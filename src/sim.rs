//! Stand-ins for the hardware, for tests and demos without a dongle: `sim-rtl-power`
//! prints rtl_power's CSV, `sim-rtl-sdr` writes rtl_sdr's raw 8-bit I/Q with made-up
//! transmitters delayed by their distance from the site, and `sim-kraken` serves a
//! KrakenSDR's `DOA_value.html`. They take the real programs' arguments, so the app runs
//! them exactly as it runs the real ones.

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use ridgewarden_client::util::now_ms;

use crate::bands::parse_hz;
use crate::dsp::C;
use crate::geo::{C_KM_S, LatLon, azimuth, distance_km};
use crate::iq::Emitter;
use crate::kraken;
use crate::rtlpower::{self, SimSignal};

/// Simulated waveforms are a function of time since this moment, the same in every process.
const EPOCH_MS: i64 = 1_795_000_000_000;

/// rtl_power's options; parsed apart from the app's own, whose -c is --config.
#[derive(Parser)]
#[command(name = "ridgewarden-sdr sim-rtl-power")]
pub struct PowerArgs {
    /// lower:upper:bin_size in Hz, as rtl_power takes it
    #[arg(short = 'f')]
    freqs: String,
    #[arg(short = 'i', default_value_t = 10)]
    interval: u64,
    #[arg(short = '1')]
    single: bool,
    /// gain; the made-up levels move with it, as real ones do, from a 30 dB reference
    #[arg(short = 'g')]
    gain: Option<f64>,
    #[arg(short = 'p', allow_negative_numbers = true)]
    ppm: Option<i32>,
    #[arg(short = 'c')]
    crop: Option<String>,
    #[arg(short = 'd')]
    device: Option<String>,
    /// noise floor per bin, dB
    #[arg(long, default_value_t = -45.0, allow_hyphen_values = true)]
    floor: f64,
    /// FREQ:DB[:WIDTH], e.g. 146.52M:-20:12k; may be repeated
    #[arg(long, allow_hyphen_values = true)]
    signal: Vec<SimSignal>,
}

pub fn rtl_power(a: PowerArgs) -> Result<()> {
    let shift = a.gain.map_or(0.0, |g| g - 30.0);
    let signals: Vec<SimSignal> = a
        .signal
        .into_iter()
        .map(|s| SimSignal {
            db: s.db + shift,
            ..s
        })
        .collect();
    let csv = rtlpower::simulate(&a.freqs, a.floor + shift, &signals)?;
    std::thread::sleep(Duration::from_secs(a.interval));
    print!("{csv}");
    Ok(())
}

fn latlon(s: &str) -> Result<LatLon> {
    let (a, o) = s.split_once(':').context("a position is LAT:LON")?;
    Ok(LatLon::new(a.trim().parse()?, o.trim().parse()?))
}

/// A simulated transmitter: FREQ:LAT:LON[:DEVIATION_HZ], 4-level FSK unless the deviation is 0.
#[derive(Clone, Debug)]
pub struct Tx {
    pub hz: u64,
    pub pos: LatLon,
    pub dev: f64,
}

impl std::str::FromStr for Tx {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let p: Vec<&str> = s.split(':').collect();
        if p.len() < 3 {
            bail!("a transmitter is FREQ:LAT:LON[:DEVIATION_HZ], e.g. 146.52M:35.4:-120.6:600");
        }
        Ok(Tx {
            hz: parse_hz(p[0])?,
            pos: LatLon::new(p[1].parse()?, p[2].parse()?),
            dev: p.get(3).map_or(Ok(600.0), |d| d.parse())?,
        })
    }
}

/// rtl_sdr's options.
#[derive(Parser)]
#[command(name = "ridgewarden-sdr sim-rtl-sdr")]
pub struct IqArgs {
    #[arg(short = 'f')]
    freq: u64,
    #[arg(short = 's', default_value_t = 2_048_000)]
    rate: u64,
    #[arg(short = 'n', default_value_t = 0)]
    samples: u64,
    #[arg(short = 'g')]
    gain: Option<f64>,
    #[arg(short = 'p', allow_negative_numbers = true)]
    ppm: Option<i32>,
    #[arg(short = 'd')]
    device: Option<String>,
    /// output file; "-" for stdout, the only kind supported here
    filename: String,
    /// where this receiver is, LAT:LON
    #[arg(long, value_parser = latlon)]
    site: LatLon,
    /// a transmitter; may be repeated
    #[arg(long = "tx", allow_hyphen_values = true)]
    txs: Vec<Tx>,
    /// how far off this dongle's crystal is, ppm: its clock and its tuning
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    crystal_ppm: f64,
}

pub fn rtl_sdr(a: IqArgs) -> Result<()> {
    if a.filename != "-" {
        bail!("sim-rtl-sdr only writes to stdout (\"-\")");
    }
    let rate = a.rate as f64;
    let t0 = (now_ms() - EPOCH_MS) as f64 / 1000.0;
    let lo_err = a.freq as f64 * a.crystal_ppm * 1e-6;
    let mut streams: Vec<Box<dyn Iterator<Item = C>>> = a
        .txs
        .iter()
        .map(|t| {
            let km = distance_km(t.pos, a.site);
            // fainter further away, as over hilly ground; 0.3 of full scale at 10 km
            let e = Emitter {
                seed: t.hz,
                dev: t.dev,
                amp: (0.3 * (10.0 / km.max(1.0)).powf(1.5)).min(0.45),
            };
            let delay = km / C_KM_S;
            let off = t.hz as f64 - a.freq as f64 - lo_err;
            Box::new(e.stream(t0, rate, delay, a.crystal_ppm, off)) as Box<dyn Iterator<Item = C>>
        })
        .collect();
    let mut noise = (now_ms() as u64) | 1;
    let mut out = std::io::stdout().lock();
    let mut buf = Vec::with_capacity(1 << 16);
    for _ in 0..a.samples {
        let mut x = C::default();
        for s in streams.iter_mut() {
            x = x + s.next().expect("streams are endless");
        }
        noise ^= noise << 13;
        noise ^= noise >> 7;
        noise ^= noise << 17;
        let n = C::new(
            (noise & 0xff) as f64 / 255.0 - 0.5,
            ((noise >> 8) & 0xff) as f64 / 255.0 - 0.5,
        )
        .scale(0.06);
        let q = |v: f64| (v * 127.5 + 127.5).round().clamp(0.0, 255.0) as u8;
        buf.push(q(x.re + n.re));
        buf.push(q(x.im + n.im));
        if buf.len() >= 1 << 15 {
            out.write_all(&buf)?;
            buf.clear();
        }
    }
    out.write_all(&buf)?;
    out.flush()?;
    Ok(())
}

/// What `sim-kraken` serves.
#[derive(Parser)]
#[command(name = "ridgewarden-sdr sim-kraken")]
pub struct KrakenArgs {
    #[arg(long, default_value = "127.0.0.1:8081")]
    listen: std::net::SocketAddr,
    /// where the KrakenSDR is, LAT:LON
    #[arg(long, value_parser = latlon)]
    site: LatLon,
    /// the transmitter it hears
    #[arg(long = "tx", allow_hyphen_values = true)]
    tx: Tx,
    /// which way the array's zero points, degrees true
    #[arg(long, default_value_t = 0.0)]
    heading: f64,
    /// a bearing error to add, degrees
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    error: f64,
}

pub async fn kraken(a: KrakenArgs) -> Result<()> {
    let line = move || {
        let truth = azimuth(a.site, a.tx.pos) + a.error;
        kraken::line(
            now_ms(),
            (truth - a.heading).rem_euclid(360.0),
            a.tx.hz,
            a.site.lat,
            a.site.lon,
            a.heading,
        )
    };
    let app = axum::Router::new().route(
        "/DOA_value.html",
        axum::routing::get(move || async move { line() }),
    );
    let listener = tokio::net::TcpListener::bind(a.listen)
        .await
        .with_context(|| format!("binding {}", a.listen))?;
    axum::serve(listener, app).await?;
    Ok(())
}
