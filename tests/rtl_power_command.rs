//! The capture path end to end through a real child process: arguments built for
//! rtl_power, the simulator standing in for it, and its CSV read back.

use std::time::Duration;

use ridgewarden_sdr::rtlpower::{args, level, parse, plan, run};

fn sim(extra: &[&str]) -> Vec<String> {
    let mut c = vec![
        env!("CARGO_BIN_EXE_ridgewarden-sdr").to_string(),
        "sim-rtl-power".into(),
    ];
    c.extend(extra.iter().map(|s| s.to_string()));
    c
}

#[tokio::test]
async fn simulated_rtl_power_takes_the_real_arguments() {
    let p = plan(146_520_000, 16_000);
    let a = args(&p, 1, Some(40.0), Some(-2));
    let loud = run(
        &sim(&["--signal", "146.52M:-20:8k"]),
        &a,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    let bins = parse(&loud).unwrap();
    let l = level(&bins, 146_512_000, 146_528_000, |_| true).unwrap();
    // gain 40 is 10 dB over the simulator's reference: everything moves up, the gap doesn't
    assert!((l.floor_db.unwrap() - -35.0).abs() < 0.6, "{l:?}");
    assert!(l.snr_db.unwrap() > 15.0, "{l:?}");
    let quiet = run(&sim(&[]), &a, Duration::from_secs(30)).await.unwrap();
    let q = level(&parse(&quiet).unwrap(), 146_512_000, 146_528_000, |_| true).unwrap();
    assert!(q.snr_db.unwrap() < 1.0, "{q:?}");
}

#[tokio::test]
async fn failures_say_what_went_wrong() {
    let a = args(&plan(146_520_000, 16_000), 1, None, None);
    let e = run(
        &["no-such-rtl-power".to_string()],
        &a,
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{e:#}").contains("starting no-such-rtl-power"),
        "{e:#}"
    );
    let e = run(&sim(&[]), &a, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("did not finish"), "{e}");
    let e = run(&sim(&["--signal", "nonsense"]), &a, Duration::from_secs(30))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("failed"), "{e}");
}
