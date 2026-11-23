//! The running app: its config, its database, and its line to the core.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use ridgewarden_client::core::{Projection, Whoami};
use ridgewarden_client::util::now_ms;
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::{Connection, params};

use crate::beacon::Beacons;
use crate::capture::Captures;
use crate::config::Config;
use crate::directory::Directory;
use crate::kraken::Bearings;
use crate::tdoa::Tdoa;

pub const PROJECTIONS: &[&dyn Projection] = &[&Directory, &Captures, &Bearings, &Tdoa, &Beacons];

/// This site's own bookkeeping, kept when the projections are rebuilt.
const LOCAL: &str = "
CREATE TABLE IF NOT EXISTS runs (
    capture    TEXT NOT NULL,
    receiver   TEXT NOT NULL,
    started_ms INTEGER NOT NULL,
    ok         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (capture, receiver)
);
CREATE TABLE IF NOT EXISTS published (
    name TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    ts   INTEGER NOT NULL
);";

pub fn init_local(conn: &Connection) -> Result<()> {
    conn.execute_batch(LOCAL)?;
    conn.execute_batch(crate::tdoa::LOCAL)?;
    Ok(())
}

pub struct App {
    pub cfg: Config,
    pub core: CoreClient,
    pub db: Mutex<Connection>,
    /// for checking receivers' web pages
    pub http: reqwest::Client,
    who: Mutex<Option<Whoami>>,
    /// one capture at a time per receiver; the next waits its turn
    locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// last capture form post per address
    pub posts: Mutex<HashMap<IpAddr, Instant>>,
    /// recent wrong form keys from anywhere, to slow down guessing from many addresses
    pub failures: Mutex<VecDeque<Instant>>,
}

impl App {
    pub fn new(cfg: Config) -> Result<Self> {
        let token = cfg
            .core
            .token_file
            .exists()
            .then_some(cfg.core.token_file.as_path());
        if token.is_none() {
            tracing::warn!(
                "core token {} not found; receivers, requests and readings can't be published",
                cfg.core.token_file.display()
            );
        }
        let core = CoreClient::new(&cfg.core.url, token)?;
        let locks = cfg
            .receivers
            .iter()
            .map(|r| (r.name.clone(), Arc::new(tokio::sync::Mutex::new(()))))
            .collect();
        let db = open_db(&cfg.db_path(), PROJECTIONS)?;
        init_local(&db)?;
        Ok(Self {
            cfg,
            core,
            db: Mutex::new(db),
            http: http_client()?,
            who: Mutex::new(None),
            locks,
            posts: Mutex::new(HashMap::new()),
            failures: Mutex::new(VecDeque::new()),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The core's identity, cached once it knows its node.
    pub async fn whoami(&self) -> Result<Whoami> {
        if let Some(w) = self.who.lock().unwrap().clone()
            && !w.node.is_empty()
        {
            return Ok(w);
        }
        let w = self.core.whoami().await?;
        *self.who.lock().unwrap() = Some(w.clone());
        Ok(w)
    }

    /// Note that this site is running `capture` on `receiver`; false if it already has.
    pub fn claim_run(&self, capture: &str, receiver: &str, now: i64) -> Result<bool> {
        let n = self.db().execute(
            "INSERT OR IGNORE INTO runs (capture, receiver, started_ms) VALUES (?1, ?2, ?3)",
            params![capture, receiver, now],
        )?;
        Ok(n == 1)
    }

    pub fn ran_ok(&self, capture: &str, receiver: &str) -> Result<()> {
        self.db().execute(
            "UPDATE runs SET ok = 1 WHERE capture = ?1 AND receiver = ?2",
            params![capture, receiver],
        )?;
        Ok(())
    }

    /// The lock a capture holds while it uses `receiver`.
    pub fn lock(&self, receiver: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks[receiver].clone()
    }

    /// Keep a snippet for solving, ours or fetched.
    pub fn keep_snippet(
        &self,
        capture: &str,
        key: &str,
        receiver: &str,
        root: &str,
        data: &[u8],
    ) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO iq_have (capture, author_key, receiver, root, ts, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![capture, key, receiver, root, now_ms(), data],
        )?;
        Ok(())
    }
}

/// The client for the receivers' own web pages and APIs: no proxy, no redirects, 10 s.
pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("ridgewarden-sdr/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

pub fn short_err(e: &anyhow::Error) -> String {
    if let Some(r) = e.downcast_ref::<reqwest::Error>() {
        if r.is_timeout() {
            return "timed out".into();
        }
        if r.is_connect() {
            return "could not connect".into();
        }
    }
    format!("{e:#}").chars().take(160).collect()
}
