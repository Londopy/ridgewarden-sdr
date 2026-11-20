//! Time differences end to end through real child processes: three simulated sites run
//! the rtl_sdr stand-in at the same moment, each recording is cut down to a snippet as a
//! site would publish it, and the snippets give back the transmitter's position.

use std::time::Duration;

use ridgewarden_sdr::fix::{Obs, locate};
use ridgewarden_sdr::geo::{LatLon, distance_km, offset};
use ridgewarden_sdr::iq::{self, Snippet};
use ridgewarden_sdr::tdoa::{End, Opts, analyze};

const TARGET: u64 = 146_520_000;
const REFERENCE: u64 = 147_000_000;

async fn record(site: LatLon, tx: LatLon, rf: LatLon, crystal_ppm: f64) -> Snippet {
    let center = iq::center(TARGET, REFERENCE);
    let cmd = vec![
        env!("CARGO_BIN_EXE_ridgewarden-sdr").to_string(),
        "sim-rtl-sdr".into(),
    ];
    // about half a second: enough to correlate, short enough for a debug build
    let args: Vec<String> = [
        "-f".to_string(),
        center.to_string(),
        "-s".into(),
        "2048000".into(),
        "-n".into(),
        "1100000".into(),
        "--site".into(),
        format!("{}:{}", site.lat, site.lon),
        "--tx".into(),
        format!("{TARGET}:{}:{}", tx.lat, tx.lon),
        "--tx".into(),
        format!("{REFERENCE}:{}:{}", rf.lat, rf.lon),
        "--crystal-ppm".into(),
        crystal_ppm.to_string(),
        "-".into(),
    ]
    .into();
    let raw = iq::record(&cmd, &args, Duration::from_secs(120))
        .await
        .unwrap();
    assert_eq!(raw.bytes.len(), 2_200_000);
    let s = iq::snippet(
        &raw,
        "c",
        "rx1",
        center,
        (TARGET, 16_000),
        (REFERENCE, 16_000),
    );
    assert!(s.header.clipped < 0.001, "{}", s.header.clipped);
    // through the shared format and back, as the solving site would get it
    iq::decode(&iq::encode(&s)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn three_sites_locate_the_transmitter() {
    let c = LatLon::new(35.4, -120.6);
    let sites = [
        offset(c, -14.0, -9.0),
        offset(c, 13.0, -11.0),
        offset(c, 1.0, 15.0),
    ];
    let tx = offset(c, 3.0, 2.0);
    let rf = offset(c, -6.0, 20.0);
    let (a, b, d) = tokio::join!(
        record(sites[0], tx, rf, 0.0),
        record(sites[1], tx, rf, 20.0),
        record(sites[2], tx, rf, -15.0)
    );
    let snips: Vec<(End, Snippet)> = [a, b, d]
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            let e = End {
                key: format!("k{i}"),
                receiver: "rx1".into(),
                node: format!("tst-{i}"),
                lat: sites[i].lat,
                lon: sites[i].lon,
            };
            (e, s)
        })
        .collect();
    let opts = Opts {
        max_skew_s: 0.25,
        sigma_s: 3e-6,
    };
    let (pairs, problems) = analyze(&snips, rf, &opts);
    assert!(problems.is_empty(), "{problems:?}");
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
    let miss = distance_km(f.pos, tx);
    assert!(miss < 1.0, "missed by {miss} km");
    assert!(
        f.ellipse.contains(f.pos, tx) || miss < 0.5,
        "{:?}",
        f.ellipse
    );
}
