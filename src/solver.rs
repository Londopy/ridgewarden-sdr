//! The solving side of time differences: a site with `tdoa.solve` fetches every site's
//! snippet for a capture through the core's blob store, works out the time differences,
//! and logs them as `sdr.tdoa` for every site to use in its fixes. Snippets are dropped
//! from the blob store a week later, so the recordings don't pile up.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use rusqlite::params;

use ridgewarden_client::util::now_ms;

use crate::app::App;
use crate::capture::{self, Cap};
use crate::geo::LatLon;
use crate::iq;
use crate::tdoa::{self, End, Opts, Solution};

/// How long after a capture snippets are still fetched and solved.
const SOLVE_FOR_MS: i64 = 6 * 3_600_000;
/// Snippets are kept this long, then dropped here and from the blob store.
pub const KEEP_MS: i64 = 7 * 86_400_000;

/// Fetch, solve and publish whatever is new; returns how many results were published.
pub async fn round(app: &App) -> Result<usize> {
    let now = now_ms();
    let caps: Vec<Cap> = capture::between(&app.db(), now - SOLVE_FOR_MS, now - 30_000)?
        .into_iter()
        .filter(|c| c.req.reference.is_some())
        .collect();
    let mut published = 0;
    for cap in caps {
        fetch_missing(app, &cap).await?;
        if solve(app, &cap).await? {
            published += 1;
        }
    }
    Ok(published)
}

async fn fetch_missing(app: &App, cap: &Cap) -> Result<()> {
    let (notes, have) = {
        let db = app.db();
        let have: HashSet<(String, String)> = tdoa::have(&db, &cap.id)?
            .into_iter()
            .map(|(k, r, _)| (k, r))
            .collect();
        (tdoa::snippets(&db, &cap.id)?, have)
    };
    for (key, note) in notes {
        if have.contains(&(key.clone(), note.receiver.clone())) || tdoa::bad(&app.db(), &note.root)?
        {
            continue;
        }
        match fetch(app, cap, &key, &note).await {
            Ok(Some(bytes)) => {
                app.keep_snippet(&cap.id, &key, &note.receiver, &note.root, &bytes)?
            }
            Ok(None) => {}
            Err(why) => {
                tracing::warn!(
                    "snippet {} from {}: {why:#}; not fetching it again",
                    &note.root[..12],
                    note.node
                );
                tdoa::mark_bad(&app.db(), &note.root)?;
            }
        }
    }
    Ok(())
}

/// One snippet's bytes once the core has them all; None while they're on their way, an
/// error if the blob isn't a snippet that station published.
async fn fetch(
    app: &App,
    cap: &Cap,
    key: &str,
    note: &tdoa::SnippetNote,
) -> Result<Option<Vec<u8>>> {
    let Some(st) = app.core.blob_status(&note.root).await? else {
        return Ok(None);
    };
    if st.author != key || !st.trusted {
        bail!("the blob was published by another station");
    }
    if st.size > iq::MAX_BYTES as u64 {
        bail!("the blob is {} bytes, too big for a snippet", st.size);
    }
    if !st.complete {
        if !st.wanted {
            app.core.blob_want(&note.root).await?;
        }
        return Ok(None);
    }
    let file = format!(
        "sdr-{}-{}-{}.iq",
        &cap.id[..12],
        &key[..key.len().min(8)],
        note.receiver
    );
    let path = std::path::PathBuf::from(app.core.blob_export(&note.root, &file).await?);
    let read = std::fs::read(&path);
    if let Err(e) = std::fs::remove_file(&path) {
        tracing::warn!("removing {}: {e}", path.display());
    }
    let bytes = read.with_context(|| format!("reading {}", path.display()))?;
    iq::decode(&bytes)?;
    Ok(Some(bytes))
}

/// Work out time differences if more snippets are in than last time. True if published.
async fn solve(app: &App, cap: &Cap) -> Result<bool> {
    let Some(r) = cap.req.reference.clone() else {
        return Ok(false);
    };
    let (notes, have, done) = {
        let db = app.db();
        (
            tdoa::snippets(&db, &cap.id)?,
            tdoa::have(&db, &cap.id)?,
            tdoa::solved(&db, &cap.id)?,
        )
    };
    if have.len() < 2 || have.len() <= done {
        return Ok(false);
    }
    let mut snips = vec![];
    for (key, receiver, data) in have.iter() {
        let Some((_, n)) = notes
            .iter()
            .find(|(k, n)| k == key && &n.receiver == receiver)
        else {
            continue;
        };
        if let Ok(s) = iq::decode(data) {
            let end = End {
                key: key.clone(),
                receiver: receiver.clone(),
                node: n.node.clone(),
                lat: n.lat,
                lon: n.lon,
            };
            snips.push((end, s));
        }
    }
    let opts = Opts {
        max_skew_s: app.cfg.tdoa.max_skew_ms as f64 / 1000.0,
        sigma_s: app.cfg.tdoa.sigma_us * 1e-6,
    };
    let n = snips.len();
    let ref_pos = LatLon::new(r.lat, r.lon);
    let (pairs, problems) =
        tokio::task::spawn_blocking(move || tdoa::analyze(&snips, ref_pos, &opts)).await?;
    let sol = Solution {
        capture: cap.id.clone(),
        pairs,
        problems,
        snippets: n,
    };
    tracing::info!(
        "capture {}: {} time differences from {n} snippets, {} problems",
        &cap.id[..12],
        sol.pairs.len(),
        sol.problems.len()
    );
    app.core
        .append_telemetry("sdr.tdoa", serde_json::to_value(&sol)?)
        .await?;
    app.db().execute(
        "INSERT INTO iq_solved (capture, n) VALUES (?1, ?2)
         ON CONFLICT(capture) DO UPDATE SET n = excluded.n",
        params![cap.id, n as i64],
    )?;
    Ok(true)
}

/// Drop snippets older than a week, here and in the core's blob store.
pub async fn tidy(app: &App) -> Result<usize> {
    let cutoff = now_ms() - KEEP_MS;
    let roots: Vec<String> = {
        let db = app.db();
        let mut st = db.prepare("SELECT DISTINCT root FROM iq_have WHERE ts < ?1")?;
        st.query_map([cutoff], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    for root in &roots {
        if let Err(e) = app.core.blob_unwant(root).await {
            tracing::debug!("dropping blob {}: {e:#}", &root[..12]);
        }
    }
    app.db()
        .execute("DELETE FROM iq_have WHERE ts < ?1", [cutoff])?;
    Ok(roots.len())
}
