use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use ridgewarden_client::util::{now_ms, short_time};

use ridgewarden_sdr::app::{App, PROJECTIONS};
use ridgewarden_sdr::bands::parse_hz;
use ridgewarden_sdr::capture::{self, Request, parse_when, rank};
use ridgewarden_sdr::config::{Config, EXAMPLE};
use ridgewarden_sdr::{hunt, kraken, locate, owrx, sim, spawn_loops, tdoa, web};

#[derive(Parser)]
#[command(
    version,
    about = "SDR directory and interference hunting for AREDN meshes (a ridgewarden app)"
)]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/sdr.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Follow the core, publish this site's receivers, run captures, serve the page
    Run,
    /// Check an OpenWebRX+ settings.json for profiles outside the amateur bands
    CheckOwrx {
        /// e.g. /var/lib/openwebrx/settings.json
        settings: PathBuf,
        /// write an amateur-only copy here, for the mesh-facing instance
        #[arg(long)]
        write: Option<PathBuf>,
        /// in the copy, move profiles that spill over a band edge back inside it
        #[arg(long, requires = "write")]
        recenter: bool,
    },
    /// Ask every site to measure a frequency at the same time (signed by this site's core)
    Measure {
        /// center frequency, e.g. 146.52M
        freq: String,
        /// bandwidth, e.g. 16k
        bw: String,
        /// how long to measure signal strength
        #[arg(long, default_value_t = 10)]
        secs: u32,
        /// minutes from now, or a UTC time like 2026-11-20T18:00:00Z
        #[arg(long, default_value = "10")]
        at: String,
        /// a [[reference]] by name: sites with an rtl_sdr also record for time differences
        #[arg(long = "ref")]
        reference: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Show what a capture found: who heard it loudest, bearings, time differences, the fix
    Result {
        /// the capture's id, or at least its first 8 characters
        id: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ridgewarden_sdr=info,ridgewarden_client=info".into()),
        )
        .init();
    // the stand-ins take the real programs' flags, which clash with ours (-c), so they're
    // parsed on their own
    let argv: Vec<_> = std::env::args_os().collect();
    match argv.get(1).and_then(|a| a.to_str()) {
        Some("sim-rtl-power") => return sim::rtl_power(sim::PowerArgs::parse_from(&argv[1..])),
        Some("sim-rtl-sdr") => return sim::rtl_sdr(sim::IqArgs::parse_from(&argv[1..])),
        Some("sim-kraken") => {
            let a = sim::KrakenArgs::parse_from(&argv[1..]);
            return tokio::runtime::Runtime::new()?.block_on(sim::kraken(a));
        }
        _ => {}
    }
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
        Cmd::CheckOwrx {
            settings,
            write,
            recenter,
        } => check_owrx(&settings, write, recenter),
        Cmd::Measure {
            freq,
            bw,
            secs,
            at,
            reference,
            note,
        } => measure(
            Config::load(&cli.config)?,
            &freq,
            &bw,
            secs,
            &at,
            reference,
            &note,
        ),
        Cmd::Result { id } => {
            let app = App::new(Config::load(&cli.config)?)?;
            tokio::runtime::Runtime::new()?.block_on(async {
                ridgewarden_client::follow(&app.core, &app.db, PROJECTIONS)
                    .await
                    .context("reading the core's feed")
            })?;
            print_result(&app, &id)
        }
    }
}

fn measure(
    cfg: Config,
    freq: &str,
    bw: &str,
    secs: u32,
    at: &str,
    reference: Option<String>,
    note: &str,
) -> Result<()> {
    let now = now_ms();
    let at = parse_when(at, now).map_err(|e| anyhow!(e))?;
    let mut req = Request::new(&cfg, parse_hz(freq)?, parse_hz(bw)?, secs, at, note, now)
        .map_err(|e| anyhow!(e))?;
    if let Some(name) = reference {
        let r = cfg.reference(&name).with_context(|| {
            let known: Vec<&str> = cfg.references.iter().map(|r| r.name.as_str()).collect();
            format!(
                "no [[reference]] named {name:?}; this site has: {}",
                known.join(", ")
            )
        })?;
        req = req
            .with_reference(&cfg, hunt::reference_tx(r))
            .map_err(|e| anyhow!(e))?;
    }
    let app = App::new(cfg)?;
    let e = tokio::runtime::Runtime::new()?
        .block_on(app.core.append("sdr.capture", serde_json::to_value(&req)?))?;
    println!(
        "Capture {} requested: {}, starting {}.",
        &e.id[..12],
        req.summary(),
        short_time(req.at_ms())
    );
    if at - now < 5 * 60_000 {
        println!(
            "That is soon: sites that sync after the start time will skip it. \
             Five minutes or more gives the request time to spread."
        );
    }
    println!("Results: ridgewarden-sdr result {}", &e.id[..12]);
    Ok(())
}

fn check_owrx(settings: &Path, write: Option<PathBuf>, recenter: bool) -> Result<()> {
    let s = owrx::load(settings)?;
    print!("{}", owrx::report(&s)?);
    let Some(out) = write else {
        return Ok(());
    };
    if out.canonicalize().ok() == settings.canonicalize().ok() {
        bail!(
            "write the copy for the mesh-facing instance somewhere else; {} belongs to the LAN instance",
            settings.display()
        );
    }
    let (copy, changes) = owrx::filtered(&s, recenter)?;
    for c in &changes {
        println!("{c}");
    }
    let tmp = out.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&copy)? + "\n")
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &out).with_context(|| format!("writing {}", out.display()))?;
    println!(
        "Wrote {} with {} profiles. Restart that OpenWebRX+ instance to use it.",
        out.display(),
        owrx::profiles(&copy)?.len()
    );
    Ok(())
}

fn print_result(app: &App, id: &str) -> Result<()> {
    let db = app.db();
    let Some(cap) = capture::find(&db, id)? else {
        bail!("no capture starts with {id:?}; give at least 8 characters of its id");
    };
    let r = rank(capture::readings(&db, &cap.id)?);
    println!(
        "{} ({}), asked by {}, start {}",
        cap.req.summary(),
        &cap.id[..12],
        cap.call,
        short_time(cap.req.at_ms())
    );
    if !r.ranked.is_empty() {
        println!("Ranked by {}. {}", r.scale.unit(), r.scale.explain());
    }
    for (i, (row, v)) in r.ranked.iter().enumerate() {
        let gain = row.r.gain_db.map_or("auto".into(), |g| format!("{g} dB"));
        println!(
            "{:>2}. {:>7.1}  {} {} (gain {gain})",
            i + 1,
            v,
            app.cfg.label(&row.r.node),
            row.r.receiver
        );
    }
    for row in &r.other {
        println!(
            "    {}  {} {}: {}",
            row.r.status,
            app.cfg.label(&row.r.node),
            row.r.receiver,
            row.r.detail
        );
    }
    for (call, b) in kraken::bearings(&db, &cap.id)? {
        match (b.bearing_deg, b.sigma_deg) {
            (Some(d), Some(s)) => println!(
                "Bearing from {} {} ({call}): {d:.1} +- {s:.1} degrees",
                app.cfg.label(&b.node),
                b.kraken
            ),
            _ => println!(
                "Bearing from {} {} ({call}): {} {}",
                app.cfg.label(&b.node),
                b.kraken,
                b.status,
                b.detail
            ),
        }
    }
    if let Some((call, sol)) = tdoa::result(&db, &cap.id)? {
        println!(
            "Time differences from {} snippets, worked out by {call}:",
            sol.snippets
        );
        for p in &sol.pairs {
            println!(
                "    {} {} to {} {}: {:+.2} +- {:.2} km",
                app.cfg.label(&p.a.node),
                p.a.receiver,
                app.cfg.label(&p.b.node),
                p.b.receiver,
                p.dd_km,
                p.sigma_km
            );
        }
        for p in &sol.problems {
            println!("    not used: {p}");
        }
    }
    match locate::capture_fix(&db, &app.cfg, &cap)?.1 {
        Ok(f) => println!(
            "Fix: {:.5}, {:.5}, 95% within {:.2} by {:.2} km (long axis {:.0} degrees){}{}",
            f.pos.lat,
            f.pos.lon,
            f.ellipse.major_km,
            f.ellipse.minor_km,
            f.ellipse.az_deg,
            if f.inside {
                ""
            } else {
                "; outside the ring of receivers"
            },
            if f.ambiguous {
                "; another spot fits as well"
            } else {
                ""
            }
        ),
        Err(why) => println!("No fix: {why}"),
    }
    if r.ranked.is_empty() && r.other.is_empty() {
        println!("No readings yet.");
    }
    Ok(())
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let app = Arc::new(App::new(cfg)?);
    tracing::info!(
        "ridgewarden-sdr {} following {}, page on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.core.base()
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(
        listener,
        web::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
