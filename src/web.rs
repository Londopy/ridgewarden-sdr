//! The directory page and map, the band plan, and the directory as GeoJSON. Captures,
//! fixes and the accuracy page are in `hunt`.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, now_ms, short_time};
use serde_json::{Value, json};

use crate::app::App;
use crate::bands::{BANDS, mhz};
use crate::directory::{self, Listed};
use crate::hunt;
use crate::map::{self, Pt};
use crate::owrx;
use crate::timing;

/// A receiver not heard from in this long is shown as quiet.
const QUIET_MS: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(directory_page))
        .route(
            "/captures",
            get(hunt::captures_page).post(hunt::capture_post),
        )
        .route("/capture", get(hunt::capture_page))
        .route("/accuracy", get(hunt::accuracy_page))
        .route("/accuracy/timing", get(timing::timing_page))
        .route("/bands", get(bands_page))
        .route("/api/receivers.geojson", get(receivers_geojson))
        .route("/api/capture", get(hunt::capture_json))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

pub struct Fail(anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

pub async fn page(app: &App, active: &str, title: &str, body: &str, refresh: bool) -> Html<String> {
    let who = match app.whoami().await {
        Ok(w) if !w.node.is_empty() => format!("{} at {}", w.call, app.cfg.label(&w.node)),
        Ok(w) => w.call,
        Err(_) => "core unreachable".into(),
    };
    let page = Page {
        app: "ridgewarden-sdr",
        nav: &[
            ("/", "Receivers"),
            ("/captures", "Captures"),
            ("/accuracy", "Accuracy"),
            ("/bands", "Band plan"),
        ],
        active,
        who: &who,
        refresh,
    };
    Html(page.render(title, body))
}

pub fn site(app: &App, node: &str) -> String {
    esc(&app.cfg.label(node))
}

pub fn range(lo: u64, hi: u64) -> String {
    format!("{} to {}", mhz(lo).trim_end_matches(" MHz"), mhz(hi))
}

fn quiet(l: &Listed, now: i64) -> bool {
    now - l.heard() > QUIET_MS
}

/// Other mesh nodes for the map, under their friendly names.
pub fn labelled(app: &App, ctx: Vec<(String, f64, f64)>) -> Vec<(String, f64, f64)> {
    ctx.into_iter()
        .map(|(n, la, lo)| (app.cfg.label(&n), la, lo))
        .collect()
}

fn rx_point(app: &App, l: &Listed, now: i64) -> Option<Pt> {
    let web = l.status.as_ref().and_then(|s| s.web);
    let class = match (quiet(l, now), web) {
        (true, _) => "unknown",
        (false, Some(false)) => "red",
        _ => "green",
    };
    Some(Pt {
        lat: l.rx.lat?,
        lon: l.rx.lon?,
        label: format!("{} {}", app.cfg.label(&l.rx.node), l.rx.name),
        tip: format!(
            "{} at {}: {}, heard {} ago",
            l.rx.name,
            app.cfg.label(&l.rx.node),
            range(l.rx.min_hz, l.rx.max_hz),
            human_duration(now - l.heard())
        ),
        class,
        r: 6.0,
    })
}

fn rf_cell(l: &Listed) -> String {
    let rx = &l.rx;
    if let Some(w) = &rx.withheld {
        return format!(
            "{}<br><span class=\"mute small\">{}</span>",
            pill(Color::Yellow, "link withheld"),
            esc(w)
        );
    }
    let off = rx.off_band();
    if !off.is_empty() {
        let names: Vec<&str> = off.iter().map(|p| p.name.as_str()).collect();
        return format!(
            "{}<br><span class=small>not amateur by this site's check, so its link isn't shown: {}</span>",
            pill(Color::Red, "outside amateur bands"),
            esc(&names.join(", "))
        );
    }
    if rx.profiles.is_empty() {
        return "<span class=mute>none</span>".into();
    }
    rx.profiles
        .iter()
        .map(|p| {
            format!(
                "{} <span class=\"mute small\">{}</span>",
                esc(&p.name),
                esc(&range(p.lo, p.hi))
            )
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

fn web_cell(l: &Listed) -> String {
    let Some(url) = l.rx.link() else {
        return "<span class=mute>no link</span>".into();
    };
    let state = match l
        .status
        .as_ref()
        .and_then(|s| s.web.map(|w| (w, &s.detail)))
    {
        Some((true, d)) => format!(
            "{} <span class=\"mute small\">{}</span>",
            pill(Color::Green, "answering"),
            esc(d)
        ),
        Some((false, d)) => format!(
            "{} <span class=\"mute small\">{}</span>",
            pill(Color::Red, "not answering"),
            esc(d)
        ),
        None => pill(Color::Unknown, "not checked yet"),
    };
    format!("<a href=\"{}\">open</a><br>{state}", esc(url))
}

fn capture_cell(l: &Listed) -> String {
    if !l.rx.capture {
        return "<span class=mute>no</span>".into();
    }
    let mut s = String::from(if l.rx.calibrated {
        "yes, calibrated"
    } else {
        "yes"
    });
    if let Some(t) = l.status.as_ref().and_then(|s| s.last_capture) {
        let _ = write!(
            s,
            "<br><span class=\"mute small\">last {}</span>",
            esc(&short_time(t))
        );
    }
    s
}

async fn directory_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let (list, ctx) = {
        let db = app.db();
        (
            directory::list(&db, now)?,
            directory::node_positions(&db, now)?,
        )
    };
    let mut b = String::from(
        "<h1>Receivers on the mesh</h1><p class=\"mute small\">Every SDR a site on the mesh has \
         published, read from the signed log this site keeps, so the list and map work with the \
         internet down. Web links go to each site's mesh-facing OpenWebRX+, which may only offer \
         amateur bands over RF; this site checks every profile against the band plan itself.</p>",
    );
    let live: Vec<&Listed> = list.iter().filter(|l| now - l.heard() < DAY).collect();
    let mut sites: Vec<&str> = live.iter().map(|l| l.key.as_str()).collect();
    sites.sort_unstable();
    sites.dedup();
    if !list.is_empty() {
        let _ = write!(
            b,
            "<div class=cards><div class=card><div class=n>{}</div>receivers heard today</div>\
             <div class=card><div class=n>{}</div>sites listening</div>\
             <div class=card><div class=n>{}</div>can measure</div></div>",
            live.len(),
            sites.len(),
            live.iter().filter(|l| l.rx.capture).count()
        );
    }
    b.push_str(map::CSS);
    let points: Vec<Pt> = list.iter().filter_map(|l| rx_point(&app, l, now)).collect();
    match map::render(&points, &labelled(&app, ctx), &[], "Receivers on the mesh") {
        Some(m) => {
            b.push_str(&m);
            b.push_str(
                "<p class=\"mute small\">Green: heard in the last hour. Red: its web page isn't \
                 answering. Gray: not heard for over an hour. Small gray dots are other mesh nodes.</p>",
            );
        }
        None if !list.is_empty() => b.push_str(
            "<p class=mute>No receiver has a position yet. Sites take it from their node's \
             sysinfo, or from lat and lon in sdr.toml.</p>",
        ),
        None => {}
    }
    if list.is_empty() {
        b.push_str(
            "<p class=mute>No receivers published yet. A site lists its SDRs as [[receiver]] \
             entries in sdr.toml; they show up here once its core has signed them and the log \
             has synced.</p>",
        );
        return Ok(page(&app, "/", "Receivers", &b, true).await);
    }
    b.push_str(
        "<div class=wrap><table><tr><th>Site</th><th>Receiver</th><th>Tunes</th>\
         <th>Over RF</th><th>Web</th><th>Captures</th><th>Heard</th></tr>",
    );
    for l in &list {
        let ago = format!("{} ago", human_duration(now - l.heard()));
        let heard = if quiet(l, now) {
            pill(Color::Yellow, &ago)
        } else {
            ago
        };
        let _ = write!(
            b,
            "<tr><td>{}<br><span class=\"mute small\">{}</span></td><td>{}<br><span class=\"mute small\">{}</span></td>\
             <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{heard}</td></tr>",
            site(&app, &l.rx.node),
            esc(&l.call),
            esc(&l.rx.name),
            esc(&l.rx.antenna),
            esc(&range(l.rx.min_hz, l.rx.max_hz)),
            rf_cell(l),
            web_cell(l),
            capture_cell(l)
        );
    }
    b.push_str(
        "</table></div><p class=\"mute small\"><a href=\"/api/receivers.geojson\">GeoJSON</a></p>",
    );
    Ok(page(&app, "/", "Receivers", &b, true).await)
}

async fn bands_page(State(app): State<Arc<App>>) -> Html<String> {
    let mut b = String::from(
        "<h1>Band plan</h1><p>FCC Part 97.113(c) doesn't allow retransmitting non-amateur signals \
         over amateur RF, which is what the mesh is. So a receiver's web page may only be offered \
         across the mesh if every profile it serves sits wholly inside one of these bands, and \
         captures measure only these frequencies unless a site allows a band locally.</p>\
         <p class=\"mute small\">US allocations from 47 CFR 97.301 (ITU Region 2, all licence \
         classes combined) and the 60 m channels of 97.303(h). 219-220 MHz is kept to fixed \
         point-to-point digital message forwarding by 97.303(l), so no web receiver offers it.</p>\
         <div class=wrap><table><tr><th>Band</th><th>From</th><th>To</th><th>Web receivers</th></tr>",
    );
    for band in BANDS {
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(band.name),
            esc(&mhz(band.lo)),
            esc(&mhz(band.hi)),
            if band.offer {
                "may offer"
            } else {
                "measure only"
            }
        );
    }
    b.push_str("</table></div><h2>This site</h2>");
    if app.cfg.capture.local_bands.is_empty() {
        b.push_str("<p>No local bands: this site measures amateur frequencies only.</p>");
    } else {
        b.push_str("<p>This site also measures these ranges locally:</p><ul class=plain>");
        for [lo, hi] in &app.cfg.capture.local_bands {
            let _ = write!(b, "<li>{lo} to {hi} MHz</li>");
        }
        b.push_str("</ul>");
    }
    for rx in &app.cfg.receivers {
        let Some(path) = &rx.openwebrx_settings else {
            continue;
        };
        let _ = write!(b, "<h2>Receiver {}</h2>", esc(&rx.name));
        match owrx::load(path).and_then(|s| owrx::report(&s)) {
            Ok(r) => {
                let _ = write!(b, "<pre class=wrap>{}</pre>", esc(&r));
            }
            Err(e) => {
                let _ = write!(b, "<p class=banner>{}</p>", esc(&format!("{e:#}")));
            }
        }
    }
    page(&app, "/bands", "Band plan", &b, false).await
}

fn feature(l: &Listed) -> Value {
    let geometry = match (l.rx.lat, l.rx.lon) {
        (Some(a), Some(o)) => json!({"type": "Point", "coordinates": [o, a]}),
        _ => Value::Null,
    };
    json!({
        "type": "Feature",
        "geometry": geometry,
        "properties": {
            "station": l.call, "node": l.rx.node, "name": l.rx.name, "antenna": l.rx.antenna,
            "min_hz": l.rx.min_hz, "max_hz": l.rx.max_hz, "url": l.rx.link(), "withheld": l.rx.withheld,
            "off_band": l.rx.off_band().iter().map(|p| &p.name).collect::<Vec<_>>(),
            "profiles": l.rx.profiles, "capture": l.rx.capture, "calibrated": l.rx.calibrated,
            "location": l.rx.location, "described": l.ts, "heard": l.heard(),
            "web": l.status.as_ref().and_then(|s| s.web),
            "last_capture": l.status.as_ref().and_then(|s| s.last_capture),
        }
    })
}

async fn receivers_geojson(State(app): State<Arc<App>>) -> Result<Response, Fail> {
    let list = directory::list(&app.db(), now_ms())?;
    let body = json!({"type": "FeatureCollection", "features": list.iter().map(feature).collect::<Vec<_>>()});
    Ok(([("content-type", "application/geo+json")], body.to_string()).into_response())
}
