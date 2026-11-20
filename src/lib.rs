//! ridgewarden-sdr: a directory of the SDRs on an AREDN mesh, and interference hunting
//! across them.
//!
//! Runs next to a ridgewarden core. Each site publishes its receivers as signed events
//! (with only amateur-band OpenWebRX+ profiles offered over RF), and every site builds the
//! same directory from the core's feed. A capture request makes every site measure one
//! frequency at the same moment: signal strength from any SDR, bearings from KrakenSDRs,
//! and time differences from IQ snippets aligned on a reference transmitter. The page
//! combines whatever came back into one fix with an error ellipse, and a weekly test
//! against a beacon at a surveyed spot publishes how far the fixes really miss.

pub mod app;
pub mod bands;
pub mod beacon;
pub mod capture;
pub mod config;
pub mod directory;
pub mod dsp;
pub mod fix;
pub mod geo;
pub mod hunt;
pub mod iq;
pub mod kraken;
pub mod locate;
pub mod map;
pub mod owrx;
pub mod publish;
pub mod rtlpower;
pub mod runner;
pub mod sim;
pub mod solver;
pub mod tdoa;
#[cfg(test)]
pub mod testing;
pub mod web;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// Run `job` every `every` seconds after `delay`, logging failures with `what`.
fn every<F, Fut>(app: &Arc<app::App>, delay: u64, every: u64, what: &'static str, job: F)
where
    F: Fn(Arc<app::App>) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send,
{
    let app = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        loop {
            if let Err(e) = job(app.clone()).await {
                tracing::warn!("{what}: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(every)).await;
        }
    });
}

/// Follow the core's feed, publish receivers, run due captures, solve time differences
/// and run the beacon test, forever.
pub fn spawn_loops(app: Arc<app::App>) {
    every(&app, 0, 10, "following the core", |a| async move {
        ridgewarden_client::follow(&a.core, &a.db, app::PROJECTIONS).await?;
        Ok(())
    });
    if !app.cfg.receivers.is_empty() {
        // after the feed has brought in the node's position
        every(
            &app,
            20,
            app.cfg.status_interval_s,
            "publishing receivers",
            |a| async move {
                let n = publish::round(&a).await?;
                if n > 0 {
                    tracing::info!("published {n} receiver descriptions");
                }
                Ok(())
            },
        );
    }
    let measures =
        app.cfg.receivers.iter().any(|r| r.capture.is_some()) || !app.cfg.krakens.is_empty();
    if measures {
        every(&app, 5, 2, "starting captures", |a| async move {
            let w = a.whoami().await?;
            if !w.node.is_empty() {
                let home = directory::home(&a.db(), &w.key)?;
                runner::round(&a, &ridgewarden_client::util::norm_node(&w.node), home)?;
            }
            Ok(())
        });
    }
    if app.cfg.tdoa.solve {
        every(&app, 30, 30, "solving time differences", |a| async move {
            solver::round(&a).await?;
            Ok(())
        });
    }
    every(&app, 300, 3600, "dropping old snippets", |a| async move {
        solver::tidy(&a).await?;
        Ok(())
    });
    if app.cfg.beacon.as_ref().is_some_and(|b| b.schedule) {
        every(&app, 60, 60, "beacon test", |a| async move {
            beacon::round(&a).await
        });
    }
}
