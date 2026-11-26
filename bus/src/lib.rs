//! sommelier: a small coordination bus for coding agents on several machines.
//!
//! One hub (`sommelier serve`) holds an append only log of short JSON messages and
//! fans them out over WebSocket. Every frame carries exactly one message as one
//! line of JSON. See `PROTOCOL.md` for the wire format and `DESIGN.md` for the
//! decisions behind it.
//!
//! Modules:
//! * [`msg`]: the message, its kinds and roles, validation, the one line form.
//! * [`proto`]: requests a client sends and the replies the hub answers with.
//! * [`claims`]: the table of who holds which topic.
//! * [`tokens`]: the member token file the hub owner edits.
//! * [`hub`]: the server.
//! * [`client`]: connecting, one shot requests, the reconnecting tail.
//! * [`time`]: timestamps without a date crate.

pub mod agent;
pub mod brief;
pub mod bundle;
pub mod claims;
pub mod cli;
pub mod client;
pub mod derive;
pub mod door;
pub mod exit;
pub mod factory;
pub mod feature;
pub mod gate;
pub mod guide;
pub mod ha;
pub mod hooks;
pub mod hub;
pub mod intake;
pub mod key;
pub mod land;
pub mod msg;
pub mod plan;
pub mod policy;
pub mod proto;
pub mod quota;
pub mod regress;
pub mod report;
pub mod review;
pub mod role;
pub mod roster;
pub mod stamp;
pub mod store;
pub mod task;
pub mod tester;
pub mod time;
pub mod tokens;
pub mod verify;
pub mod web;

/// Default port of the hub.
pub const DEFAULT_PORT: u16 = 7300;

/// The hub data folder when `--data` is not given: `SOMMELIER_DATA`, else
/// `%LOCALAPPDATA%\sommelier` on Windows, else `$XDG_DATA_HOME/sommelier`, else
/// `~/.local/share/sommelier`, else `./sommelier-data` as a last resort.
pub fn default_data_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(d) = env("SOMMELIER_DATA") {
        return d;
    }
    if cfg!(windows) {
        if let Some(d) = env("LOCALAPPDATA") {
            return d.join("sommelier");
        }
    } else if let Some(d) = env("XDG_DATA_HOME") {
        return d.join("sommelier");
    } else if let Some(h) = env("HOME") {
        return h.join(".local").join("share").join("sommelier");
    }
    PathBuf::from("sommelier-data")
}

/// Locks a std mutex and keeps going if another thread panicked while holding it.
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
