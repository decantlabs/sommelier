//! The private side store of the test network: the address block of the sender of each report and
//! the run orders handed out by `test.next`.
//!
//! It is its own SQLite file in the data folder, next to `state.db`. It is never part of
//! `state.db`, never logged and never served: the public matrix gets only counts from it. A report
//! counts toward a quorum, a stamp or a regression only when its row here holds a block. Hand outs
//! live here too, so a restart of the hub does not forget them.
//!
//! One statement or one transaction decides each question that two requests could race for, so a
//! member cannot send two reports for one order.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// File name of the side store, in the data folder.
pub const SIDE_FILE: &str = "report_side.db";

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS HANDED (
    member TEXT NOT NULL,
    run TEXT NOT NULL,
    machine_key TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    PRIMARY KEY (member, run)
);
CREATE TABLE IF NOT EXISTS SENT (
    run TEXT NOT NULL,
    member TEXT NOT NULL,
    block TEXT NOT NULL,
    at_ms INTEGER NOT NULL,
    PRIMARY KEY (run, member)
);
";

/// The data folder of the hub that owns `conn` (its `state.db`). `None` for a store in memory.
pub fn data_dir_of(conn: &Connection) -> Option<PathBuf> {
    let p = conn.path()?;
    if p.is_empty() {
        return None;
    }
    Path::new(p).parent().map(Path::to_path_buf)
}

/// What the claim of a hand out answers.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// The order was handed to this member and is now used up.
    Taken,
    /// No hand out of this order to this member, or it is older than the hold time.
    NotHanded,
    /// The machine key of the report differs from the one the order was handed to.
    OtherMachine(String),
    /// The member sent a report for this order already.
    Sent,
}

/// An open side store.
pub struct Side {
    conn: Connection,
}

impl Side {
    /// Opens the store in `dir` and creates it when it is missing.
    pub fn open(dir: &Path) -> Result<Side, String> {
        let conn =
            Connection::open(dir.join(SIDE_FILE)).map_err(|e| format!("{SIDE_FILE}: {e}"))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("{SIDE_FILE}: {e}"))?;
        Ok(Side { conn })
    }

    /// Opens the store only when the file exists. For readers that must not create it.
    pub fn open_existing(dir: &Path) -> Option<Side> {
        let path = dir.join(SIDE_FILE);
        if !path.is_file() {
            return None;
        }
        Side::open(dir).ok()
    }

    /// Notes that `run` was handed to `member` for a machine. The first hand out keeps its time.
    pub fn hand(
        &self,
        member: &str,
        run: &str,
        machine_key: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO HANDED (member, run, machine_key, at_ms) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(member, run) DO UPDATE SET machine_key = excluded.machine_key",
                params![member, run, machine_key, now_ms as i64],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The runs handed to `member` with the time of the hand out in ms.
    pub fn handed_to(&self, member: &str) -> Result<BTreeMap<String, u64>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT run, at_ms FROM HANDED WHERE member = ?1")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![member], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }

    /// Forgets the hand outs of `member` that no longer count: not in `keep`, or older than `hold_ms`.
    pub fn prune_member(
        &self,
        member: &str,
        keep: &dyn Fn(&str) -> bool,
        now_ms: u64,
        hold_ms: u64,
    ) -> Result<(), String> {
        for (run, at) in self.handed_to(member)? {
            if now_ms.saturating_sub(at) > hold_ms || !keep(&run) {
                self.conn
                    .execute(
                        "DELETE FROM HANDED WHERE member = ?1 AND run = ?2",
                        params![member, run],
                    )
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// Forgets every hand out that no longer counts, for every member. Called on the tick.
    pub fn prune_all(
        &self,
        keep: &dyn Fn(&str) -> bool,
        now_ms: u64,
        hold_ms: u64,
    ) -> Result<usize, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT member, run, at_ms FROM HANDED")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?.max(0) as u64,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(stmt);
        let mut gone = 0;
        for (member, run, at) in rows {
            if now_ms.saturating_sub(at) > hold_ms || !keep(&run) {
                gone += self
                    .conn
                    .execute(
                        "DELETE FROM HANDED WHERE member = ?1 AND run = ?2",
                        params![member, run],
                    )
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(gone)
    }

    /// Uses up the hand out of `run` to `member` and records the block of the sender, in one
    /// transaction. `block` is empty when the front door knows none: the report is then a hint.
    pub fn claim(
        &mut self,
        member: &str,
        run: &str,
        machine_key: &str,
        block: &str,
        now_ms: u64,
        hold_ms: u64,
    ) -> Result<Claim, String> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let sent: bool = tx
            .query_row(
                "SELECT 1 FROM SENT WHERE run = ?1 AND member = ?2",
                params![run, member],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .unwrap_or(false);
        if sent {
            return Ok(Claim::Sent);
        }
        let hand: Option<(String, i64)> = tx
            .query_row(
                "SELECT machine_key, at_ms FROM HANDED WHERE member = ?1 AND run = ?2",
                params![member, run],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((handed_key, at)) = hand else {
            return Ok(Claim::NotHanded);
        };
        if now_ms.saturating_sub(at.max(0) as u64) > hold_ms {
            return Ok(Claim::NotHanded);
        }
        if handed_key != machine_key {
            return Ok(Claim::OtherMachine(handed_key));
        }
        tx.execute(
            "DELETE FROM HANDED WHERE member = ?1 AND run = ?2",
            params![member, run],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO SENT (run, member, block, at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![run, member, block, now_ms as i64],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(Claim::Taken)
    }

    /// Takes a claim back, when the report could not be written. The hand out comes back.
    pub fn release(
        &mut self,
        member: &str,
        run: &str,
        machine_key: &str,
        handed_at_ms: u64,
    ) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM SENT WHERE run = ?1 AND member = ?2",
            params![run, member],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR IGNORE INTO HANDED (member, run, machine_key, at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![member, run, machine_key, handed_at_ms as i64],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// The time a run was handed to a member, in ms.
    pub fn handed_at(&self, member: &str, run: &str) -> Option<u64> {
        self.conn
            .query_row(
                "SELECT at_ms FROM HANDED WHERE member = ?1 AND run = ?2",
                params![member, run],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .ok()
            .flatten()
            .map(|n| n.max(0) as u64)
    }

    /// The blocks of the reports that hold one, by (run, member). Reports without a block are not in it.
    pub fn blocks(&self) -> Result<BTreeMap<(String, String), String>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT run, member, block FROM SENT WHERE block <> ''")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }
}

/// The blocks of the reports in the data folder of `conn`. Empty when there is no side store yet.
pub fn blocks_of(conn: &Connection) -> BTreeMap<(String, String), String> {
    data_dir_of(conn)
        .and_then(|d| Side::open_existing(&d))
        .and_then(|s| s.blocks().ok())
        .unwrap_or_default()
}

/// The block the front door saw `member` in last, read from its `door.db` in the data folder. The
/// door writes it when a member connects, so for a report that arrives on a live connection it is
/// the block of the sender at this moment. `None` when the door keeps no blocks.
pub fn door_block(data_dir: &Path, member: &str) -> Option<String> {
    let path = data_dir.join("door.db");
    if !path.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.busy_timeout(Duration::from_secs(2)).ok()?;
    conn.query_row(
        "SELECT block FROM SEEN WHERE member = ?1 ORDER BY last_seen DESC, rowid DESC LIMIT 1",
        params![member],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .filter(|b| !b.is_empty())
}

/// Whether the front door of the hub in `data_dir` keeps address blocks at all.
pub fn door_keeps_blocks(data_dir: &Path) -> bool {
    data_dir.join("door.db").is_file()
}
