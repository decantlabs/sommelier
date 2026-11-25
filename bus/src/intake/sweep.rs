//! Deletes the files of submissions that are over, so the store does not grow for ever.
//!
//! A submission is over when it landed, was rejected or failed (the gates, the clean build or the
//! review). Its files, the bundle, the diff and the gate report, stay for `keep_days` and then go. The
//! rows and the FACTs stay: they are the record. The sweep runs on the tick, at most once a minute, and
//! never holds the state lock while it touches files.

use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::facts::query_ended_before;
use crate::hub::Hub;

/// Rows looked at in one sweep. The next sweep takes the next ones.
const PER_SWEEP: usize = 200;

/// The time between two sweeps.
const EVERY_MS: u64 = 60_000;

/// The files that belong to one submission.
const SUFFIXES: [&str; 5] = [
    ".bundle",
    ".bundle.part",
    ".diff.json",
    ".gate.json",
    ".part",
];

/// Removes the files of one submission. Returns how many files went.
fn remove_files(dir: &Path, sha: &str) -> usize {
    SUFFIXES
        .iter()
        .filter(|s| fs::remove_file(dir.join(format!("{sha}{s}"))).is_ok())
        .count()
}

/// One sweep, at most once a minute. Returns the number of files deleted.
pub fn on_tick(hub: &Hub, now_ms: u64) -> usize {
    let cfg = &hub.config().intake;
    if cfg.keep_days == 0 {
        return 0;
    }
    let last = cfg.runtime.last_sweep.load(Ordering::Relaxed);
    if last != 0 && now_ms.saturating_sub(last) < EVERY_MS {
        return 0;
    }
    cfg.runtime.last_sweep.store(now_ms, Ordering::Relaxed);
    sweep_now(hub, now_ms)
}

/// One sweep without the time check, for tests.
pub fn sweep_now(hub: &Hub, now_ms: u64) -> usize {
    let cfg = &hub.config().intake;
    let before = (now_ms / 1000).saturating_sub(cfg.keep_days.saturating_mul(86_400));
    let rows = hub
        .with_store(|s| query_ended_before(s.connection(), before, PER_SWEEP))
        .unwrap_or_default();
    let dir = cfg.store_path(&hub.config().data_dir);
    // A submission in the gate run keeps its files: the rows here are all over, so none runs.
    rows.iter().map(|r| remove_files(&dir, &r.sha256)).sum()
}
