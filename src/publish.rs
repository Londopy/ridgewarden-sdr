//! Publishing this site's receivers: a signed description when it changes (and daily, so
//! sites that joined later get it), and live status as telemetry every few minutes.

use std::time::Instant;

use anyhow::{Result, bail};
use ridgewarden_client::event::list_hash;
use ridgewarden_client::util::{norm_node, now_ms};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::app::{App, short_err};
use crate::capture::last_capture;
use crate::config::ReceiverCfg;
use crate::directory::{self, Served, Status};
use crate::owrx;

const REFRESH_MS: i64 = 86_400_000;

/// Publish every receiver's description if it is new or a day old, and its status.
/// Returns how many descriptions went out.
pub async fn round(app: &App) -> Result<usize> {
    let me = app.whoami().await?;
    if me.node.is_empty() {
        bail!("the core hasn't polled its node yet, so this site's node name isn't known");
    }
    let node = norm_node(&me.node);
    let pos = directory::home(&app.db(), &me.key)?;
    let now = now_ms();
    let mut sent = 0;
    for rx in &app.cfg.receivers {
        let live = match &rx.openwebrx_url {
            Some(url) => served(&app.http, url).await,
            None => Ok(vec![]),
        };
        let desc = directory::describe(rx, &node, pos, &live);
        let payload = serde_json::to_value(&desc)?;
        let hash = list_hash(&payload);
        if is_due(app, &rx.name, &hash, now)? {
            if let Some(w) = &desc.withheld {
                tracing::warn!("receiver {}: web link withheld: {w}", rx.name);
            }
            app.core.append("sdr.receiver", payload).await?;
            app.db().execute(
                "INSERT INTO published (name, hash, ts) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET hash = excluded.hash, ts = excluded.ts",
                params![rx.name, hash, now],
            )?;
            sent += 1;
        }
        let status = status(app, rx).await?;
        app.core
            .append_telemetry("sdr.status", serde_json::to_value(&status)?)
            .await?;
    }
    Ok(sent)
}

/// What the OpenWebRX+ instance at `url` says it serves, from its status.json.
async fn served(http: &reqwest::Client, url: &str) -> Served {
    let asked = async {
        // links often carry a #freq= to open on, which isn't part of the path
        let mut status = reqwest::Url::parse(url)?;
        let path = format!("{}/status.json", status.path().trim_end_matches('/'));
        status.set_path(&path);
        status.set_query(None);
        status.set_fragment(None);
        // the reason is published, so it mustn't carry the address of a withheld link
        let v: Value = http
            .get(status)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(reqwest::Error::without_url)?
            .json()
            .await
            .map_err(reqwest::Error::without_url)?;
        owrx::status_profiles(&v)
    };
    asked.await.map_err(|e| short_err(&e))
}

fn is_due(app: &App, name: &str, hash: &str, now: i64) -> Result<bool> {
    let last: Option<(String, i64)> = app
        .db()
        .query_row(
            "SELECT hash, ts FROM published WHERE name = ?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(last.is_none_or(|(h, ts)| h != hash || now - ts > REFRESH_MS))
}

async fn status(app: &App, rx: &ReceiverCfg) -> Result<Status> {
    let (web, detail) = match &rx.openwebrx_url {
        None => (None, String::new()),
        Some(url) => {
            let t = Instant::now();
            match app.http.get(url).send().await {
                Ok(r) if r.status().as_u16() < 500 => (
                    Some(true),
                    format!(
                        "HTTP {} in {} ms",
                        r.status().as_u16(),
                        t.elapsed().as_millis()
                    ),
                ),
                Ok(r) => (Some(false), format!("HTTP {}", r.status().as_u16())),
                Err(e) => (Some(false), short_err(&e.without_url().into())),
            }
        }
    };
    Ok(Status {
        name: rx.name.clone(),
        web,
        detail,
        last_capture: last_capture(&app.db(), &rx.name)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::RawQuery;
    use axum::http::StatusCode;
    use axum::routing::get;

    #[tokio::test]
    async fn asks_the_running_instance_what_it_serves() {
        let (copy, _) = owrx::filtered(&owrx::tests::sample(), true).unwrap();
        let status = owrx::tests::status_of(&copy);
        let instance = Router::new()
            .route(
                "/status.json",
                // a query left on the link would arrive here
                get(move |RawQuery(q): RawQuery| async move {
                    match q {
                        None => Ok(axum::Json(status)),
                        Some(_) => Err(StatusCode::BAD_REQUEST),
                    }
                }),
            )
            .route(
                "/broken/status.json",
                get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "KeyError: 'samp_rate'") }),
            )
            .route(
                "/other/status.json",
                get(|| async { "<html>not OpenWebRX</html>" }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, instance).await.unwrap() });
        let http = crate::app::http_client().unwrap();

        let live = served(&http, &format!("{base}/")).await.unwrap();
        assert_eq!(live.len(), 2);
        assert!(live.iter().any(|p| p.name == "70cm"));
        let opened = served(&http, &format!("{base}/#freq=145500000,mod=nfm")).await;
        assert_eq!(
            opened.unwrap().len(),
            2,
            "the link's fragment isn't in the path"
        );
        let queried = served(&http, &format!("{base}/?lang=de")).await;
        assert_eq!(queried.unwrap().len(), 2, "the link's query isn't sent");
        // the reasons are published with the link withheld, so they don't name its address
        let e = served(&http, &format!("{base}/broken")).await.unwrap_err();
        assert!(e.contains("500") && !e.contains("127.0.0.1"), "{e}");
        let e = served(&http, &format!("{base}/other/")).await.unwrap_err();
        assert!(e.contains("decoding") && !e.contains("127.0.0.1"), "{e}");
    }
}
