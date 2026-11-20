//! Test helpers: a database with every projection, and events from any station.

use ridgewarden_client::core::init_db;
use ridgewarden_client::util::ms_to_rfc3339;
use ridgewarden_client::{Author, Event};
use rusqlite::Connection;
use serde_json::Value;

use crate::app::PROJECTIONS;

pub fn init() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    init_db(&c, PROJECTIONS).unwrap();
    crate::app::init_local(&c).unwrap();
    c
}

/// Run an event from the station with key `key` through every projection. Projections
/// don't check signatures (the core already has), so none is made here.
pub fn ev(c: &mut Connection, key: &str, kind: &str, payload: Value, ts: i64) -> Event {
    let id = blake3::hash(format!("{key}{kind}{ts}{payload}").as_bytes())
        .to_hex()
        .to_string();
    let e = Event {
        id,
        author: Author {
            call: format!("N0{}", key.to_ascii_uppercase()),
            key: key.into(),
        },
        seq: 0,
        prev: None,
        lamport: 0,
        wall_time: ms_to_rfc3339(ts),
        mono_time: 0,
        kind: kind.into(),
        payload,
        sig: String::new(),
        log: None,
    };
    let tx = c.transaction().unwrap();
    for p in PROJECTIONS {
        p.apply(&tx, &e, ts).unwrap();
    }
    tx.commit().unwrap();
    e
}
