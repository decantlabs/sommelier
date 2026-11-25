//! Local outbox when the hub is unreachable (sections 2, 6.4, 8.4, 9, and step 7).
//!
//! When the supervisor or `agent exec` cannot reach the hub, requests are spooled in order
//! to `<data_dir>/outbox.jsonl` (with monotonic sequence numbers). When connectivity returns,
//! `flush` sends them in strict FIFO order and keeps any remaining items if the hub drops again.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::{self, Opts};
use crate::time::now_ms;

/// One spooled request in the local outbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub seq: u64,
    pub queued_at_ms: u64,
    pub request: Value,
}

/// Persistent FIFO outbox backed by an append-only JSONL file.
#[derive(Clone, Debug)]
pub struct Outbox {
    path: PathBuf,
}

impl Outbox {
    /// Opens or creates an outbox at `<data_dir>/outbox.jsonl`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("outbox.jsonl"),
        }
    }

    /// Opens an outbox at a specific file path.
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads all queued entries in FIFO order.
    pub fn list(&self) -> io::Result<Vec<OutboxEntry>> {
        let file = match fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut entries = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<OutboxEntry>(trimmed) {
                entries.push(entry);
            }
        }
        entries.sort_by_key(|e| e.seq);
        Ok(entries)
    }

    /// Number of pending requests in the outbox.
    pub fn len(&self) -> io::Result<usize> {
        Ok(self.list()?.len())
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Appends a request to the outbox and returns its assigned sequence number.
    pub fn enqueue(&self, request: Value) -> io::Result<OutboxEntry> {
        self.enqueue_at(request, now_ms())
    }

    /// Appends a request with an explicit timestamp.
    pub fn enqueue_at(&self, request: Value, queued_at_ms: u64) -> io::Result<OutboxEntry> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let existing = self.list()?;
        let next_seq = existing.last().map_or(1, |e| e.seq + 1);
        let entry = OutboxEntry {
            seq: next_seq,
            queued_at_ms,
            request,
        };
        let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
        Ok(entry)
    }

    /// Atomically rewrites the outbox file with the remaining entries.
    fn write_all(&self, entries: &[OutboxEntry]) -> io::Result<()> {
        if entries.is_empty() {
            match fs::remove_file(&self.path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        for entry in entries {
            let line = serde_json::to_string(entry).map_err(io::Error::other)?;
            writeln!(temp, "{line}")?;
        }
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Flushes queued requests in order using a custom sender closure.
    /// Stops at the first transient failure and preserves the unsent tail in order.
    pub async fn flush_with<F, Fut>(&self, mut sender: F) -> Result<usize, String>
    where
        F: FnMut(Value) -> Fut,
        Fut: std::future::Future<Output = Result<Value, String>>,
    {
        let entries = self.list().map_err(|e| e.to_string())?;
        if entries.is_empty() {
            return Ok(0);
        }
        let mut sent = 0usize;
        let mut err_msg: Option<String> = None;
        for entry in &entries {
            match sender(entry.request.clone()).await {
                Ok(_) => {
                    sent += 1;
                }
                Err(e) => {
                    err_msg = Some(e);
                    break;
                }
            }
        }
        if sent > 0 {
            self.write_all(&entries[sent..])
                .map_err(|e| e.to_string())?;
        }
        if let Some(e) = err_msg {
            return Err(e);
        }
        Ok(sent)
    }

    /// Flushes queued requests to the live hub over WebSocket in FIFO order.
    pub async fn flush(&self, opts: &Opts) -> Result<usize, String> {
        let entries = self.list().map_err(|e| e.to_string())?;
        if entries.is_empty() {
            return Ok(0);
        }
        let mut conn = client::Conn::open(opts).await?;
        let mut sent = 0usize;
        let mut err_msg: Option<String> = None;
        for entry in &entries {
            match conn.call_ok(&entry.request).await {
                Ok(_) => sent += 1,
                Err(e) => {
                    err_msg = Some(e);
                    break;
                }
            }
        }
        conn.close().await;
        if sent > 0 {
            self.write_all(&entries[sent..])
                .map_err(|e| e.to_string())?;
        }
        if let Some(e) = err_msg {
            return Err(e);
        }
        Ok(sent)
    }

    /// Tries to flush the outbox and then send `req` to the hub.
    /// If the hub cannot be reached, spools `req` to the outbox and returns `Ok(None)`
    /// (caller can map this to exit code 7 `HUB_UNREACHABLE` or continue).
    pub async fn send_or_spool(&self, opts: &Opts, req: Value) -> Result<Option<Value>, String> {
        if self.flush(opts).await.is_err() {
            self.enqueue(req).map_err(|e| e.to_string())?;
            return Ok(None);
        }
        match client::Conn::open(opts).await {
            Ok(mut conn) => {
                let res = conn.call_ok(&req).await;
                conn.close().await;
                match res {
                    Ok(v) => Ok(Some(v)),
                    Err(e) if is_transport_error(&e) => {
                        self.enqueue(req).map_err(|io_e| io_e.to_string())?;
                        Ok(None)
                    }
                    Err(e) => Err(e),
                }
            }
            Err(_) => {
                self.enqueue(req).map_err(|e| e.to_string())?;
                Ok(None)
            }
        }
    }
}

fn is_transport_error(err: &str) -> bool {
    err.contains("cannot reach the hub")
        || err.contains("hub closed the connection")
        || err.contains("no reply within")
        || err.contains("send:")
        || err.contains("receive:")
}
