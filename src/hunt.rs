//! Interference hunting pages: asking for a capture, each capture's readings, bearings,
//! time differences and combined fix, and the accuracy record from the weekly beacon test.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Query, State};
use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Form, Json};
use ridgewarden_client::svg::{Chart, Line};
use ridgewarden_client::ui::{Color, pill};
use ridgewarden_client::util::{esc, human_duration, now_ms, short_time};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::App;
use crate::bands::parse_hz;
use crate::beacon;
use crate::capture::{self, Cap, MAX_SECS, Ranking, RefTx, Request, Row, khz, parse_when, rank};
use crate::directory::{self, Listed};
use crate::fix::Fix;
use crate::geo::{LatLon, destination, distance_km};
use crate::kraken::{self, Bearing};
use crate::locate::{self, Evidence};
use crate::map::{self, Pt, Shape};
use crate::tdoa;
use crate::web::{Fail, labelled, page, site};

/// Gap between capture form posts from one address, so the passphrase can't be guessed fast.
const POST_GAP: Duration = Duration::from_secs(10);
/// Wrong keys from anywhere within this window...
const FAIL_WINDOW: Duration = Duration::from_secs(600);
/// ...beyond which the form stops checking keys until the window clears.
const MAX_FAILURES: usize = 20;
/// Captures in the list that get their fix worked out; older ones show it on their page.
const FIXES_LISTED: usize = 30;
/// A fix this tight has found the neighborhood.
const NEIGHBORHOOD_KM: f64 = 1.0;

#[derive(Deserialize, Default)]
pub struct CaptureForm {
    #[serde(default)]
    freq: String,
    #[serde(default)]
    bw: String,
    #[serde(default)]
    secs: String,
    #[serde(default)]
    when: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    reference: String,
    #[serde(default)]
    key: String,
}

fn form(app: &App, f: &CaptureForm, error: Option<&str>) -> String {
    let mut b = String::from("<h2>Request a capture</h2>");
    if app.cfg.capture.form_key.is_empty() {
        b.push_str(
            "<p class=mute>The capture form is off on this site: set <code>capture.form_key</code> \
             in sdr.toml, or use <code>ridgewarden-sdr measure</code> on the box.</p>",
        );
        return b;
    }
    if let Some(e) = error {
        let _ = write!(b, "<div class=banner>{}</div>", esc(e));
    }
    let v = |s: &str, d: &str| esc(if s.is_empty() { d } else { s });
    let mut refs =
        String::from("<option value=\"\">none: signal strength and bearings only</option>");
    for r in &app.cfg.references {
        let _ = write!(
            refs,
            "<option{}>{}</option>",
            if f.reference == r.name {
                " selected"
            } else {
                ""
            },
            esc(&r.name)
        );
    }
    let _ = write!(
        b,
        "<form method=post action=\"/captures\"><p>\
         <label>Frequency <input name=freq value=\"{}\" placeholder=\"146.52M\" required size=12></label> \
         <label>Bandwidth <input name=bw value=\"{}\" required size=8></label> \
         <label>Seconds <input name=secs type=number min=1 max={MAX_SECS} value=\"{}\" required></label></p>\
         <p><label>Start <input name=when value=\"{}\" required size=24></label> \
         <span class=\"mute small\">minutes from now, or a UTC time like 2026-11-20T18:00:00Z</span></p>\
         <p><label>Reference for time differences <select name=reference>{refs}</select></label></p>\
         <p><label>Note <input name=note value=\"{}\" maxlength=200 size=40></label></p>\
         <p><label>Form key <input name=key type=password required autocomplete=off></label> \
         <button type=submit>Request</button></p></form>\
         <p class=\"mute small\">The request is signed by this site's core under its callsign and \
         goes to every site. Leave a few minutes: sites sync every couple of minutes, and one that \
         gets the request after its start time skips it. The form key keeps out casual misuse; it \
         crosses the mesh in the clear like everything on amateur RF, so it isn't a password.</p>",
        v(&f.freq, ""),
        v(&f.bw, "16k"),
        v(&f.secs, "10"),
        v(&f.when, "10"),
        v(&f.note, ""),
    );
    b
}

fn loudest(app: &App, r: &Ranking) -> Option<String> {
    let (row, v) = r.ranked.first()?;
    Some(format!(
        "{} {} ({:.1} {})",
        site(app, &row.r.node),
        esc(&row.r.receiver),
        v,
        r.scale.unit()
    ))
}

fn when_text(cap: &Cap, now: i64) -> String {
    let at = cap.req.at_ms();
    if at > now {
        format!("{} (in {})", short_time(at), human_duration(at - now))
    } else {
        short_time(at)
    }
}

fn fix_cell(f: &Option<Result<Fix, String>>) -> String {
    let Some(f) = f else {
        return "<span class=mute>see the capture</span>".into();
    };
    match f {
        Ok(f) if f.ellipse.major_km <= NEIGHBORHOOD_KM => {
            format!(
                "{} {:.1} km",
                pill(Color::Green, "neighborhood"),
                f.ellipse.major_km
            )
        }
        Ok(f) => format!("within {:.1} km", f.ellipse.major_km),
        Err(_) => "<span class=mute>none</span>".into(),
    }
}

async fn render_captures(
    app: &App,
    f: &CaptureForm,
    error: Option<&str>,
) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let caps = capture::recent(&app.db(), now)?;
    let mut b = String::from(
        "<h1>Captures</h1><p class=\"mute small\">Ask every site to measure one frequency at the \
         same moment: which site hears it loudest, KrakenSDR bearings where sites have one, and \
         time differences where sites have an rtl_sdr and a reference transmitter is named. The \
         capture page combines whatever comes back into one fix. Sites measure amateur \
         frequencies only, unless a site's own config allows a band locally.</p>",
    );
    b.push_str(&form(app, f, error));
    b.push_str("<h2>Recent captures</h2>");
    if caps.is_empty() {
        b.push_str("<p class=mute>No captures in the last 30 days.</p>");
        return Ok(page(app, "/captures", "Captures", &b, false).await);
    }
    b.push_str(
        "<div class=wrap><table><tr><th>Start</th><th>What</th><th>Asked by</th>\
         <th>Readings</th><th>Loudest</th><th>Fix (95%)</th></tr>",
    );
    for (i, c) in caps.iter().enumerate() {
        let (r, ev) = {
            let db = app.db();
            let ev = if i < FIXES_LISTED {
                Some(locate::evidence(&db, &app.cfg, c)?)
            } else {
                None
            };
            (rank(capture::readings(&db, &c.id)?), ev)
        };
        let fix = ev.map(|e| locate::solve(&app.cfg, &e));
        let n = r.ranked.len() + r.other.len();
        let what = match (&c.req.beacon, &c.req.reference) {
            (Some(s), _) => format!("{}, beacon test: {}", c.req.summary(), s.name),
            (None, Some(rf)) => format!("{}, reference {}", c.req.summary(), rf.name),
            _ => c.req.summary(),
        };
        let _ = write!(
            b,
            "<tr><td><a href=\"/capture?id={}\">{}</a></td><td>{}<br><span class=\"mute small\">{}</span></td>\
             <td>{}</td><td>{n}</td><td>{}</td><td>{}</td></tr>",
            esc(&c.id),
            esc(&when_text(c, now)),
            esc(&what),
            esc(&c.req.note),
            esc(&c.call),
            loudest(app, &r).unwrap_or_else(|| "<span class=mute>none yet</span>".into()),
            fix_cell(&fix)
        );
    }
    b.push_str("</table></div>");
    Ok(page(app, "/captures", "Captures", &b, false).await)
}

pub async fn captures_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    render_captures(&app, &CaptureForm::default(), None).await
}

/// A form post from another site's page carries that site's Origin.
fn same_origin(h: &HeaderMap) -> bool {
    let Some(origin) = h.get(ORIGIN) else {
        return true;
    };
    let host = h.get(HOST).and_then(|v| v.to_str().ok());
    origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, rest)| rest)
        == host
}

fn too_soon(app: &App, peer: SocketAddr) -> bool {
    let mut posts = app.posts.lock().unwrap_or_else(|e| e.into_inner());
    if posts
        .get(&peer.ip())
        .is_some_and(|t| t.elapsed() < POST_GAP)
    {
        return true;
    }
    posts.retain(|_, t| t.elapsed() < POST_GAP);
    posts.insert(peer.ip(), Instant::now());
    false
}

/// Too many wrong keys lately, from anywhere: someone is guessing from many addresses.
fn locked_out(app: &App) -> bool {
    let mut f = app.failures.lock().unwrap_or_else(|e| e.into_inner());
    while f.front().is_some_and(|t| t.elapsed() > FAIL_WINDOW) {
        f.pop_front();
    }
    f.len() >= MAX_FAILURES
}

fn key_ok(app: &App, key: &str, peer: SocketAddr) -> bool {
    let want = &app.cfg.capture.form_key;
    // blake3's Hash compares in constant time
    let ok = !want.is_empty() && blake3::hash(key.as_bytes()) == blake3::hash(want.as_bytes());
    if !ok {
        tracing::warn!("wrong capture form key from {}", peer.ip());
        app.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(Instant::now());
    }
    ok
}

fn request_from(app: &App, f: &CaptureForm, now: i64) -> Result<Request, String> {
    let freq = parse_hz(&f.freq).map_err(|e| e.to_string())?;
    let bw = parse_hz(&f.bw).map_err(|e| e.to_string())?;
    let secs: u32 = f
        .secs
        .trim()
        .parse()
        .map_err(|_| format!("seconds must be a whole number from 1 to {MAX_SECS}"))?;
    let at = parse_when(&f.when, now)?;
    let req = Request::new(&app.cfg, freq, bw, secs, at, &f.note, now)?;
    match f.reference.trim() {
        "" => Ok(req),
        name => {
            let r = app
                .cfg
                .reference(name)
                .ok_or_else(|| format!("{name:?} is not one of this site's references"))?;
            req.with_reference(&app.cfg, reference_tx(r))
        }
    }
}

pub fn reference_tx(r: &crate::config::ReferenceCfg) -> RefTx {
    RefTx {
        name: r.name.clone(),
        freq: r.freq_hz(),
        bw: r.bw_hz(),
        lat: r.lat,
        lon: r.lon,
    }
}

pub async fn capture_post(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<CaptureForm>,
) -> Result<Response, Fail> {
    if !same_origin(&headers) {
        return Ok((
            StatusCode::FORBIDDEN,
            "Capture requests have to come from this site's own captures page.",
        )
            .into_response());
    }
    if app.cfg.capture.form_key.is_empty() {
        return Ok((
            StatusCode::FORBIDDEN,
            "The capture form is off on this site.",
        )
            .into_response());
    }
    if !app.cfg.form_allowed(peer.ip()) {
        tracing::warn!(
            "capture form post from {}, which capture.form_allow leaves out",
            peer.ip()
        );
        return Ok((
            StatusCode::FORBIDDEN,
            "The capture form only takes requests from this site's own network.",
        )
            .into_response());
    }
    let now = now_ms();
    let checked = if too_soon(&app, peer) {
        Err("Wait a few seconds between requests.".to_string())
    } else if locked_out(&app) {
        Err("Too many wrong form keys in the last few minutes; the form is resting. Try again later.".to_string())
    } else if !key_ok(&app, &f.key, peer) {
        Err("That form key is wrong.".to_string())
    } else {
        request_from(&app, &f, now)
    };
    let req = match checked {
        Ok(r) => r,
        Err(e) => {
            let page = render_captures(&app, &f, Some(&e)).await?;
            return Ok((StatusCode::BAD_REQUEST, page).into_response());
        }
    };
    let ev = app
        .core
        .append("sdr.capture", serde_json::to_value(&req)?)
        .await?;
    tracing::info!(
        "capture {} requested from {}: {}",
        &ev.id[..12],
        peer.ip(),
        req.summary()
    );
    if let Err(e) = ridgewarden_client::follow(&app.core, &app.db, crate::app::PROJECTIONS).await {
        tracing::warn!("following the core after a request: {e:#}");
    }
    Ok(Redirect::to(&format!("/capture?id={}", ev.id)).into_response())
}

fn value_text(v: f64, r: &Ranking) -> String {
    format!("{v:.1} {}", r.scale.unit())
}

fn row_cells(app: &App, row: &Row) -> String {
    let r = &row.r;
    let num = |x: Option<f64>| x.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into());
    let gain = r.gain_db.map_or("auto".into(), |g| format!("{g} dB"));
    let cal = r
        .calibration_db
        .map_or("none".into(), |c| format!("{c:+.1} dB"));
    let pos = match (r.lat, r.lon) {
        (Some(a), Some(o)) => format!("{a:.3}, {o:.3}"),
        _ => "unknown".into(),
    };
    format!(
        "<td>{}<br><span class=\"mute small\">{}</span></td><td>{}</td><td class=small>{}</td>\
         <td>{}</td><td>{}</td><td>{}</td><td>{}</td>",
        site(app, &r.node),
        esc(&row.call),
        esc(&r.receiver),
        esc(&pos),
        num(r.level_db),
        num(r.floor_db),
        esc(&gain),
        esc(&cal)
    )
}

struct Found {
    cap: Cap,
    rows: Vec<Row>,
    dir: Vec<Listed>,
    ctx: Vec<(String, f64, f64)>,
    bearings: Vec<(String, Bearing)>,
    tdoa: Option<(String, tdoa::Solution)>,
    evidence: Evidence,
    fix: Result<Fix, String>,
}

fn find(app: &App, id: &str, now: i64) -> anyhow::Result<Option<Found>> {
    let mut found = {
        let db = app.db();
        let Some(cap) = capture::find(&db, id)? else {
            return Ok(None);
        };
        Found {
            rows: capture::readings(&db, &cap.id)?,
            dir: directory::list(&db, now)?,
            ctx: directory::node_positions(&db, now)?,
            bearings: kraken::bearings(&db, &cap.id)?,
            tdoa: tdoa::result(&db, &cap.id)?,
            evidence: locate::evidence(&db, &app.cfg, &cap)?,
            fix: Err(String::new()),
            cap,
        }
    };
    found.fix = locate::solve(&app.cfg, &found.evidence);
    Ok(Some(found))
}

fn fix_section(f: &Found) -> String {
    let mut b = String::from("<h2>Location</h2>");
    let fix = match &f.fix {
        Ok(fix) => fix,
        Err(why) => {
            let _ = write!(b, "<p class=mute>No fix: {}</p>", esc(why));
            return b;
        }
    };
    let e = fix.ellipse;
    let color = if e.major_km <= NEIGHBORHOOD_KM {
        "green"
    } else {
        "yellow"
    };
    let _ = write!(
        b,
        "<div class=\"headline {color}\">At {:.4}, {:.4}, within {:.2} km by {:.2} km (95%)</div>",
        fix.pos.lat, fix.pos.lon, e.major_km, e.minor_km
    );
    let mut notes = vec![];
    if e.major_km <= NEIGHBORHOOD_KM {
        notes.push("Narrowed to a neighborhood.".to_string());
    }
    if !fix.inside {
        notes.push("The fix is outside the ring of sites that measured it, where accuracy falls off fast; the ellipse is stretched to say so.".into());
    }
    if fix.ambiguous {
        notes.push("Another, distant spot fits the measurements about as well. Add a site or a bearing to tell them apart.".into());
    }
    if fix.dof == 0 {
        notes.push("There are exactly as many measurements as unknowns, so nothing cross-checks this fix: the ellipse comes from stated errors alone.".into());
    } else {
        notes.push(format!(
            "Measurements disagree by {:.1} times what their stated errors allow (1 is as expected){}.",
            (fix.chi2 / fix.dof as f64).sqrt(),
            if fix.chi2 > fix.dof as f64 { "; the ellipse is widened to match" } else { "" }
        ));
    }
    let used: Vec<String> = fix.used.iter().map(|(k, n)| format!("{n} {k}")).collect();
    notes.push(format!("From {}.", used.join(", ")));
    if fix.used.len() == 1 && fix.used[0].0 == "signal strength" {
        notes.push(
            "Signal strength alone is rough: antennas, terrain and gain differ between sites."
                .into(),
        );
    }
    if let Some(s) = &f.cap.req.beacon {
        let truth = LatLon::new(s.lat, s.lon);
        let miss = distance_km(fix.pos, truth);
        notes.push(format!(
            "Beacon test: the surveyed spot is {miss:.2} km from the fix, {} the 95% ellipse.",
            if e.contains(fix.pos, truth) {
                "inside"
            } else {
                "outside"
            }
        ));
    }
    b.push_str("<ul class=plain>");
    for n in notes {
        let _ = write!(b, "<li>{}</li>", esc(&n));
    }
    b.push_str("</ul>");
    b
}

fn map_section(app: &App, f: &Found, r: &Ranking) -> Option<String> {
    let mut points: Vec<Pt> = vec![];
    let n = r.ranked.len().max(1) as f64;
    for (i, (row, v)) in r.ranked.iter().enumerate() {
        if let (Some(lat), Some(lon)) = (row.r.lat, row.r.lon) {
            points.push(Pt {
                lat,
                lon,
                label: format!(
                    "{}. {} {}",
                    i + 1,
                    app.cfg.label(&row.r.node),
                    row.r.receiver
                ),
                tip: format!(
                    "{} {}: {}",
                    app.cfg.label(&row.r.node),
                    row.r.receiver,
                    value_text(*v, r)
                ),
                class: if i == 0 { "green" } else { "yellow" },
                r: 5.0 + 7.0 * (n - i as f64) / n,
            });
        }
    }
    let mut shapes = vec![];
    let reach = 40.0;
    for (call, b) in f.bearings.iter().filter(|(_, b)| b.status == "ok") {
        let (Some(lat), Some(lon), Some(az)) = (b.lat, b.lon, b.bearing_deg) else {
            continue;
        };
        let from = LatLon::new(lat, lon);
        let to = destination(from, az, reach);
        shapes.push(Shape {
            pts: vec![(lat, lon), (to.lat, to.lon)],
            class: "ray",
            tip: format!("{call} {}: bearing {az:.0} degrees", b.kraken),
            fit: false,
        });
        if !points.iter().any(|p| p.lat == lat && p.lon == lon) {
            points.push(Pt {
                lat,
                lon,
                label: format!("{} {}", app.cfg.label(&b.node), b.kraken),
                tip: format!("KrakenSDR {} at {}", b.kraken, app.cfg.label(&b.node)),
                class: "yellow",
                r: 5.0,
            });
        }
    }
    if let Ok(fix) = &f.fix {
        shapes.push(Shape {
            pts: fix
                .ellipse
                .outline(fix.pos)
                .iter()
                .map(|p| (p.lat, p.lon))
                .collect(),
            class: "ellipse",
            tip: format!(
                "95% error ellipse, {:.2} by {:.2} km",
                fix.ellipse.major_km, fix.ellipse.minor_km
            ),
            fit: true,
        });
        points.push(Pt {
            lat: fix.pos.lat,
            lon: fix.pos.lon,
            label: "fix".into(),
            tip: format!("fix at {:.4}, {:.4}", fix.pos.lat, fix.pos.lon),
            class: "red",
            r: 4.0,
        });
    }
    if let Some(s) = &f.cap.req.beacon {
        points.push(Pt {
            lat: s.lat,
            lon: s.lon,
            label: s.name.clone(),
            tip: format!("{}, surveyed", s.name),
            class: "spot",
            r: 7.0,
        });
    }
    map::render(
        &points,
        &labelled(app, f.ctx.clone()),
        &shapes,
        "Readings, bearings and the fix",
    )
}

fn bearings_section(app: &App, f: &Found) -> String {
    if f.bearings.is_empty() {
        return String::new();
    }
    let mut b = String::from(
        "<h2>KrakenSDR bearings</h2><div class=wrap><table><tr><th>Site</th><th>KrakenSDR</th>\
         <th>Bearing</th><th>Error</th><th>Readings</th><th>State</th></tr>",
    );
    for (call, x) in &f.bearings {
        let state = match x.status.as_str() {
            "ok" => pill(Color::Green, "ok"),
            "refused" => format!("{} {}", pill(Color::Unknown, "refused"), esc(&x.detail)),
            _ => format!("{} {}", pill(Color::Red, "failed"), esc(&x.detail)),
        };
        let _ = write!(
            b,
            "<tr><td>{}<br><span class=\"mute small\">{}</span></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{state}</td></tr>",
            site(app, &x.node),
            esc(call),
            esc(&x.kraken),
            x.bearing_deg
                .map_or("-".into(), |d| format!("{d:.1} degrees true")),
            x.sigma_deg
                .map_or("-".into(), |s| format!("{s:.1} degrees")),
            x.samples
        );
    }
    b.push_str(
        "</table></div><p class=\"mute small\">KrakenSDR bearings are good to several degrees and \
         suffer from multipath off ridges; each counts with at least its configured error.</p>",
    );
    b
}

fn tdoa_section(app: &App, f: &Found) -> String {
    let Some(r) = &f.cap.req.reference else {
        return String::new();
    };
    let mut b = format!(
        "<h2>Time differences</h2><p class=\"mute small\">Aligned on reference {} ({}). With \
         RTL-SDRs, expect 0.5 to 2 km inside the ring of receivers at best, much worse outside it: \
         their clocks are lined up on the reference, not on GPS. The weekly beacon test shows how \
         far fixes really miss.</p>",
        esc(&r.name),
        esc(&khz(r.freq))
    );
    let Some((call, sol)) = &f.tdoa else {
        let _ = write!(
            b,
            "<p>{} snippets published so far. A site with <code>tdoa.solve</code> fetches them and \
             works out the time differences once at least two are in.</p>",
            f.evidence.snippets
        );
        return b;
    };
    let _ = write!(
        b,
        "<p>From {} snippets, worked out by {}.</p>",
        sol.snippets,
        esc(call)
    );
    if !sol.pairs.is_empty() {
        b.push_str(
            "<div class=wrap><table><tr><th>From</th><th>To</th><th>Path difference</th>\
             <th>Error</th><th>Target peak</th><th>Reference peak</th></tr>",
        );
        for p in &sol.pairs {
            let _ = write!(
                b,
                "<tr><td>{} {}</td><td>{} {}</td><td>{:+.2} km</td><td>{:.2} km</td><td>{:.1}</td><td>{:.1}</td></tr>",
                site(app, &p.a.node),
                esc(&p.a.receiver),
                site(app, &p.b.node),
                esc(&p.b.receiver),
                p.dd_km,
                p.sigma_km,
                p.psr_target,
                p.psr_ref
            );
        }
        b.push_str(
            "</table></div><p class=\"mute small\">Path difference is how much further the second \
             site is from the transmitter. Peaks are the correlation peak over the spread of the \
             rest; below 8 a pair is refused.</p>",
        );
    }
    if !sol.problems.is_empty() {
        b.push_str("<ul class=plain>");
        for p in &sol.problems {
            let _ = write!(b, "<li>{} {}</li>", pill(Color::Yellow, "not used"), esc(p));
        }
        b.push_str("</ul>");
    }
    b
}

pub async fn capture_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Fail> {
    let now = now_ms();
    let id = q.get("id").map(String::as_str).unwrap_or("");
    let Some(f) = find(&app, id, now)? else {
        let b = "<h1>Not found</h1><p>No capture with that id. <a href=\"/captures\">All captures</a></p>";
        return Ok((
            StatusCode::NOT_FOUND,
            page(&app, "", "Not found", b, false).await,
        )
            .into_response());
    };
    let req = &f.cap.req;
    let at = req.at_ms();
    let mut b = format!(
        "<h1>{}</h1><p>Asked by {} for {}.{}</p>",
        esc(&req.summary()),
        esc(&f.cap.call),
        esc(&when_text(&f.cap, now)),
        if req.note.is_empty() {
            String::new()
        } else {
            format!(" <span class=mute>{}</span>", esc(&req.note))
        }
    );
    let missing: Vec<&Listed> = f
        .dir
        .iter()
        .filter(|l| l.rx.capture && l.rx.covers(req.lo(), req.hi()))
        .filter(|l| {
            !f.rows
                .iter()
                .any(|r| r.key == l.key && r.r.receiver == l.rx.name)
        })
        .collect();
    let r = rank(f.rows.clone());
    if at > now {
        b.push_str("<div class=\"headline unknown\">Scheduled. Readings show here once sites have measured and their logs have synced.</div>");
    } else if let Some(top) = r.ranked.first() {
        let _ = write!(
            b,
            "<div class=\"headline green\">Loudest at {} on {}: {}</div>",
            site(&app, &top.0.r.node),
            esc(&top.0.r.receiver),
            esc(&value_text(top.1, &r))
        );
    } else {
        b.push_str("<div class=\"headline unknown\">No readings yet. Sites log them as telemetry; they arrive with the next sync.</div>");
    }
    if at <= now {
        b.push_str(&fix_section(&f));
    }
    b.push_str(map::CSS);
    if let Some(m) = map_section(&app, &f, &r) {
        b.push_str(&m);
    }
    if !r.ranked.is_empty() {
        let _ = write!(
            b,
            "<h2>Signal strength</h2><p class=banner>{}</p>",
            esc(r.scale.explain())
        );
        let _ = write!(
            b,
            "<div class=wrap><table><tr><th>#</th><th>{}</th><th>Site</th><th>Receiver</th><th>Position</th>\
             <th>Level, dB raw</th><th>Floor, dB raw</th><th>Gain</th><th>Calibration</th></tr>",
            esc(r.scale.unit())
        );
        for (i, (row, v)) in r.ranked.iter().enumerate() {
            let _ = write!(
                b,
                "<tr><td>{}</td><td><b>{v:.1}</b></td>{}</tr>",
                i + 1,
                row_cells(&app, row)
            );
        }
        b.push_str(
            "</table></div><p class=\"mute small\">Raw levels are dB relative to each dongle's ADC, \
             summed over the requested bandwidth. The floor is the median bin either side of it.</p>",
        );
    }
    b.push_str(&bearings_section(&app, &f));
    b.push_str(&tdoa_section(&app, &f));
    if !r.other.is_empty() {
        b.push_str("<h2>Sites that didn't measure it</h2><ul class=plain>");
        for row in &r.other {
            let c = if row.r.status == "refused" {
                Color::Unknown
            } else {
                Color::Red
            };
            let _ = write!(
                b,
                "<li>{} {} {}: {}</li>",
                pill(c, &row.r.status),
                site(&app, &row.r.node),
                esc(&row.r.receiver),
                esc(&row.r.detail)
            );
        }
        b.push_str("</ul>");
    }
    if !missing.is_empty() && at <= now {
        b.push_str(
            "<h2>No reading yet</h2><p class=\"mute small\">These receivers cover the frequency. A \
             reading may still be on its way; if not, the site was cut off, got the request after \
             its start time, or its clock is off.</p><ul class=plain>",
        );
        for l in missing {
            let _ = write!(b, "<li>{} {}</li>", site(&app, &l.rx.node), esc(&l.rx.name));
        }
        b.push_str("</ul>");
    }
    let _ = write!(
        b,
        "<p class=\"mute small\"><a href=\"/api/capture?id={}\">JSON</a> &middot; <a href=\"/captures\">All captures</a></p>",
        esc(&f.cap.id)
    );
    let busy = at > now - 30 * 60_000;
    Ok(page(&app, "/captures", &req.summary(), &b, busy)
        .await
        .into_response())
}

fn row_json(row: &Row, value: Option<f64>) -> Value {
    json!({"station": row.call, "received": row.ts, "value": value, "reading": row.r})
}

fn fix_json(f: &Result<Fix, String>) -> Value {
    match f {
        Ok(f) => json!({
            "lat": f.pos.lat, "lon": f.pos.lon,
            "ellipse_95": {"major_km": f.ellipse.major_km, "minor_km": f.ellipse.minor_km, "az_deg": f.ellipse.az_deg},
            "chi2": f.chi2, "dof": f.dof, "inside_ring": f.inside, "ambiguous": f.ambiguous,
            "used": f.used.iter().map(|(k, n)| json!({"kind": k, "count": n})).collect::<Vec<_>>(),
        }),
        Err(why) => json!({"error": why}),
    }
}

pub async fn capture_json(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Fail> {
    let id = q.get("id").map(String::as_str).unwrap_or("");
    let Some(f) = find(&app, id, now_ms())? else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no capture with that id"})),
        )
            .into_response());
    };
    let r = rank(f.rows.clone());
    Ok(Json(json!({
        "id": f.cap.id, "requested_by": f.cap.call, "request": f.cap.req,
        "scale": r.scale.unit(), "note": r.scale.explain(),
        "ranked": r.ranked.iter().map(|(row, v)| row_json(row, Some(*v))).collect::<Vec<_>>(),
        "other": r.other.iter().map(|row| row_json(row, None)).collect::<Vec<_>>(),
        "bearings": f.bearings.iter().map(|(c, b)| json!({"station": c, "bearing": b})).collect::<Vec<_>>(),
        "tdoa": f.tdoa.as_ref().map(|(c, s)| json!({"solved_by": c, "result": s})),
        "fix": fix_json(&f.fix),
    }))
    .into_response())
}

pub async fn accuracy_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let h = beacon::history(&app.db())?;
    let mut b = String::from(
        "<h1>Accuracy</h1><p class=\"mute small\">Once a week one site asks every site to locate a \
         beacon whose position was surveyed, and publishes how far the fix missed. That is the \
         real accuracy of this network, as opposed to what its error ellipses claim: about 95% of \
         misses should fall inside their ellipse if the claimed errors are honest.</p>",
    );
    match &app.cfg.beacon {
        Some(c) => {
            let _ = write!(
                b,
                "<p>Test: {} on {} at {:.5}, {:.5}, every {} at {:02}:{:02} UTC. {}</p>",
                esc(&c.name),
                esc(&khz(c.freq_hz())),
                c.lat,
                c.lon,
                esc(&c.weekday),
                c.hour_utc,
                c.minute,
                if c.schedule { "This site asks for it and publishes the result." } else { "Another site asks for it." }
            );
        }
        None => b.push_str("<p class=mute>This site has no [beacon] section; results published by other sites still show here.</p>"),
    }
    if h.is_empty() {
        b.push_str("<p class=mute>No beacon tests published yet. Set up [beacon] in sdr.toml at one site with schedule = true.</p>");
        return Ok(page(&app, "/accuracy", "Accuracy", &b, false).await);
    }
    let misses: Vec<&beacon::Outcome> = h
        .iter()
        .map(|x| &x.1)
        .filter(|o| o.miss_km.is_some())
        .collect();
    let inside = misses.iter().filter(|o| o.inside == Some(true)).count();
    let last = misses.last().and_then(|o| o.miss_km);
    let _ = write!(
        b,
        "<div class=cards><div class=card><div class=n>{}</div>latest miss</div>\
         <div class=card><div class=n>{}/{}</div>inside their ellipse</div>\
         <div class=card><div class=n>{}</div>tests without a fix</div></div>",
        last.map_or("-".into(), |m| format!("{m:.2} km")),
        inside,
        misses.len(),
        h.len() - misses.len()
    );
    if let Some((now_m, before)) = beacon::trend(&h) {
        let word = if now_m < before * 0.9 {
            "better"
        } else if now_m > before * 1.1 {
            "worse"
        } else {
            "about the same"
        };
        let _ = write!(
            b,
            "<p>Median miss of the last four tests: {now_m:.2} km, against {before:.2} km for the four before: {word}.</p>"
        );
    }
    let from = h.first().map_or(0, |x| x.1.at);
    let to = h.last().map_or(0, |x| x.1.at).max(from + 1);
    let chart = Chart {
        from,
        to,
        unit: "km".into(),
        lines: vec![
            Line {
                label: "miss".into(),
                points: misses
                    .iter()
                    .map(|o| (o.at, o.miss_km.unwrap_or(0.0)))
                    .collect(),
            },
            Line {
                label: "95% ellipse, long axis".into(),
                points: misses
                    .iter()
                    .filter_map(|o| Some((o.at, o.major_km?)))
                    .collect(),
            },
        ],
        ..Default::default()
    };
    b.push_str(&chart.render("Beacon test miss distance"));
    b.push_str(
        "<div class=wrap><table><tr><th>When</th><th>Beacon</th><th>Miss</th><th>Ellipse</th>\
         <th>Inside</th><th>From</th><th>Published by</th></tr>",
    );
    for (call, o) in h.iter().rev() {
        let miss = match (o.miss_km, &o.problem) {
            (Some(m), _) => format!("{m:.2} km"),
            (None, Some(p)) => format!("no fix: {p}"),
            _ => "-".into(),
        };
        let _ = write!(
            b,
            "<tr><td><a href=\"/capture?id={}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&o.capture),
            esc(&short_time(o.at)),
            esc(&o.name),
            esc(&miss),
            o.major_km.map_or("-".into(), |m| format!("{m:.2} km")),
            match o.inside {
                Some(true) => pill(Color::Green, "yes"),
                Some(false) => pill(Color::Red, "no"),
                None => "-".into(),
            },
            esc(&o.used),
            esc(call)
        );
    }
    b.push_str("</table></div>");
    Ok(page(&app, "/accuracy", "Accuracy", &b, false).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_site_posts_are_refused() {
        let mut h = HeaderMap::new();
        h.insert(HOST, "tst-pi.local.mesh:8798".parse().unwrap());
        assert!(same_origin(&h));
        h.insert(ORIGIN, "http://tst-pi.local.mesh:8798".parse().unwrap());
        assert!(same_origin(&h));
        h.insert(ORIGIN, "http://elsewhere.local.mesh:8798".parse().unwrap());
        assert!(!same_origin(&h));
    }
}
