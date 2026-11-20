//! Publishing this site's receivers: a signed description when it changes (and daily, so
//! sites that joined later get it), and live status as telemetry every few minutes.

use std::time::Instant;

use anyhow::{Result, bail};
use ridgewarden_client::event::list_hash;
use ridgewarden_client::util::{norm_node, now_ms};
use rusqlite::{OptionalExtension, params};

use crate::app::{App, short_err};
use crate::capture::last_capture;
use crate::config::ReceiverCfg;
use crate::directory::{self, Status};

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
        let desc = directory::describe(rx, &node, pos);
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
                Err(e) => (Some(false), short_err(&e.into())),
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
