//! The front door: what the hub knows about where a connection comes from, and the limits that follow.
//!
//! See `docs/factory.md` sections 5.3, 11.3 and 12.1. Network addresses are personal data. The raw
//! address of a client is used for one HMAC in memory, then dropped. Only an opaque block (the /24 of an
//! IPv4 address, the /48 of an IPv6 address) is kept, in the side store `door.db` that is never part of
//! `state.db`, never logged and never served. Other modules get only the block of a member through
//! [`block_of`].

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::lock;
use crate::msg::{Kind, Message, Role};
use crate::review::hmac_sha256;
use crate::stamp::tier_table::tier_from_table;

/// Settings of the front door. Lives in `HubConfig.door`. Everything is off by default: a zero
/// disables the limit, `trust_header: None` ignores every header, `keep_days: 0` records no blocks.
#[derive(Clone, Debug)]
pub struct DoorConfig {
    /// Name of a header such as `CF-Connecting-IP` that carries the client address. It is honoured
    /// only when the socket peer is the loopback address, which is the case behind a tunnel.
    pub trust_header: Option<String>,
    /// Joins one address block may try per hour. 0 is no limit.
    pub joins_per_block_per_hour: u32,
    /// Connections one member may hold at once. 0 is no limit.
    pub conns_per_member: u32,
    /// Connections the hub holds at most. 0 is no limit.
    pub max_conns: u32,
    /// The share of `max_conns` that tier 0 and anonymous readers can never fill, from 0 to 1.
    pub reserved_share: f64,
    /// Whether a WebSocket or `/api/state` without credentials reads as an anonymous observer.
    pub public_read: bool,
    /// Connections and state reads per address block and minute for the public reader. 0 is no limit.
    pub public_reads_per_block_per_min: u32,
    /// Days a block stays in `door.db`. 0 records no block at all.
    pub keep_days: u32,
    /// Refused joins within `join_flood_window_secs` that raise the alert JOIN_FLOOD. 0 is no alert.
    pub join_flood_refusals: u32,
    pub join_flood_window_secs: u64,
    /// Plain HTTP connections the hub holds at most while it reads and answers a request head. They
    /// are closed after one answer, a WebSocket leaves this count once it is upgraded. 0 is no limit.
    pub max_http_conns: u32,
    /// Plain HTTP connections one address block may hold. A loopback peer without a trusted header
    /// is not counted. 0 is no limit.
    pub http_conns_per_block: u32,
    /// Seconds a connection may take to send its whole request head. Default 10.
    pub head_timeout_secs: u64,
    /// Bytes of upload bodies the hub holds in memory at once. 0 is no limit.
    pub max_upload_bytes_in_flight: u64,
    /// Failed join tries per address block and hour, counted apart from the joins that went through.
    /// 0 is no limit.
    pub bad_joins_per_block_per_hour: u32,
    /// The public addresses of this hub as a client types them: `host`, which stands for the
    /// default ports 443 and 80, or `host:port`. A key signs the address it connected to, and the hub
    /// takes the sign in only when that address is listed here or is a loopback address. Empty: only
    /// loopback sign ins work.
    pub public_addresses: Vec<String>,
    /// A file that holds the secret of the address blocks (64 hex characters), made at the first
    /// start when it is missing. `None` derives the secret from `challenge_secret` in the data folder.
    pub secret_file: Option<std::path::PathBuf>,
}

impl Default for DoorConfig {
    fn default() -> Self {
        DoorConfig {
            trust_header: None,
            joins_per_block_per_hour: 0,
            conns_per_member: 0,
            max_conns: 0,
            reserved_share: 0.25,
            public_read: false,
            public_reads_per_block_per_min: 60,
            keep_days: 0,
            join_flood_refusals: 20,
            join_flood_window_secs: 600,
            max_http_conns: 512,
            http_conns_per_block: 32,
            head_timeout_secs: 10,
            max_upload_bytes_in_flight: 64 * 1024 * 1024,
            bad_joins_per_block_per_hour: 30,
            public_addresses: Vec::new(),
            secret_file: None,
        }
    }
}

/// Seconds a refused connection is told to wait.
const CONN_RETRY_SECS: u64 = 10;
const HOUR_MS: u64 = 3_600_000;
const MINUTE_MS: u64 = 60_000;
/// Window maps larger than this are cleaned at once, not only on the tick.
const WINDOW_MAP_SOFT_CAP: usize = 10_000;

/// The address the limits and the block apply to: the socket peer, or the value of the trusted
/// header when the peer is the loopback address. A header that does not parse is ignored. When the
/// header lists several addresses (a proxy chain) the last one counts, the one the nearest proxy added.
pub fn client_ip(peer: IpAddr, trust_header: Option<&str>, header_value: Option<&str>) -> IpAddr {
    if trust_header.is_none() || !peer.to_canonical().is_loopback() {
        return peer;
    }
    header_value
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

/// The opaque block of an address: HMAC of the /24 (IPv4) or the /48 (IPv6) under the hub secret,
/// cut to 16 hex characters. The address is not kept.
pub fn block_id(secret: &[u8], ip: IpAddr) -> String {
    let mut data: Vec<u8> = b"door-block-v1".to_vec();
    match ip.to_canonical() {
        IpAddr::V4(a) => {
            data.push(4);
            data.extend_from_slice(&a.octets()[..3]);
        }
        IpAddr::V6(a) => {
            data.push(6);
            data.extend_from_slice(&a.octets()[..6]);
        }
    }
    hmac_sha256(secret, &data)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A FACT of the quota or spend meters. The public reader never gets one.
pub fn hidden_from_public(msg: &Message) -> bool {
    msg.kind == Kind::Fact && (msg.topic.starts_with("quota/") || msg.topic.starts_with("spend/"))
}

/// Cuts the `/api/state` object down for a reader without a token: no meter rows, no spend, no
/// estimates, each pool with its state and reset time only, no `quota/` or `spend/` FACT in `recent`.
pub fn redact_state(state: &mut Value) {
    let Some(root) = state.as_object_mut() else {
        return;
    };
    for key in ["quota", "spend", "estimates"] {
        root.remove(key);
    }
    if let Some(Value::Array(pools)) = root.get_mut("pools") {
        for p in pools.iter_mut() {
            let keep = |k: &str| p.get(k).cloned().unwrap_or(Value::Null);
            *p = json!({
                "pool": keep("pool"),
                "window": keep("window"),
                "state": keep("state"),
                "resets_at": keep("resets_at"),
            });
        }
    }
    if let Some(Value::Array(recent)) = root.get_mut("recent") {
        recent.retain(|m| {
            let topic = m.get("topic").and_then(Value::as_str).unwrap_or("");
            let fact = m.get("kind").and_then(Value::as_str) == Some("FACT");
            !(fact && (topic.starts_with("quota/") || topic.starts_with("spend/")))
        });
    }
}

#[derive(Default)]
struct DState {
    /// Accepted join tries per block, newest last (epoch ms).
    joins: HashMap<String, VecDeque<u64>>,
    /// Public reads per block (epoch ms).
    reads: HashMap<String, VecDeque<u64>>,
    /// Refused joins, newest last (epoch ms).
    refusals: VecDeque<u64>,
    /// When the JOIN_FLOOD of the running window was raised.
    alerted_at: Option<u64>,
    /// Open connections per member name.
    members: HashMap<String, u32>,
    total: u32,
    /// Open connections of tier 0 or anonymous.
    low: u32,
    next_purge: u64,
    /// Plain HTTP connections in their head and answer phase, in total and per block.
    http_total: u32,
    http_blocks: HashMap<String, u32>,
    /// Bytes of upload bodies reserved right now.
    upload_bytes: u64,
    /// Failed join tries per block, newest last (epoch ms).
    bad_joins: HashMap<String, VecDeque<u64>>,
}

struct Inner {
    cfg: DoorConfig,
    secret: [u8; 32],
    state: Mutex<DState>,
    db: Mutex<Option<Connection>>,
}

/// The door of one hub. Cheap to clone.
#[derive(Clone)]
pub struct Door {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Door {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Door")
    }
}

/// One open connection counted by the door. Dropping it gives the place back.
pub struct Slot {
    door: Door,
    member: String,
    low: bool,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut s = lock(&self.door.inner.state);
        s.total = s.total.saturating_sub(1);
        if self.low {
            s.low = s.low.saturating_sub(1);
        }
        if !self.member.is_empty()
            && let Some(n) = s.members.get_mut(&self.member)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                s.members.remove(&self.member);
            }
        }
    }
}

/// One plain HTTP connection counted by the door. Dropping it gives the place back.
pub struct HttpSlot {
    door: Door,
    block: Option<String>,
}

impl Drop for HttpSlot {
    fn drop(&mut self) {
        let mut s = lock(&self.door.inner.state);
        s.http_total = s.http_total.saturating_sub(1);
        if let Some(b) = self.block.take()
            && let Some(n) = s.http_blocks.get_mut(&b)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                s.http_blocks.remove(&b);
            }
        }
    }
}

impl HttpSlot {
    /// Counts the connection against `block` too, once. Refuses when the block holds too many.
    pub fn bind_block(&mut self, block: &str) -> Result<(), String> {
        if self.block.is_some() {
            return Ok(());
        }
        let limit = self.door.inner.cfg.http_conns_per_block;
        let mut s = lock(&self.door.inner.state);
        if limit > 0 && s.http_blocks.get(block).copied().unwrap_or(0) >= limit {
            return Err(format!(
                "connection limit: {limit} plain HTTP connections per address block"
            ));
        }
        *s.http_blocks.entry(block.to_owned()).or_default() += 1;
        self.block = Some(block.to_owned());
        Ok(())
    }
}

/// Bytes of an upload body reserved in memory. Dropping it gives them back.
pub struct UploadGuard {
    door: Door,
    bytes: u64,
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        let mut s = lock(&self.door.inner.state);
        s.upload_bytes = s.upload_bytes.saturating_sub(self.bytes);
    }
}

fn retry_in(oldest: u64, window_ms: u64, now: u64) -> u64 {
    (oldest + window_ms)
        .saturating_sub(now)
        .div_ceil(1000)
        .max(1)
}

impl Door {
    /// Opens the door. `secret` is derived from the hub secret by the caller. `door.db` is created in
    /// `data_dir` only when blocks are kept at all.
    pub fn open(cfg: DoorConfig, secret: [u8; 32], data_dir: &Path) -> Door {
        let db = if cfg.keep_days > 0 {
            match open_db(&data_dir.join("door.db")) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("sommelier: the front door keeps no address blocks, {e}");
                    None
                }
            }
        } else {
            None
        };
        Door {
            inner: Arc::new(Inner {
                cfg,
                secret,
                state: Mutex::new(DState::default()),
                db: Mutex::new(db),
            }),
        }
    }

    pub fn config(&self) -> &DoorConfig {
        &self.inner.cfg
    }

    pub fn public_read(&self) -> bool {
        self.inner.cfg.public_read
    }

    /// The address a request counts for, see [`client_ip`].
    pub fn client_ip(&self, peer: IpAddr, header_value: Option<&str>) -> IpAddr {
        client_ip(peer, self.inner.cfg.trust_header.as_deref(), header_value)
    }

    /// Lower case name of the trusted header, if any.
    pub fn trust_header(&self) -> Option<String> {
        self.inner
            .cfg
            .trust_header
            .as_ref()
            .map(|h| h.to_ascii_lowercase())
    }

    pub fn block(&self, ip: IpAddr) -> String {
        block_id(&self.inner.secret, ip)
    }

    /// Counts one join try of `block`. Refuses over the limit and names the time to retry.
    pub fn charge_join(&self, block: &str, now_ms: u64) -> Result<(), String> {
        let limit = self.inner.cfg.joins_per_block_per_hour as usize;
        if limit == 0 {
            return Ok(());
        }
        let mut s = lock(&self.inner.state);
        self.purge(&mut s, now_ms);
        let q = s.joins.entry(block.to_owned()).or_default();
        while q.front().is_some_and(|t| *t + HOUR_MS <= now_ms) {
            q.pop_front();
        }
        if q.len() >= limit {
            let wait = retry_in(q[0], HOUR_MS, now_ms);
            s.refusals.push_back(now_ms);
            return Err(format!(
                "join limit: {limit} joins per hour from one address block, try again in {wait} s"
            ));
        }
        q.push_back(now_ms);
        Ok(())
    }

    /// Counts one plain HTTP connection. `block` is the block of the socket peer when it is known at
    /// accept time. Refuses over the total or over the block, the caller closes the socket.
    pub fn admit_http(&self, block: Option<&str>) -> Result<HttpSlot, String> {
        let cfg = &self.inner.cfg;
        {
            let mut s = lock(&self.inner.state);
            if cfg.max_http_conns > 0 && s.http_total >= cfg.max_http_conns {
                return Err(format!(
                    "connection limit: the hub holds {} plain HTTP connections",
                    cfg.max_http_conns
                ));
            }
            s.http_total += 1;
        }
        let mut slot = HttpSlot {
            door: self.clone(),
            block: None,
        };
        if let Some(b) = block {
            slot.bind_block(b)?;
        }
        Ok(slot)
    }

    /// Reserves `bytes` of upload body memory. Refuses over `max_upload_bytes_in_flight`.
    pub fn reserve_upload(&self, bytes: u64) -> Result<UploadGuard, String> {
        let limit = self.inner.cfg.max_upload_bytes_in_flight;
        let mut s = lock(&self.inner.state);
        if limit > 0 && s.upload_bytes.saturating_add(bytes) > limit {
            return Err(format!(
                "the hub holds {limit} bytes of uploads in flight already, try again in {CONN_RETRY_SECS} s"
            ));
        }
        s.upload_bytes += bytes;
        Ok(UploadGuard {
            door: self.clone(),
            bytes,
        })
    }

    /// Refuses a join try of `block` that has failed too often in the last hour. Nothing is counted.
    pub fn check_bad_joins(&self, block: &str, now_ms: u64) -> Result<(), String> {
        let limit = self.inner.cfg.bad_joins_per_block_per_hour as usize;
        if limit == 0 {
            return Ok(());
        }
        let mut s = lock(&self.inner.state);
        self.purge(&mut s, now_ms);
        let Some(q) = s.bad_joins.get_mut(block) else {
            return Ok(());
        };
        while q.front().is_some_and(|t| *t + HOUR_MS <= now_ms) {
            q.pop_front();
        }
        if q.len() >= limit {
            let wait = retry_in(q[0], HOUR_MS, now_ms);
            s.refusals.push_back(now_ms);
            return Err(format!(
                "join limit: {limit} failed join tries per hour from one address block, try again in {wait} s"
            ));
        }
        Ok(())
    }

    /// Counts one failed join try of `block`, apart from the joins that went through.
    pub fn note_bad_join(&self, block: &str, now_ms: u64) {
        if self.inner.cfg.bad_joins_per_block_per_hour == 0 {
            return;
        }
        let mut s = lock(&self.inner.state);
        self.purge(&mut s, now_ms);
        s.bad_joins
            .entry(block.to_owned())
            .or_default()
            .push_back(now_ms);
    }

    /// Counts one connection or state read of the public reader.
    pub fn charge_read(&self, block: &str, now_ms: u64) -> Result<(), String> {
        let limit = self.inner.cfg.public_reads_per_block_per_min as usize;
        if limit == 0 {
            return Ok(());
        }
        let mut s = lock(&self.inner.state);
        self.purge(&mut s, now_ms);
        let q = s.reads.entry(block.to_owned()).or_default();
        while q.front().is_some_and(|t| *t + MINUTE_MS <= now_ms) {
            q.pop_front();
        }
        if q.len() >= limit {
            let wait = retry_in(q[0], MINUTE_MS, now_ms);
            return Err(format!(
                "public read limit: {limit} reads per minute from one address block, try again in {wait} s"
            ));
        }
        q.push_back(now_ms);
        Ok(())
    }

    /// Takes a place for a connection. `member` is empty for the anonymous reader. `low` is true for
    /// tier 0 and for anonymous readers: they can fill the hub only up to the part that is not reserved.
    pub fn admit(&self, member: &str, low: bool) -> Result<Slot, String> {
        let cfg = &self.inner.cfg;
        let mut s = lock(&self.inner.state);
        if cfg.conns_per_member > 0
            && !member.is_empty()
            && s.members.get(member).copied().unwrap_or(0) >= cfg.conns_per_member
        {
            return Err(format!(
                "connection limit: {} connections per member, close one or try again in {CONN_RETRY_SECS} s",
                cfg.conns_per_member
            ));
        }
        if cfg.max_conns > 0 {
            let max = cfg.max_conns;
            let reserved =
                ((f64::from(max) * cfg.reserved_share.clamp(0.0, 1.0)).ceil() as u32).min(max);
            let full = if low {
                s.low >= max - reserved || s.total >= max
            } else {
                s.total >= max
            };
            if full {
                return Err(format!(
                    "connection limit: the hub holds {max} connections and {reserved} of them are kept for members above tier 0, try again in {CONN_RETRY_SECS} s"
                ));
            }
        }
        s.total += 1;
        if low {
            s.low += 1;
        }
        if !member.is_empty() {
            *s.members.entry(member.to_owned()).or_default() += 1;
        }
        Ok(Slot {
            door: self.clone(),
            member: member.to_owned(),
            low,
        })
    }

    fn purge(&self, s: &mut DState, now: u64) {
        let over = s.joins.len() + s.reads.len() + s.bad_joins.len() > WINDOW_MAP_SOFT_CAP;
        if now < s.next_purge && !over {
            return;
        }
        s.next_purge = now + MINUTE_MS;
        s.joins
            .retain(|_, q| q.back().is_some_and(|t| *t + HOUR_MS > now));
        s.reads
            .retain(|_, q| q.back().is_some_and(|t| *t + MINUTE_MS > now));
    }

    /// Notes that `member` was seen in `block`. A no-op while no blocks are kept.
    pub fn note_seen(&self, member: &str, block: &str, now_ms: u64) {
        let db = lock(&self.inner.db);
        let Some(conn) = db.as_ref() else { return };
        let now = (now_ms / 1000) as i64;
        let _ = conn.execute(
            "INSERT INTO SEEN (member, block, first_seen, last_seen) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(member, block) DO UPDATE SET last_seen = ?3",
            params![member, block, now],
        );
    }

    /// The block `member` was seen in last.
    pub fn block_of(&self, member: &str) -> Option<String> {
        let db = lock(&self.inner.db);
        db.as_ref()?
            .query_row(
                "SELECT block FROM SEEN WHERE member = ?1 ORDER BY last_seen DESC, rowid DESC LIMIT 1",
                params![member],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None)
    }

    /// Drops old rows and raises the alert JOIN_FLOOD, once per window, with counts and no address.
    pub fn tick(&self, now_ms: u64) -> Vec<Message> {
        let cfg = &self.inner.cfg;
        if cfg.keep_days > 0 {
            let cutoff = (now_ms / 1000).saturating_sub(u64::from(cfg.keep_days) * 86_400) as i64;
            if let Some(conn) = lock(&self.inner.db).as_ref() {
                let _ = conn.execute("DELETE FROM SEEN WHERE last_seen < ?1", params![cutoff]);
            }
        }
        let mut out = Vec::new();
        let mut s = lock(&self.inner.state);
        self.purge(&mut s, now_ms);
        let window = cfg.join_flood_window_secs.saturating_mul(1000);
        while s.refusals.front().is_some_and(|t| *t + window <= now_ms) {
            s.refusals.pop_front();
        }
        if s.alerted_at.is_some_and(|t| t + window <= now_ms) {
            s.alerted_at = None;
        }
        let count = s.refusals.len();
        if cfg.join_flood_refusals > 0
            && count >= cfg.join_flood_refusals as usize
            && s.alerted_at.is_none()
        {
            s.alerted_at = Some(now_ms);
            let at = now_ms / 1000;
            let text = format!(
                "{count} joins were refused in {} s, the join limit per address block is {} per hour",
                cfg.join_flood_window_secs, cfg.joins_per_block_per_hour
            );
            let body = json!({
                "v": 1,
                "code": "JOIN_FLOOD",
                "key": "join",
                "severity": "warn",
                "state": "open",
                "opened_at": at,
                "last_at": at,
                "count": count,
                "text": text,
                "data": {
                    "refused": count,
                    "window_secs": cfg.join_flood_window_secs,
                    "limit_per_block_per_hour": cfg.joins_per_block_per_hour,
                    "blocks_tracked": s.joins.len(),
                }
            })
            .to_string();
            out.push(Message {
                id: 0,
                ts: String::new(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Alert,
                topic: "alert/JOIN_FLOOD/join".into(),
                body,
                refs: vec!["state:open".into()],
                to: vec![],
            });
        }
        out
    }
}

fn open_db(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("door.db: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS SEEN (
            member     TEXT NOT NULL,
            block      TEXT NOT NULL,
            first_seen INTEGER NOT NULL,
            last_seen  INTEGER NOT NULL,
            PRIMARY KEY (member, block)
        );",
    )
    .map_err(|e| format!("door.db: {e}"))?;
    Ok(conn)
}

/// Whether a member counts as tier 0 for the connection cap. `role` is `None` for a key member.
pub fn is_low(hub: &Hub, member: &str, role: Option<Role>) -> bool {
    hub.with_store(|s| tier_from_table(s.connection(), member, role) <= 0)
        .unwrap_or(role.is_none())
}

/// The opaque address block a member was last seen in, or `None` when unknown. Two members with the
/// same block count as one owner for reviews and for the quorum of test reports.
pub fn block_of(hub: &Hub, member: &str) -> Option<String> {
    hub.door().block_of(member)
}

/// Called on the hub tick: drops old rows and returns the alert of a join flood.
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    hub.door().tick(now_ms)
}
