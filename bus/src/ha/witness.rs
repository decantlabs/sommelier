//! Witness key client and local backends for hub availability (section 13.5).
//!
//! The witness holds one record `{ holder, epoch, heartbeat }` in the Cloudflare
//! Worker KV (or a local atomic file / in-memory store for drills and tests).
//! Promotion takes `epoch + 1` with compare-and-set so two primaries cannot exist.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Witness record stored in Cloudflare KV (`ha:witness`) or local test witness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessRecord {
    pub holder: String,
    pub epoch: u64,
    /// UTC epoch seconds of the last primary heartbeat.
    pub heartbeat: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WitnessError {
    StaleEpoch { current: u64, sent: u64 },
    Unreachable(String),
    Unauthorized(String),
}

impl std::fmt::Display for WitnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WitnessError::StaleEpoch { current, sent } => {
                write!(f, "stale epoch {sent}, current witness epoch is {current}")
            }
            WitnessError::Unreachable(msg) => write!(f, "witness unreachable: {msg}"),
            WitnessError::Unauthorized(msg) => write!(f, "witness unauthorized: {msg}"),
        }
    }
}

/// True when the witness heartbeat is older than `witness_stale_min` minutes.
pub fn is_stale(record: &WitnessRecord, now_sec: u64, witness_stale_min: u64) -> bool {
    now_sec.saturating_sub(record.heartbeat) >= witness_stale_min.saturating_mul(60)
}

/// Witness operations used by primary heartbeat, self-fencing, and promotion.
pub trait WitnessBackend: Send + Sync {
    fn get(&self) -> Result<Option<WitnessRecord>, WitnessError>;
    fn heartbeat(
        &self,
        holder: &str,
        epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError>;
    fn promote(
        &self,
        new_holder: &str,
        expected_epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError>;
}

/// In-memory CAS witness for unit tests and drills.
#[derive(Clone, Debug, Default)]
pub struct MemoryWitness {
    inner: Arc<Mutex<MemoryWitnessState>>,
}

#[derive(Debug, Default)]
struct MemoryWitnessState {
    record: Option<WitnessRecord>,
    offline: bool,
}

impl MemoryWitness {
    pub fn new(initial: Option<WitnessRecord>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MemoryWitnessState {
                record: initial,
                offline: false,
            })),
        }
    }

    pub fn set_offline(&self, offline: bool) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).offline = offline;
    }
}

impl WitnessBackend for MemoryWitness {
    fn get(&self) -> Result<Option<WitnessRecord>, WitnessError> {
        let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.offline {
            return Err(WitnessError::Unreachable("simulated witness outage".into()));
        }
        Ok(st.record.clone())
    }

    fn heartbeat(
        &self,
        holder: &str,
        epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError> {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.offline {
            return Err(WitnessError::Unreachable("simulated witness outage".into()));
        }
        if let Some(cur) = &st.record {
            if epoch < cur.epoch {
                return Err(WitnessError::StaleEpoch {
                    current: cur.epoch,
                    sent: epoch,
                });
            }
            if epoch == cur.epoch && cur.holder != holder {
                return Err(WitnessError::StaleEpoch {
                    current: cur.epoch,
                    sent: epoch,
                });
            }
        }
        let rec = WitnessRecord {
            holder: holder.to_owned(),
            epoch,
            heartbeat: now_sec,
        };
        st.record = Some(rec.clone());
        Ok(rec)
    }

    fn promote(
        &self,
        new_holder: &str,
        expected_epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError> {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.offline {
            return Err(WitnessError::Unreachable("simulated witness outage".into()));
        }
        let cur_epoch = st.record.as_ref().map_or(0, |r| r.epoch);
        if cur_epoch != expected_epoch {
            return Err(WitnessError::StaleEpoch {
                current: cur_epoch,
                sent: expected_epoch,
            });
        }
        let rec = WitnessRecord {
            holder: new_holder.to_owned(),
            epoch: expected_epoch + 1,
            heartbeat: now_sec,
        };
        st.record = Some(rec.clone());
        Ok(rec)
    }
}

/// Atomic file-backed witness for local CLI drills (`sommelier drill` / `sommelier standby --witness file://...`).
#[derive(Clone, Debug)]
pub struct FileWitness {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl FileWitness {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    fn read_unlocked(&self) -> Result<Option<WitnessRecord>, WitnessError> {
        match fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map(Some)
                .map_err(|e| WitnessError::Unreachable(format!("corrupt witness file: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WitnessError::Unreachable(e.to_string())),
        }
    }

    fn write_unlocked(&self, rec: &WitnessRecord) -> Result<(), WitnessError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| WitnessError::Unreachable(e.to_string()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(rec)
            .map_err(|e| WitnessError::Unreachable(e.to_string()))?;
        fs::write(&tmp, text + "\n").map_err(|e| WitnessError::Unreachable(e.to_string()))?;
        fs::rename(&tmp, &self.path).map_err(|e| WitnessError::Unreachable(e.to_string()))?;
        Ok(())
    }
}

impl WitnessBackend for FileWitness {
    fn get(&self) -> Result<Option<WitnessRecord>, WitnessError> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read_unlocked()
    }

    fn heartbeat(
        &self,
        holder: &str,
        epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cur) = self.read_unlocked()?
            && (epoch < cur.epoch || (epoch == cur.epoch && cur.holder != holder))
        {
            return Err(WitnessError::StaleEpoch {
                current: cur.epoch,
                sent: epoch,
            });
        }
        let rec = WitnessRecord {
            holder: holder.to_owned(),
            epoch,
            heartbeat: now_sec,
        };
        self.write_unlocked(&rec)?;
        Ok(rec)
    }

    fn promote(
        &self,
        new_holder: &str,
        expected_epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let cur_epoch = self.read_unlocked()?.map_or(0, |r| r.epoch);
        if cur_epoch != expected_epoch {
            return Err(WitnessError::StaleEpoch {
                current: cur_epoch,
                sent: expected_epoch,
            });
        }
        let rec = WitnessRecord {
            holder: new_holder.to_owned(),
            epoch: expected_epoch + 1,
            heartbeat: now_sec,
        };
        self.write_unlocked(&rec)?;
        Ok(rec)
    }
}

/// HTTP witness talking to `cloudflare/worker.js` (`/witness`, `/witness/heartbeat`, `/witness/promote`).
#[derive(Clone, Debug)]
pub struct HttpWitness {
    pub base_url: String,
    pub token: String,
}

impl HttpWitness {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            token: token.into(),
        }
    }

    fn client(&self) -> Result<reqwest::Client, WitnessError> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| WitnessError::Unreachable(e.to_string()))
    }

    pub async fn get_async(&self) -> Result<Option<WitnessRecord>, WitnessError> {
        let client = self.client()?;
        let resp = client
            .get(format!("{}/witness", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| WitnessError::Unreachable(e.without_url().to_string()))?;
        if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
            return Err(WitnessError::Unauthorized(format!(
                "HTTP {}",
                resp.status()
            )));
        }
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(WitnessError::Unreachable(format!("HTTP {}", resp.status())));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| WitnessError::Unreachable(e.to_string()))?;
        let val: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| WitnessError::Unreachable(e.to_string()))?;
        if val["witness"].is_null() {
            return Ok(None);
        }
        serde_json::from_value(val["witness"].clone())
            .map(Some)
            .map_err(|e| WitnessError::Unreachable(e.to_string()))
    }

    pub async fn heartbeat_async(
        &self,
        holder: &str,
        epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError> {
        let client = self.client()?;
        let body = serde_json::json!({
            "holder": holder,
            "epoch": epoch,
            "heartbeat": now_sec,
        })
        .to_string();
        let resp = client
            .post(format!("{}/witness/heartbeat", self.base_url))
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| WitnessError::Unreachable(e.without_url().to_string()))?;
        parse_witness_mutation_response(resp, epoch).await
    }

    pub async fn promote_async(
        &self,
        new_holder: &str,
        expected_epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError> {
        let client = self.client()?;
        let body = serde_json::json!({
            "holder": new_holder,
            "expected_epoch": expected_epoch,
            "heartbeat": now_sec,
        })
        .to_string();
        let resp = client
            .post(format!("{}/witness/promote", self.base_url))
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| WitnessError::Unreachable(e.without_url().to_string()))?;
        parse_witness_mutation_response(resp, expected_epoch).await
    }
}

async fn parse_witness_mutation_response(
    resp: reqwest::Response,
    sent_epoch: u64,
) -> Result<WitnessRecord, WitnessError> {
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        return Err(WitnessError::Unauthorized(format!("HTTP {status}")));
    }
    if status == 409 {
        let bytes = resp.bytes().await.unwrap_or_default();
        let val: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let current = val["current_epoch"].as_u64().unwrap_or(sent_epoch + 1);
        return Err(WitnessError::StaleEpoch {
            current,
            sent: sent_epoch,
        });
    }
    if !resp.status().is_success() {
        return Err(WitnessError::Unreachable(format!("HTTP {status}")));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| WitnessError::Unreachable(e.to_string()))?;
    let val: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| WitnessError::Unreachable(e.to_string()))?;
    serde_json::from_value(val["witness"].clone())
        .map_err(|e| WitnessError::Unreachable(e.to_string()))
}

/// Unified witness handle supporting memory, file, and HTTP (Cloudflare Worker) backends.
#[derive(Clone, Debug)]
pub enum Witness {
    Memory(MemoryWitness),
    File(FileWitness),
    Http(HttpWitness),
}

impl Witness {
    pub async fn get(&self) -> Result<Option<WitnessRecord>, WitnessError> {
        match self {
            Witness::Memory(w) => w.get(),
            Witness::File(w) => w.get(),
            Witness::Http(w) => w.get_async().await,
        }
    }

    pub async fn heartbeat(
        &self,
        holder: &str,
        epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError> {
        match self {
            Witness::Memory(w) => w.heartbeat(holder, epoch, now_sec),
            Witness::File(w) => w.heartbeat(holder, epoch, now_sec),
            Witness::Http(w) => w.heartbeat_async(holder, epoch, now_sec).await,
        }
    }

    pub async fn promote(
        &self,
        new_holder: &str,
        expected_epoch: u64,
        now_sec: u64,
    ) -> Result<WitnessRecord, WitnessError> {
        match self {
            Witness::Memory(w) => w.promote(new_holder, expected_epoch, now_sec),
            Witness::File(w) => w.promote(new_holder, expected_epoch, now_sec),
            Witness::Http(w) => w.promote_async(new_holder, expected_epoch, now_sec).await,
        }
    }
}
