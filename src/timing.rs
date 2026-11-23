//! How good the time differences really are. A weekly beacon test with a reference keeps
//! the pairs of receivers it measured; for each, the delay where the correlation peaked is
//! set against the delay the surveyed spot and the two receivers' positions give. The
//! shared chart draws only time series, so this plot is drawn here with the same styles.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::State;
use axum::response::Html;
use ridgewarden_client::svg::{Chart, Line};
use ridgewarden_client::ui::{Color, pill};
use ridgewarden_client::util::{esc, median, short_time};

use crate::app::App;
use crate::beacon::{self, Outcome};
use crate::geo::{C_KM_S, LatLon};
use crate::tdoa::{PSR_MIN, Pair};
use crate::web::{Fail, page, site};

/// A 1-sigma error to 95%, for one measurement (two-sided normal).
const Z95: f64 = 1.959_963_984_540_054;
const MICRO: &str = "\u{b5}s";
/// The table lists this many of the latest pairs.
const ROWS: usize = 100;
/// The plot shows this many of the latest pairs.
const PLOTTED: usize = 300;
/// Tests it takes before the ratio is steady enough to act on.
const JUDGE_AFTER: usize = 6;

const W: f64 = 640.0;
const H: f64 = 360.0;
const LEFT: f64 = 50.0;
const RIGHT: f64 = 12.0;
const TOP: f64 = 22.0;
const BOTTOM: f64 = 36.0;

fn us(km: f64) -> f64 {
    km / C_KM_S * 1e6
}

/// One pair of receivers in one beacon test, in microseconds.
struct Point<'a> {
    at: i64,
    capture: &'a str,
    /// who published the test's result
    by: &'a str,
    pair: &'a Pair,
    /// the delay the surveyed spot gives
    known: f64,
    /// where the correlation peaked
    measured: f64,
    /// the pair's stated error, 1 sigma
    sigma: f64,
}

impl Point<'_> {
    fn error(&self) -> f64 {
        self.measured - self.known
    }

    fn inside(&self) -> bool {
        self.error().abs() <= Z95 * self.sigma
    }
}

fn test_points<'a>(by: &'a str, o: &'a Outcome) -> impl Iterator<Item = Point<'a>> {
    let spot = LatLon::new(o.spot_lat, o.spot_lon);
    o.pairs.iter().map(move |p| Point {
        at: o.at,
        capture: &o.capture,
        by,
        pair: p,
        known: us(p.expected_km(spot)),
        measured: us(p.dd_km),
        sigma: us(p.sigma_km),
    })
}

/// Every pair from every beacon test, oldest first.
fn points(h: &[(String, Outcome)]) -> Vec<Point<'_>> {
    h.iter().flat_map(|(by, o)| test_points(by, o)).collect()
}

#[derive(Debug)]
struct Summary {
    pairs: usize,
    /// pairs whose error is inside their stated 95% bar
    inside: usize,
    /// root mean square error, microseconds
    rms: f64,
    /// mean error with its sign, microseconds: far from 0 is a steady offset, not scatter
    mean: f64,
    /// root mean square of each error over its stated sigma: 1 when the claims are honest
    ratio: f64,
}

fn summary(pts: &[Point]) -> Option<Summary> {
    if pts.is_empty() {
        return None;
    }
    let n = pts.len() as f64;
    let rms =
        |f: &dyn Fn(&Point) -> f64| (pts.iter().map(|p| f(p).powi(2)).sum::<f64>() / n).sqrt();
    Some(Summary {
        pairs: pts.len(),
        inside: pts.iter().filter(|p| p.inside()).count(),
        rms: rms(&|p| p.error()),
        mean: pts.iter().map(Point::error).sum::<f64>() / n,
        ratio: rms(&|p| p.error() / p.sigma),
    })
}

/// Each test with time differences, oldest first: when it ran and how its pairs did.
fn per_test(h: &[(String, Outcome)]) -> Vec<(i64, Summary)> {
    h.iter()
        .filter_map(|(by, o)| {
            let pts: Vec<Point> = test_points(by, o).collect();
            Some((o.at, summary(&pts)?))
        })
        .collect()
}

/// How many times their stated errors a typical test's pairs were off: the median over
/// tests, so one bad test can't swing it.
fn typical(tests: &[(i64, Summary)]) -> Option<f64> {
    median(&mut tests.iter().map(|(_, s)| s.ratio).collect::<Vec<_>>())
}

/// What the errors say about `tdoa.sigma_us`, the timing error a clean pair claims. It only
/// ever advises raising it: ellipses that are too small mislead, ones too large are only
/// cautious, and lowering it is a call for a person.
fn verdict(tests: &[(i64, Summary)]) -> String {
    if tests.len() < JUDGE_AFTER {
        return format!(
            "Too few beacon tests yet to judge tdoa.sigma_us: {} of the {JUDGE_AFTER} it takes.",
            tests.len()
        );
    }
    let Some(r) = typical(tests).filter(|r| r.is_finite()) else {
        return "Can't judge tdoa.sigma_us: the pairs' stated errors give no usable ratio.".into();
    };
    if r > 1.5 {
        format!(
            "A typical test's errors run {r:.1} times what its pairs claim, so fixes' ellipses \
             are too small: raise tdoa.sigma_us at the solving sites by about that much."
        )
    } else if r < 0.5 {
        format!(
            "A typical test's errors are {r:.1} times what its pairs claim, so the ellipses are \
             on the safe side."
        )
    } else {
        "Errors are about what the pairs claim, so tdoa.sigma_us is about right.".into()
    }
}

/// A round step, 1 or 2 times a power of ten, giving one to five ticks each side of 0.
fn tick_step(max: f64) -> f64 {
    let p = 10f64.powf(max.log10().floor());
    if max / p <= 5.0 { p } else { 2.0 * p }
}

/// Measured against known delay as inline SVG: each pair's point with its 95% bar, and
/// the dashed line where the two agree. The scale reaches just past the 95th percentile, so
/// a few wild pairs don't squash the rest; they are drawn on the edge. `label` names a node.
fn plot(pts: &[Point], label: &dyn Fn(&str) -> String) -> String {
    let title = "Time differences against known delays";
    let mut s = format!(
        "<svg class=chart viewBox=\"0 0 {W} {H}\" role=img aria-label=\"{title}\"><title>{title}</title>"
    );
    let mut size: Vec<f64> = pts
        .iter()
        .map(|p| p.known.abs().max(p.measured.abs()))
        .collect();
    size.sort_by(f64::total_cmp);
    let p95 = size
        .get((size.len() * 95).div_ceil(100).saturating_sub(1))
        .copied()
        .unwrap_or(0.0);
    let max = p95.max(1.0) * 1.05;
    let step = tick_step(max);
    let ticks = (max / step).ceil() as i64;
    let m = ticks as f64 * step;
    let x = |v: f64| LEFT + (v.clamp(-m, m) + m) / (2.0 * m) * (W - LEFT - RIGHT);
    let y = |v: f64| TOP + (m - v.clamp(-m, m)) / (2.0 * m) * (H - TOP - BOTTOM);
    let _ = write!(
        s,
        "<rect class=frame x=\"{LEFT}\" y=\"{TOP}\" width=\"{}\" height=\"{}\"/>",
        W - LEFT - RIGHT,
        H - TOP - BOTTOM
    );
    for k in -ticks..=ticks {
        let v = k as f64 * step;
        let _ = write!(
            s,
            "<text class=axis x=\"{:.1}\" y=\"{}\" text-anchor=middle>{v:.0}</text>\
             <text class=axis x=\"{}\" y=\"{:.1}\" text-anchor=end>{v:.0}</text>",
            x(v),
            H - BOTTOM + 14.0,
            LEFT - 4.0,
            y(v) + 4.0
        );
    }
    let _ = write!(
        s,
        "<text class=axis x=\"{}\" y=\"{}\" text-anchor=middle>known delay, {MICRO}</text>\
         <text class=axis x=\"{LEFT}\" y=\"14\">measured, {MICRO}</text>\
         <line class=s0 stroke-dasharray=\"4 3\" x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\">\
         <title>measured = known</title></line>",
        (LEFT + W - RIGHT) / 2.0,
        H - 4.0,
        x(-m),
        y(-m),
        x(m),
        y(m)
    );
    for p in pts {
        let c = if p.inside() { 1 } else { 2 };
        let bar = Z95 * p.sigma;
        let edge = if p.known.abs() > m || p.measured.abs() > m {
            "; off the scale, drawn on its edge"
        } else {
            ""
        };
        let tip = format!(
            "{}, {} {} to {} {}: known {:+.2} {MICRO}, measured {:+.2} {MICRO}, off by {:+.2} {MICRO}; \
             stated 95% error {bar:.2} {MICRO}; peak {:.1}{edge}",
            short_time(p.at),
            label(&p.pair.a.node),
            p.pair.a.receiver,
            label(&p.pair.b.node),
            p.pair.b.receiver,
            p.known,
            p.measured,
            p.error(),
            p.pair.psr_target
        );
        let _ = write!(
            s,
            "<line class=s{c} stroke-width=\"1.5\" x1=\"{cx:.1}\" x2=\"{cx:.1}\" y1=\"{:.1}\" y2=\"{:.1}\"/>\
             <circle class=d{c} cx=\"{cx:.1}\" cy=\"{:.1}\" r=\"3.5\"><title>{}</title></circle>",
            y(p.measured + bar),
            y(p.measured - bar),
            y(p.measured),
            esc(&tip),
            cx = x(p.known)
        );
    }
    s.push_str("</svg>");
    s
}

fn cards(s: &Summary, tests: &[(i64, Summary)]) -> String {
    let ratio = typical(tests)
        .filter(|r| r.is_finite())
        .map_or_else(|| "?".to_string(), |r| format!("{r:.1}"));
    format!(
        "<div class=cards><div class=card><div class=n>{}</div>pairs from {} test{}</div>\
         <div class=card><div class=n>{}/{}</div>inside their 95% bar</div>\
         <div class=card><div class=n>{:.2} {MICRO}</div>root mean square error, {:.0} m</div>\
         <div class=card><div class=n>{:+.2} {MICRO}</div>mean error; a steady offset shows here</div>\
         <div class=card><div class=n>{ratio}</div>times the stated error, typical test</div></div>",
        s.pairs,
        tests.len(),
        if tests.len() == 1 { "" } else { "s" },
        s.inside,
        s.pairs,
        s.rms,
        s.rms * C_KM_S * 1e-3,
        s.mean
    )
}

fn table(app: &App, pts: &[Point]) -> String {
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Test</th><th>Pair</th><th>Known</th><th>Measured</th>\
         <th>Off by</th><th>95% error</th><th>Peak</th><th>Published by</th></tr>",
    );
    for p in pts.iter().rev().take(ROWS) {
        let off = format!(
            "{:+.2} {MICRO} ({:.0} m)",
            p.error(),
            p.error().abs() * C_KM_S * 1e-3
        );
        let _ = write!(
            b,
            "<tr><td><a href=\"/capture?id={}\">{}</a></td><td>{} {}<br>\
             <span class=\"mute small\">to {} {}</span></td><td>{:+.2} {MICRO}</td>\
             <td>{:+.2} {MICRO}</td><td>{}{}</td><td>&plusmn;{:.2} {MICRO}</td><td>{:.1}</td>\
             <td>{}</td></tr>",
            esc(p.capture),
            esc(&short_time(p.at)),
            site(app, &p.pair.a.node),
            esc(&p.pair.a.receiver),
            site(app, &p.pair.b.node),
            esc(&p.pair.b.receiver),
            p.known,
            p.measured,
            esc(&off),
            if p.inside() {
                String::new()
            } else {
                format!(" {}", pill(Color::Yellow, "outside its bar"))
            },
            Z95 * p.sigma,
            p.pair.psr_target,
            esc(p.by)
        );
    }
    let _ = write!(
        b,
        "</table></div><p class=\"mute small\">Peak is how far the target's correlation peak \
         stands above the spread of the rest of the search window (PSR); below {PSR_MIN} a \
         pair is refused."
    );
    if pts.len() > ROWS {
        let _ = write!(
            b,
            " The table has the latest {ROWS} of {} pairs, the plot the latest {}.",
            pts.len(),
            pts.len().min(PLOTTED)
        );
    }
    b.push_str("</p>");
    b
}

pub async fn timing_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let h = beacon::history(&app.db())?;
    let pts = points(&h);
    let tests = per_test(&h);
    let mut b = String::from(
        "<h1>Timing against known delays</h1><p class=\"mute small\">Each point is one pair of \
         receivers in a weekly beacon test. The delay is how much later the beacon reaches the \
         second receiver than the first. Across is the delay the surveyed spot and the two \
         receivers' positions give; up is where the correlation peaked, after the clocks were \
         lined up on the reference transmitter. On the dashed line the two agree. Each bar is \
         the pair's stated 95% error; a point whose bar misses the line is drawn orange.</p>",
    );
    match summary(&pts) {
        Some(s) => {
            b.push_str(&cards(&s, &tests));
            let _ = write!(b, "<p>{}</p>", esc(&verdict(&tests)));
            let latest = &pts[pts.len().saturating_sub(PLOTTED)..];
            b.push_str(&plot(latest, &|n| app.cfg.label(n)));
        }
        None => b.push_str(
            "<p class=mute>No beacon test has time differences yet. They need the [beacon] \
             section to name a reference, receivers with an iq_command, and a site with \
             tdoa.solve.</p>",
        ),
    }
    if let (Some((from, _)), Some((to, _))) = (tests.first(), tests.last()) {
        let chart = Chart {
            from: *from,
            to: (*to).max(from.saturating_add(1)),
            unit: MICRO.into(),
            lines: vec![Line {
                label: "root mean square error".into(),
                points: tests.iter().map(|(at, s)| (*at, s.rms)).collect(),
            }],
            rule: Some((app.cfg.tdoa.sigma_us, "this site's tdoa.sigma_us".into())),
            ..Default::default()
        };
        let _ = write!(
            b,
            "<h2>Timing error per test</h2><p class=\"mute small\">Each point is one test's root \
             mean square timing error. The red dashed line is this site's tdoa.sigma_us, {} \
             {MICRO}, the 1-sigma error a clean pair claims; tests well above it were off by \
             more than their pairs claimed.</p>",
            app.cfg.tdoa.sigma_us
        );
        b.push_str(&chart.render("Timing error per beacon test"));
        b.push_str(&table(&app, &pts));
    }
    b.push_str("<p class=\"mute small\"><a href=\"/accuracy\">All beacon tests</a></p>");
    Ok(page(&app, "/accuracy", "Timing", &b, false).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::offset;
    use crate::tdoa::End;

    fn end(i: usize, p: LatLon) -> End {
        End {
            key: format!("k{i}"),
            receiver: "rx1".into(),
            node: format!("tst-{i}"),
            lat: p.lat,
            lon: p.lon,
        }
    }

    fn pair(a: LatLon, b: LatLon, dd_km: f64, sigma_km: f64) -> Pair {
        Pair {
            a: end(0, a),
            b: end(1, b),
            dd_km,
            sigma_km,
            psr_target: 14.0,
            psr_ref: 30.0,
        }
    }

    fn outcome(spot: LatLon, pairs: Vec<Pair>) -> Outcome {
        Outcome {
            capture: "c".repeat(64),
            name: "TST beacon".into(),
            at: 1_795_000_000_000,
            spot_lat: spot.lat,
            spot_lon: spot.lon,
            pairs,
            ..Default::default()
        }
    }

    fn pt(known: f64, measured: f64, sigma: f64, p: &Pair) -> Point<'_> {
        Point {
            at: 1_795_000_000_000,
            capture: "c",
            by: "N0AA",
            pair: p,
            known,
            measured,
            sigma,
        }
    }

    #[test]
    fn the_known_delay_comes_from_the_surveyed_spot() {
        let c = LatLon::new(35.4, -120.6);
        // 10 km nearer the second receiver than the first: it hears the beacon 33.36 us sooner
        let (a, b, spot) = (
            offset(c, -10.0, 0.0),
            offset(c, 10.0, 0.0),
            offset(c, 5.0, 0.0),
        );
        let h = vec![(
            "N0AA".to_string(),
            outcome(spot, vec![pair(a, b, -10.3, 0.9)]),
        )];
        let p = &points(&h)[0];
        assert!((p.known - -33.356).abs() < 0.01, "{}", p.known);
        assert!((p.measured - -34.357).abs() < 0.01, "{}", p.measured);
        assert!((p.sigma - 3.002).abs() < 0.01, "{}", p.sigma);
        assert_eq!(p.by, "N0AA");
    }

    #[test]
    fn summary_compares_errors_with_their_stated_size() {
        let c = LatLon::new(35.4, -120.6);
        let p = pair(c, offset(c, 10.0, 0.0), 0.0, 1.0);
        // errors 1, -1 and 3 us against sigmas 1, 1 and 0.5 us
        let pts = [
            pt(5.0, 6.0, 1.0, &p),
            pt(-2.0, -3.0, 1.0, &p),
            pt(10.0, 13.0, 0.5, &p),
        ];
        let s = summary(&pts).unwrap();
        assert_eq!((s.pairs, s.inside), (3, 2));
        assert!((s.rms - (11.0f64 / 3.0).sqrt()).abs() < 1e-9, "{}", s.rms);
        assert!((s.mean - 1.0).abs() < 1e-9, "the sign is kept: {}", s.mean);
        assert!(
            (s.ratio - (38.0f64 / 3.0).sqrt()).abs() < 1e-9,
            "{}",
            s.ratio
        );
        assert!(summary(&[]).is_none());
    }

    #[test]
    fn the_95_percent_bar_is_1_96_sigma() {
        let c = LatLon::new(35.4, -120.6);
        let p = pair(c, offset(c, 10.0, 0.0), 0.0, 1.0);
        assert!(pt(0.0, 1.959, 1.0, &p).inside());
        assert!(pt(0.0, -1.959, 1.0, &p).inside());
        assert!(pt(0.0, Z95, 1.0, &p).inside(), "the edge itself is inside");
        assert!(!pt(0.0, 1.961, 1.0, &p).inside());
        assert!(
            pt(0.0, -3.91, 2.0, &p).inside(),
            "the bar scales with sigma"
        );
        assert!(!pt(0.0, -3.93, 2.0, &p).inside());
    }

    #[test]
    fn the_verdict_follows_the_typical_test() {
        let verdict_of = |ratios: &[f64]| {
            let tests: Vec<(i64, Summary)> = ratios
                .iter()
                .map(|&ratio| {
                    let s = Summary {
                        pairs: 3,
                        inside: 3,
                        rms: 1.0,
                        mean: 0.0,
                        ratio,
                    };
                    (0, s)
                })
                .collect();
            verdict(&tests)
        };
        assert!(verdict_of(&[3.0; 5]).starts_with("Too few"), "five tests");
        let raise = verdict_of(&[2.4; 6]);
        assert!(raise.contains("raise tdoa.sigma_us"), "{raise}");
        assert!(raise.contains("2.4 times"), "{raise}");
        assert!(
            verdict_of(&[1.0, 1.1, 0.9, 1.2, 1.0, 40.0]).contains("about right"),
            "one wild test doesn't swing it"
        );
        assert!(verdict_of(&[1.5; 6]).contains("about right"));
        assert!(verdict_of(&[1.51; 6]).contains("raise tdoa.sigma_us"));
        assert!(verdict_of(&[0.5; 6]).contains("about right"));
        let low = verdict_of(&[0.49; 6]);
        assert!(low.contains("safe side"), "{low}");
        assert!(!low.contains("lower") && !low.contains("down"), "{low}");
        assert!(verdict_of(&[f64::NAN; 6]).starts_with("Can't judge"));
    }

    /// The first `name="..."` number in `s`.
    fn attr(s: &str, name: &str) -> f64 {
        let v = s.split(&format!("{name}=\"")).nth(1).unwrap();
        v.split('"').next().unwrap().parse().unwrap()
    }

    /// Each dot's class, x and y.
    fn circles(svg: &str) -> Vec<(String, f64, f64)> {
        svg.split("<circle class=")
            .skip(1)
            .map(|c| (c[..2].to_string(), attr(c, "cx"), attr(c, "cy")))
            .collect()
    }

    #[test]
    fn the_plot_puts_each_peak_against_its_known_delay() {
        let c = LatLon::new(35.4, -120.6);
        let p = pair(c, offset(c, 10.0, 0.0), 0.0, 1.0);
        let pts = [
            pt(20.0, 20.0, 1.0, &p),
            pt(-20.0, 0.0, 1.0, &p),
            pt(-40.0, -38.0, 2.0, &p),
        ];
        let svg = plot(&pts, &|n| format!("<{n}>"));
        assert!(!svg.contains("<tst-0>"), "labels are escaped");
        let dots = circles(&svg);
        assert_eq!(dots.len(), 3);
        // pixels from the dashed line where measured equals known
        let diag = svg.split("stroke-dasharray").nth(1).unwrap();
        let [x1, y1, x2, y2] = ["x1", "y1", "x2", "y2"].map(|a| attr(diag, a));
        let off_line = |x: f64, y: f64| {
            ((x2 - x1) * (y - y1) - (y2 - y1) * (x - x1)).abs() / (x2 - x1).hypot(y2 - y1)
        };
        let (on, off, near) = (&dots[0], &dots[1], &dots[2]);
        assert!(off_line(on.1, on.2) < 0.2, "{on:?}");
        assert!(off_line(off.1, off.2) > 20.0, "{off:?}");
        assert!(off.1 < on.1, "a smaller known delay sits further left");
        assert!(off.2 > on.2, "a smaller measured delay sits lower");
        let line_at = |x: f64| y1 + (x - x1) * (y2 - y1) / (x2 - x1);
        assert!(
            off.2 < line_at(off.1),
            "measured later than known sits above the line"
        );
        assert_eq!(on.0, "d1");
        assert_eq!(off.0, "d2", "20 us off with a 2 us bar misses the line");
        assert_eq!(near.0, "d1", "2 us off with a 3.9 us bar reaches it");
    }

    #[test]
    fn a_few_wild_pairs_dont_set_the_scale() {
        let c = LatLon::new(35.4, -120.6);
        let p = pair(c, offset(c, 10.0, 0.0), 0.0, 1.0);
        let mut pts: Vec<Point> = (0..19)
            .map(|i| pt(4.0 - i as f64 * 0.1, 4.0, 1.0, &p))
            .collect();
        pts.push(pt(500.0, 480.0, 1.0, &p));
        let svg = plot(&pts, &|n| n.to_string());
        let top = svg
            .split("text-anchor=end>")
            .skip(1)
            .map(|t| t.split('<').next().unwrap().parse::<f64>().unwrap())
            .fold(f64::MIN, f64::max);
        assert_eq!(top, 5.0, "the scale follows the 19 pairs near 4 us");
        let (_, x, y) = circles(&svg)[19].clone();
        assert!(
            (x - (W - RIGHT)).abs() < 0.1 && (y - TOP).abs() < 0.1,
            "the wild pair sits in the frame's corner: {x} {y}"
        );
        assert!(svg.contains("off the scale"));
    }

    #[test]
    fn each_test_gets_its_own_error() {
        let c = LatLon::new(35.4, -120.6);
        let (a, b) = (offset(c, -10.0, 0.0), offset(c, 10.0, 0.0));
        // a transmitter at the midpoint arrives at both at once: 0 delay
        let exact = outcome(c, vec![pair(a, b, 0.0, 0.9)]);
        let mut off = outcome(c, vec![pair(a, b, 0.3, 0.9)]);
        off.at += 7 * 86_400_000;
        let none = outcome(c, vec![]);
        let h: Vec<(String, Outcome)> = [exact, none, off]
            .into_iter()
            .map(|o| ("N0AA".to_string(), o))
            .collect();
        let series = per_test(&h);
        assert_eq!(
            series.len(),
            2,
            "a test without time differences is left out"
        );
        assert!(series[0].1.rms.abs() < 1e-6, "{series:?}");
        assert!(
            (series[1].1.rms - 1.0007).abs() < 1e-3,
            "300 m is 1 us: {series:?}"
        );
        assert!(
            (series[1].1.mean - 1.0007).abs() < 1e-3,
            "measured late, so positive: {series:?}"
        );
    }
}
