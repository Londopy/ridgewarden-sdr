//! Running captures at this site: every local receiver and KrakenSDR that can take part
//! waits for the start time, measures, and logs what it found through the core.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use ridgewarden_client::util::now_ms;

use crate::app::App;
use crate::bands::band_of;
use crate::capture::{self, Cap, GRACE_MS, Reading, RefTx, UNIT, UNIT_IQ, measure_csv};
use crate::config::ReceiverCfg;
use crate::directory;
use crate::dsp::psd;
use crate::iq;
use crate::kraken;
use crate::rtlpower::{self, Bin, Level};

/// How far past a channel's edge its filter still passes noticeable signal.
const SKIRT: u64 = 10_000;

/// Sleep until wall-clock time `at_ms`, so every site starts together.
pub async fn sleep_until(at_ms: i64) {
    let wait = at_ms - now_ms();
    if wait > 0 {
        tokio::time::sleep(Duration::from_millis(wait as u64)).await;
    }
}

/// Start every capture that is due on each local receiver and KrakenSDR that hasn't run it.
/// `home` is this station's node position, for receivers and KrakenSDRs without their own.
pub fn round(app: &Arc<App>, node: &str, home: Option<(f64, f64)>) -> Result<usize> {
    let now = now_ms();
    let due = capture::due(&app.db(), now)?;
    let mut started = 0;
    for cap in due {
        let (lo, hi) = (cap.req.lo(), cap.req.hi());
        for rx in &app.cfg.receivers {
            let covers = rx.min_hz() <= lo && hi <= rx.max_hz();
            if rx.capture.is_none() || !covers || !app.claim_run(&cap.id, &rx.name, now)? {
                continue;
            }
            started += 1;
            let (app, cap, rx, node) = (app.clone(), cap.clone(), rx.clone(), node.to_string());
            tokio::spawn(async move {
                let reading = run_receiver(&app, &cap, &rx, &node, home).await;
                tracing::info!(
                    "capture {} on {}: {} {}",
                    &cap.id[..12],
                    rx.name,
                    reading.status,
                    reading.detail
                );
                if reading.status == "ok"
                    && let Err(e) = app.ran_ok(&cap.id, &rx.name)
                {
                    tracing::warn!("recording capture {}: {e:#}", &cap.id[..12]);
                }
                let payload = serde_json::to_value(&reading).expect("reading serializes");
                if let Err(e) = app.core.append_telemetry("sdr.reading", payload).await {
                    tracing::warn!("logging reading for {}: {e:#}", &cap.id[..12]);
                }
            });
        }
        for k in &app.cfg.krakens {
            if !app.claim_run(&cap.id, &format!("kraken:{}", k.name), now)? {
                continue;
            }
            started += 1;
            let (app, cap, k, node) = (app.clone(), cap.clone(), k.clone(), node.to_string());
            tokio::spawn(async move {
                let b = kraken::collect(app.clone(), cap.clone(), k.clone(), node, home).await;
                tracing::info!(
                    "capture {} on KrakenSDR {}: {} {}",
                    &cap.id[..12],
                    k.name,
                    b.status,
                    b.detail
                );
                let payload = serde_json::to_value(&b).expect("bearing serializes");
                if let Err(e) = app.core.append_telemetry("sdr.bearing", payload).await {
                    tracing::warn!("logging bearing for {}: {e:#}", &cap.id[..12]);
                }
            });
        }
    }
    Ok(started)
}

/// Run one capture on one local receiver and say how it went.
async fn run_receiver(
    app: &Arc<App>,
    cap: &Cap,
    rx: &ReceiverCfg,
    node: &str,
    home: Option<(f64, f64)>,
) -> Reading {
    let (lat, lon, _) = directory::position(rx, home);
    let rc = rx
        .capture
        .as_ref()
        .expect("only capture receivers run captures");
    let mut out = Reading {
        capture: cap.id.clone(),
        receiver: rx.name.clone(),
        node: node.to_string(),
        lat,
        lon,
        gain_db: rc.gain_db,
        calibration_db: rc.calibration_db,
        unit: UNIT.into(),
        ..Default::default()
    };
    let req = &cap.req;
    if let Err(why) = app.cfg.may_measure(req.lo(), req.hi()) {
        out.status = "refused".into();
        out.detail = why;
        return out;
    }
    // record IQ for time differences when asked and able; otherwise just measure strength.
    // A snippet is a recording, so both channels must be amateur whatever local_bands says,
    // with room for the channel filter's skirts
    let clear = |lo: u64, hi: u64| band_of(lo.saturating_sub(SKIRT), hi + SKIRT).is_some();
    let reference = req.reference.as_ref().filter(|r| {
        rc.iq_command.is_some()
            && clear(req.lo(), req.hi())
            && clear(r.lo(), r.hi())
            && rx.min_hz() <= r.lo()
            && r.hi() <= rx.max_hz()
            && app.cfg.may_measure(r.lo(), r.hi()).is_ok()
    });
    sleep_until(req.at_ms()).await;
    let lock = app.lock(&rx.name);
    let wait = Duration::from_millis((req.at_ms() + GRACE_MS - now_ms()).max(0) as u64);
    let Ok(_guard) = tokio::time::timeout(wait, lock.lock()).await else {
        out.status = "failed".into();
        out.detail =
            "the receiver was busy with another capture until it was too late to start".into();
        return out;
    };
    let result = match (reference, lat.zip(lon)) {
        (Some(r), Some(pos)) => {
            out.unit = UNIT_IQ.into();
            // calibration was measured with rtl_power, on its scale
            out.calibration_db = None;
            record_iq(app, cap, rx, r, node, pos).await
        }
        _ => run_rtl_power(app, cap, rx).await,
    };
    match result {
        Ok(l) => {
            out.status = "ok".into();
            out.level_db = Some(round2(l.level_db));
            out.peak_db = Some(round2(l.peak_db));
            out.floor_db = l.floor_db.map(round2);
            out.snr_db = l.snr_db.map(round2);
            out.bins = Some(l.bins as u32);
        }
        Err(why) => {
            out.status = "failed".into();
            out.detail = why.chars().filter(|c| !c.is_control()).take(300).collect();
        }
    }
    out
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

async fn run_rtl_power(app: &App, cap: &Cap, rx: &ReceiverCfg) -> Result<Level, String> {
    let rc = rx.capture.as_ref().expect("checked by the caller");
    let req = &cap.req;
    let plan = rtlpower::plan(req.freq, req.bw);
    let args = rtlpower::args(&plan, req.secs, rc.gain_db, rc.ppm);
    let timeout = Duration::from_secs(req.secs as u64 + 60);
    let csv = rtlpower::run(&rc.command, &args, timeout)
        .await
        .map_err(|e| format!("{e:#}"))?;
    measure_csv(&app.cfg, req, &csv)
}

/// Record 2 s of IQ around the target and the reference, measure the target's level from
/// it, and share the cut-down snippet with the other sites.
async fn record_iq(
    app: &Arc<App>,
    cap: &Cap,
    rx: &ReceiverCfg,
    r: &RefTx,
    node: &str,
    pos: (f64, f64),
) -> Result<Level, String> {
    let rc = rx.capture.as_ref().expect("checked by the caller");
    let cmd = rc.iq_command.as_ref().expect("checked by the caller");
    let req = &cap.req;
    let center = iq::center(req.freq, r.freq);
    let args = iq::args(center, rc.gain_db, rc.ppm);
    let raw = iq::record(cmd, &args, Duration::from_secs(60))
        .await
        .map_err(|e| format!("{e:#}"))?;
    let (app2, cap2, name, rr) = (app.clone(), cap.clone(), rx.name.clone(), r.clone());
    let worked = tokio::task::spawn_blocking(move || {
        let level = level_from_iq(&app2, &cap2, &raw, center);
        let snip = iq::snippet(
            &raw,
            &cap2.id,
            &name,
            center,
            (cap2.req.freq, cap2.req.bw),
            (rr.freq, rr.bw),
        );
        (level, iq::encode(&snip), snip.header.clipped)
    })
    .await
    .map_err(|e| format!("processing the recording stopped: {e}"))?;
    let (level, bytes, clipped) = worked;
    if let Err(e) = share(app, cap, &rx.name, node, pos, &bytes, clipped).await {
        tracing::warn!("sharing the IQ snippet for {}: {e:#}", &cap.id[..12]);
    }
    level
}

/// Signal level from the recording's own spectrum, with the same band and floor rules as
/// rtl_power's bins but on its own dB scale.
fn level_from_iq(app: &App, cap: &Cap, raw: &iq::Raw, center: u64) -> Result<Level, String> {
    const NFFT: usize = 4096;
    let skip = ((iq::SETTLE_S * iq::FS) as usize * 2).min(raw.bytes.len());
    let p = psd(&raw.bytes[skip..], NFFT);
    let step = iq::FS / NFFT as f64;
    let bins: Vec<Bin> = p
        .iter()
        .enumerate()
        .filter(|(_, v)| **v > 0.0)
        .map(|(k, v)| Bin {
            hz: center as f64 - iq::FS / 2.0 + k as f64 * step,
            db: 10.0 * v.log10(),
            step,
        })
        .collect();
    rtlpower::level(&bins, cap.req.lo(), cap.req.hi(), |f| {
        app.cfg.may_measure_at(f)
    })
    .ok_or_else(|| "the recording had no bins inside the requested band".into())
}

/// Hand a snippet to the core as a blob and tell the other sites where it is.
async fn share(
    app: &App,
    cap: &Cap,
    receiver: &str,
    node: &str,
    pos: (f64, f64),
    bytes: &[u8],
    clipped: f64,
) -> Result<()> {
    let file = format!("sdr-{}-{receiver}.iq", &cap.id[..12]);
    let dir = &app.cfg.core.imports_dir;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(&file);
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    let name = format!("ridgewarden-sdr/iq/{}/{node}-{receiver}", &cap.id[..12]);
    let added = app.core.blob_add(&file, &name, 3).await;
    // the core has its own copy once it has chunked the file
    if let Err(e) = std::fs::remove_file(&path) {
        tracing::warn!("removing {}: {e}", path.display());
    }
    let root = added?;
    let key = app.whoami().await?.key;
    app.keep_snippet(&cap.id, &key, receiver, &root, bytes)?;
    let note = json!({
        "capture": cap.id, "receiver": receiver, "node": node, "lat": pos.0, "lon": pos.1,
        "root": root, "clipped": clipped,
    });
    app.core.append_telemetry("sdr.snippet", note).await?;
    Ok(())
}
