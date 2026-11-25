//! The hub: authenticates connections, stamps and stores messages, fans them out.
//!
//! Ordering: every message is stamped, written to the log, applied to the claim
//! table and broadcast while one lock is held, so ids are dense, the log order
//! is the delivery order, and a subscriber that registers under the same lock
//! sees every message after the id it noted, from the log or live, never twice.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HsRequest, Response as HsResponse,
};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};

use crate::claims::Claims;
use crate::key::book::{Lookup, Standing};
use crate::key::{Challenges, JoinRules, KeyBook};
use crate::lock;
use crate::msg::{
    Filter, Kind, Limits, Message, Role, check_kind_limit, check_worker_name, split_addressee,
    validate,
};
use crate::policy::{Snapshot as PolicySnapshot, Store as PolicyStore};
use crate::proto::{ConnectParams, Request, parse_request, reply_err, reply_ok};
use crate::review::hmac_sha256;
use crate::roster::{self, Roster};
use crate::time::{now_ms, rfc3339};
use crate::tokens::{Member, TokenFile};

/// Hub settings. [`HubConfig::new`] gives the defaults `sommelier serve` uses.
#[derive(Clone, Debug)]
pub struct HubConfig {
    /// Holds `bus.jsonl` (the log) and `tokens.json`.
    pub data_dir: PathBuf,
    pub policy_path: Option<PathBuf>,
    pub limits: Limits,
    /// Sustained requests per second per member name.
    pub rate_per_sec: f64,
    /// Requests a member may send at once before the rate applies.
    pub burst: f64,
    /// A claim whose holder sent nothing for this long is released by the hub.
    pub claim_ttl: Duration,
    /// A free slot silent for this long triggers an IDLE alert on tick.
    pub idle_alert_after: Duration,
    pub sweep_every: Duration,
    pub token_reload_every: Duration,
    /// WebSocket ping to every client, keeps tunnels from closing idle connections.
    pub ping_every: Duration,
    /// Print connects and disconnects to stderr.
    pub verbose: bool,
    /// Folder with the dashboard files served over plain HTTP, `None` serves none.
    pub ui_dir: Option<PathBuf>,
    /// Whether new members may join with a key. Off by default.
    pub join: JoinRules,
    /// How long a challenge nonce lives.
    pub nonce_ttl: Duration,
    /// Intake of submissions, see `intake`.
    pub intake: crate::intake::IntakeConfig,
    /// The clean build, see `verify::ops`.
    pub verify: crate::verify::ops::VerifyConfig,
    /// Assigned review, see `review::ops`.
    pub review: crate::review::ops::ReviewConfig,
    /// Stamps and tiers, see `stamp::ops`.
    pub stamp: crate::stamp::ops::StampConfig,
    /// The test network, see `report::ops`.
    pub test: crate::report::ops::TestConfig,
    /// The front door, see `door`.
    pub door: crate::door::DoorConfig,
}

impl HubConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        HubConfig {
            data_dir: data_dir.into(),
            policy_path: None,
            limits: Limits::default(),
            rate_per_sec: 2.0,
            burst: 30.0,
            claim_ttl: Duration::from_secs(60 * 60),
            idle_alert_after: Duration::from_secs(10 * 60),
            sweep_every: Duration::from_secs(10),
            token_reload_every: Duration::from_secs(2),
            ping_every: Duration::from_secs(30),
            verbose: true,
            ui_dir: None,
            join: JoinRules::default(),
            nonce_ttl: Duration::from_millis(crate::key::join::NONCE_TTL_MS),
            intake: Default::default(),
            verify: Default::default(),
            review: Default::default(),
            stamp: Default::default(),
            test: Default::default(),
            door: Default::default(),
        }
    }
}

/// A stored message with its JSON line, shared by all subscribers.
pub struct Entry {
    pub msg: Message,
    pub line: String,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    /// Takes one request, or says how many seconds until the next one fits.
    fn take(&mut self, rate: f64, burst: f64, now: Instant) -> Result<(), f64> {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * rate).min(burst);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err((1.0 - self.tokens) / rate)
        }
    }
}

struct State {
    next_id: u64,
    log: File,
    policy: PolicySnapshot,
    claims: Claims,
    roster: Roster,
    /// Latest HELLO body per member.
    hello: HashMap<String, String>,
    /// Last non-STATUS message or heartbeat timestamp per member (epoch ms).
    last_seen: HashMap<String, u64>,
    derive: crate::derive::DeriveStore,
    store: Option<crate::store::Store>,
    /// The keys of key members, folded from the key facts of the log.
    keys: KeyBook,
}

impl State {
    /// Everything derived from messages, the same for live appends and the replay at start.
    fn derive(
        claims: &mut Claims,
        roster: &mut Roster,
        hello: &mut HashMap<String, String>,
        last_seen: &mut HashMap<String, u64>,
        derive: &mut crate::derive::DeriveStore,
        m: &Message,
        now: u64,
    ) {
        claims.apply(m, now);
        roster.apply(m);
        if m.kind == Kind::Hello {
            hello.insert(m.from.clone(), m.body.clone());
        }
        if m.kind != Kind::Status && m.role != Role::Hub {
            let ts = crate::time::parse_ms(&m.ts).unwrap_or(now);
            last_seen
                .entry(m.from.clone())
                .and_modify(|prev| *prev = (*prev).max(ts))
                .or_insert(ts);
        }
        derive.apply(m, now);
    }
}

/// Lines of the log that one replay holds in memory at once, and the most `last` may ask for.
const REPLAY_PAGE: usize = 1000;

/// How long one frame may take to be taken by a client before the connection is dropped.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// A reader of the log that gives matching lines a page at a time, so that a replay holds one page and
/// reads the next one only after the last was sent.
struct LogCursor {
    lines: Option<std::io::Lines<BufReader<File>>>,
    from: u64,
    upto: u64,
    filter: Filter,
}

impl LogCursor {
    /// The next at most `max` matching entries, empty at the end.
    fn page(&mut self, max: usize) -> io::Result<Vec<Arc<Entry>>> {
        let mut out = Vec::new();
        let Some(lines) = self.lines.as_mut() else {
            return Ok(out);
        };
        for line in lines.by_ref() {
            let line = line?;
            let Ok(msg) = serde_json::from_str::<Message>(&line) else {
                continue;
            };
            if msg.id > self.upto {
                self.lines = None;
                break;
            }
            if msg.id < self.from || !self.filter.matches(&msg) {
                continue;
            }
            out.push(Arc::new(Entry { msg, line }));
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }
}

struct ConnInfo {
    name: String,
    role: Role,
    since: String,
    live: bool,
}

pub struct Hub {
    cfg: HubConfig,
    log_path: PathBuf,
    tokens_path: PathBuf,
    tokens: RwLock<TokenFile>,
    tokens_stamp: Mutex<Option<(SystemTime, u64)>>,
    state: Mutex<State>,
    buckets: Mutex<HashMap<String, Bucket>>,
    tx: broadcast::Sender<Arc<Entry>>,
    conns: Mutex<HashMap<u64, ConnInfo>>,
    next_conn: AtomicU64,
    stop: tokio::sync::watch::Sender<bool>,
    /// Random id of this hub, stored in the data folder at the first start.
    hub_id: String,
    challenges: Challenges,
    door: crate::door::Door,
}

/// How a connection proved who it is. Opaque outside the hub.
pub(crate) struct Credential(Cred);

/// What a connection used to prove who it is.
enum Cred {
    /// A bearer token, rechecked against the token file.
    Token(String),
    /// A key: the base64url public key it signed with. The member has tier 0.
    Key(String),
    /// No credentials at all on a hub with public read: an anonymous observer.
    Public,
}

/// The member an anonymous observer acts as. It is never registered as a connection.
fn public_member() -> Member {
    Member {
        name: "public".into(),
        role: Role::Observer,
        salt: String::new(),
        hash: String::new(),
        created: String::new(),
        token: None,
    }
}

/// The hub id of the data folder, made at the first start.
fn load_hub_id(dir: &std::path::Path) -> io::Result<String> {
    let path = dir.join("hub_id");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let id = text.trim().to_owned();
        if id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Ok(id);
        }
    }
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
    let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(
        &path,
        format!(
            "{id}
"
        ),
    )?;
    Ok(id)
}

/// The secret that makes the nonces of this data folder, made at the first start. It is never
/// logged and never sent. A hub that loses the file only invalidates the nonces in flight.
fn load_nonce_secret(dir: &std::path::Path) -> io::Result<[u8; 32]> {
    let path = dir.join("challenge_secret");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let text = text.trim();
        if text.len() == 64
            && let Some(raw) = (0..32)
                .map(|i| u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()
        {
            let mut out = [0u8; 32];
            out.copy_from_slice(&raw);
            return Ok(out);
        }
    }
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
    let text: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path)?;
        f.write_all(text.as_bytes())?;
        f.write_all(
            b"
",
        )?;
    }
    Ok(raw)
}

fn file_stamp(path: &std::path::Path) -> Option<(SystemTime, u64)> {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 120 {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

impl Hub {
    /// Opens the data folder, reads the tokens and replays the log to rebuild ids and claims.
    pub fn open(cfg: HubConfig) -> io::Result<Arc<Hub>> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let log_path = cfg.data_dir.join("bus.jsonl");
        let tokens_path = cfg.data_dir.join("tokens.json");
        let tokens = TokenFile::load(&tokens_path)?;
        let tokens_stamp = file_stamp(&tokens_path);
        let hub_id = load_hub_id(&cfg.data_dir)?;
        let nonce_secret = load_nonce_secret(&cfg.data_dir)?;

        let store = PolicyStore::new(&cfg.data_dir);
        store.initialize(cfg.policy_path.as_deref())?;
        let baseline = store.versions_after(None)?[0];
        let mut announced = baseline;
        let mut next_id = 1;
        let mut claims = Claims::default();
        let mut roster = Roster::default();
        let mut hello = HashMap::new();
        let mut last_seen = HashMap::new();
        let mut derive = crate::derive::DeriveStore::default();
        let mut keys = KeyBook::default();
        derive.timers.idle_alert_sec = cfg.idle_alert_after.as_secs();
        let now = now_ms();
        if let Ok(f) = File::open(&log_path) {
            for (n, line) in BufReader::new(f).lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Message>(&line) {
                    Ok(m) => {
                        next_id = next_id.max(m.id + 1);
                        if m.kind == Kind::Policy
                            && m.role == Role::Hub
                            && let Ok(body) = serde_json::from_str::<Value>(&m.body)
                            && let Some(version) = body["version"].as_u64()
                        {
                            announced = announced.max(version);
                        }
                        keys.apply(&m);
                        // Claims restored from the log get a fresh timeout from now.
                        State::derive(
                            &mut claims,
                            &mut roster,
                            &mut hello,
                            &mut last_seen,
                            &mut derive,
                            &m,
                            now,
                        );
                    }
                    Err(e) => eprintln!(
                        "sommelier: skipping line {} of {}: {e}",
                        n + 1,
                        log_path.display()
                    ),
                }
            }
        }
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&log_path)?;
        // A crash in the middle of a write leaves a line without its newline: close it.
        let len = log.metadata()?.len();
        if len > 0 {
            let mut last = [0u8; 1];
            log.seek(SeekFrom::Start(len - 1))?;
            log.read_exact(&mut last)?;
            if last[0] != b'\n' {
                log.write_all(b"\n")?;
            }
        }
        let (tx, _) = broadcast::channel(4096);
        let mut sqlite_store = crate::store::Store::open(&cfg.data_dir).ok();
        if let Some(ref mut s) = sqlite_store {
            let _ = s.catch_up_from_log(&log_path);
        }
        let door = crate::door::Door::open(
            cfg.door.clone(),
            hmac_sha256(&nonce_secret, b"door-secret-v1"),
            &cfg.data_dir,
        );
        let hub = Arc::new(Hub {
            cfg,
            log_path,
            tokens_path,
            tokens: RwLock::new(tokens),
            tokens_stamp: Mutex::new(tokens_stamp),
            state: Mutex::new(State {
                next_id,
                log,
                policy: store.read_version(announced)?,
                claims,
                roster,
                hello,
                last_seen,
                derive,
                store: sqlite_store,
                keys,
            }),
            buckets: Mutex::new(HashMap::new()),
            tx,
            conns: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
            stop: tokio::sync::watch::channel(false).0,
            hub_id,
            challenges: Challenges::with_secret(nonce_secret),
            door,
        });
        hub.reload_policy().map_err(io::Error::other)?;
        Ok(hub)
    }

    /// Closes every connection and makes [`serve`] return.
    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }

    pub fn config(&self) -> &HubConfig {
        &self.cfg
    }

    /// The front door: limits and address blocks.
    pub fn door(&self) -> &crate::door::Door {
        &self.door
    }

    /// The role of the token member `name`, `None` when no token has that name.
    pub(crate) fn token_role(&self, name: &str) -> Option<Role> {
        let tokens = self.tokens.read().unwrap_or_else(|e| e.into_inner());
        tokens.get(name).map(|m| m.role)
    }

    /// Id of the newest stored message, 0 when the log is empty.
    pub fn last_id(&self) -> u64 {
        lock(&self.state).next_id - 1
    }

    /// Stamps id and time, stores, applies and broadcasts. Call with the state lock held.
    fn append(&self, st: &mut State, mut msg: Message) -> Result<u64, String> {
        let now = now_ms();
        msg.id = st.next_id;
        msg.ts = rfc3339(now);
        let line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        st.log
            .write_all(&buf)
            .map_err(|e| format!("the hub could not store the message: {e}"))?;
        st.next_id += 1;
        st.keys.apply(&msg);
        let State {
            claims,
            roster,
            hello,
            last_seen,
            derive,
            store,
            ..
        } = st;
        State::derive(claims, roster, hello, last_seen, derive, &msg, now);
        if let Some(s) = store {
            let _ = s.apply(&msg);
        }
        let id = msg.id;
        // No receivers is fine: nobody is listening right now.
        let _ = self.tx.send(Arc::new(Entry { msg, line }));
        Ok(id)
    }

    /// Validates, stamps, appends to `bus.jsonl`, projects into `store`, and broadcasts
    /// an `OFFER`, `TASK_UPDATE`, `ALERT`, or `FACT` message emitted by a module hook or tick.
    pub(crate) fn append_hook_message(&self, mut msg: Message) -> Result<u64, String> {
        if !crate::hooks::is_hook_emittable_kind(msg.kind) {
            return Err(format!(
                "hook may only emit OFFER, TASK_UPDATE, ALERT, or FACT (got {})",
                msg.kind.as_str()
            ));
        }
        if msg.kind == Kind::Fact
            && msg.role != Role::Hub
            && let Some(prefix) = hub_only_prefix(&msg.topic)
        {
            return Err(format!(
                "FACT topics under {prefix} are written by the hub only"
            ));
        }
        check_kind_limit(&self.cfg.limits, msg.kind, &msg.body)?;
        validate(&self.cfg.limits, &msg.topic, &msg.body, &msg.refs)?;
        if !msg.to.is_empty() {
            msg.to = self.check_addressees(msg.to)?;
        }
        let mut st = lock(&self.state);
        self.append(&mut st, msg)
    }

    pub(crate) fn with_store<R>(&self, f: impl FnOnce(&mut crate::store::Store) -> R) -> Option<R> {
        let mut st = lock(&self.state);
        if st.store.is_none() {
            st.store = crate::store::Store::open(&self.cfg.data_dir).ok();
        }
        let log_path = self.log_path.clone();
        if let Some(ref mut s) = st.store {
            let _ = s.catch_up_from_log(&log_path);
            Some(f(s))
        } else {
            None
        }
    }

    pub(crate) fn derive_on_message(&self, msg: &Message, now_ms: u64) {
        lock(&self.state).derive.apply(msg, now_ms);
    }

    pub(crate) fn derived_snapshot(&self, now_ms: u64) -> crate::derive::DerivedSnapshot {
        let mut online = HashMap::new();
        for c in lock(&self.conns).values() {
            online.insert(c.name.clone(), true);
        }
        let st = lock(&self.state);
        st.derive
            .compute(&st.roster, &st.claims, &online, &st.last_seen, now_ms)
    }

    pub(crate) fn derive_on_tick(&self, now_ms: u64) -> Vec<Message> {
        let mut online = HashMap::new();
        for c in lock(&self.conns).values() {
            online.insert(c.name.clone(), true);
        }
        let coordinator = {
            let t = self.tokens.read().unwrap_or_else(|e| e.into_inner());
            t.members
                .iter()
                .find(|m| m.role == Role::Orchestrator)
                .map(|m| m.name.clone())
        };
        let mut st = lock(&self.state);
        let snap = st
            .derive
            .compute(&st.roster, &st.claims, &online, &st.last_seen, now_ms);
        st.derive.tick_alerts(&snap, coordinator.as_deref(), now_ms)
    }

    pub(crate) fn policy_response(&self) -> Value {
        lock(&self.state).policy.response()
    }

    fn reload_policy(&self) -> Result<(), String> {
        let store = PolicyStore::new(&self.cfg.data_dir);
        let mut st = lock(&self.state);
        let versions = store
            .versions_after(Some(st.policy.policy.version))
            .map_err(|e| e.to_string())?;
        for version in versions {
            let next = store.read_version(version).map_err(|e| e.to_string())?;
            self.append(
                &mut st,
                Message {
                    id: 0,
                    ts: String::new(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::Policy,
                    topic: String::new(),
                    body: json!({"version": version, "hash": next.hash}).to_string(),
                    refs: vec![],
                    to: vec![],
                },
            )?;
            st.policy = next;
        }
        Ok(())
    }

    /// Checks role, size and claims, then stores and broadcasts a message from `member`.
    pub fn publish(
        &self,
        member: &Member,
        kind: Kind,
        topic: String,
        body: String,
        refs: Vec<String>,
        to: Vec<String>,
    ) -> Result<u64, String> {
        let msg = Message {
            id: 0,
            ts: String::new(),
            from: member.name.clone(),
            role: member.role,
            kind,
            topic,
            body,
            refs,
            to,
        };
        self.publish_message(msg, None).map(|(id, _)| id)
    }

    fn publish_message(
        &self,
        mut msg: Message,
        policy_hash: Option<&str>,
    ) -> Result<(u64, Option<&'static str>), String> {
        let kind = msg.kind;
        if kind == Kind::Policy {
            return Err("only the hub may send POLICY".into());
        }
        match msg.role {
            Role::Observer => return Err("observers are read only, this token cannot send".into()),
            Role::Hub => return Err("the hub role cannot send".into()),
            Role::Member if kind == Kind::Task => {
                return Err("only an orchestrator may send TASK".into());
            }
            _ => {}
        }
        if kind == Kind::Fact
            && let Some(prefix) = hub_only_prefix(&msg.topic)
        {
            return Err(format!(
                "FACT topics under {prefix} are written by the hub only"
            ));
        }
        check_kind_limit(&self.cfg.limits, kind, &msg.body)?;
        validate(&self.cfg.limits, &msg.topic, &msg.body, &msg.refs)?;
        if kind == Kind::Status {
            roster::check(&msg.topic, &msg.refs)?;
        }
        msg.to = self.check_addressees(msg.to)?;
        let mut st = lock(&self.state);
        let warning = match kind {
            Kind::Claim => {
                let warning = st.policy.check_hash(policy_hash, SystemTime::now())?;
                st.claims.check_claim(&msg.topic, &msg.from)?;
                warning
            }
            Kind::Release => {
                st.claims.check_release(&msg.topic, &msg.from, msg.role)?;
                None
            }
            _ => None,
        };
        Ok((self.append(&mut st, msg)?, warning))
    }

    /// Lower cases the names, drops repeats, refuses names without a token so a typo
    /// does not send a message into the void. An addressee may be `name/worker`: the name
    /// needs a token, the worker is free text kept as given.
    fn check_addressees(&self, to: Vec<String>) -> Result<Vec<String>, String> {
        if to.len() > self.cfg.limits.max_refs {
            return Err(format!(
                "{} addressees, the limit is {}",
                to.len(),
                self.cfg.limits.max_refs
            ));
        }
        let tokens = self.tokens.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<String> = Vec::new();
        for addr in to {
            let (name, worker) = split_addressee(addr.trim());
            let name = name.to_ascii_lowercase();
            if tokens.get(&name).is_none() {
                return Err(format!(
                    "unknown addressee `{name}`, no token has that name"
                ));
            }
            let full = match worker {
                Some(w) => {
                    check_worker_name(w)?;
                    format!("{name}/{w}")
                }
                None => name,
            };
            if !out.contains(&full) {
                out.push(full);
            }
        }
        Ok(out)
    }

    /// Releases claims whose holder was silent too long, as RELEASE messages from `hub`,
    /// and runs module ticks (such as IDLE alerts).
    pub fn sweep(&self) {
        let now = now_ms();
        let ttl = self.cfg.claim_ttl;
        {
            let mut st = lock(&self.state);
            for (topic, c) in st.claims.expired(now, ttl.as_millis() as u64) {
                let body = format!(
                    "claim of {} expired, no message or heartbeat for {}",
                    c.holder,
                    human(ttl)
                );
                let msg = Message {
                    id: 0,
                    ts: String::new(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::Release,
                    topic,
                    body,
                    refs: vec![c.holder.clone()],
                    to: vec![c.holder],
                };
                if let Err(e) = self.append(&mut st, msg) {
                    eprintln!("sommelier: {e}");
                }
            }
        }
        let mut tick_msgs = crate::hooks::on_tick(self, now);
        tick_msgs.extend(crate::door::on_tick(self, now));
        for msg in tick_msgs {
            if let Err(e) = self.append_hook_message(msg) {
                eprintln!("sommelier: {e}");
            }
        }
    }

    fn charge(&self, name: &str) -> Result<(), String> {
        let (rate, burst) = (self.cfg.rate_per_sec, self.cfg.burst);
        let mut b = lock(&self.buckets);
        let bucket = b.entry(name.into()).or_insert(Bucket {
            tokens: burst,
            last: Instant::now(),
        });
        bucket.take(rate, burst, Instant::now()).map_err(|wait| {
            format!("rate limit: {rate} requests per second with a burst of {burst}, try again in {wait:.1} s")
        })
    }

    /// Rereads `tokens.json` when its time or size changed.
    pub fn reload_tokens(&self) {
        let stamp = file_stamp(&self.tokens_path);
        let mut old = lock(&self.tokens_stamp);
        if *old == stamp {
            return;
        }
        match TokenFile::load(&self.tokens_path) {
            Ok(t) => {
                *self.tokens.write().unwrap_or_else(|e| e.into_inner()) = t;
                *old = stamp;
            }
            Err(e) => eprintln!("sommelier: keeping the old tokens, {e}"),
        }
    }

    /// The token this connection presented still belongs to the same name and role.
    fn token_valid(&self, m: &Member, token: &str) -> bool {
        let t = self.tokens.read().unwrap_or_else(|e| e.into_inner());
        t.lookup(token)
            .is_some_and(|x| x.name == m.name && x.role == m.role)
    }

    /// The member for a token from the `Authorization: Bearer` header, or from the URL
    /// query for observer tokens only (URLs end up in proxy and tunnel logs).
    pub(crate) fn check_token(
        &self,
        header: Option<String>,
        query: Option<String>,
    ) -> Result<(Member, String), (StatusCode, String)> {
        let in_url = header.is_none();
        let token = header.or(query).ok_or((
            StatusCode::UNAUTHORIZED,
            "missing token: send `Authorization: Bearer <token>` (or `?token=` with an observer token)".to_string(),
        ))?;
        let t = self.tokens.read().unwrap_or_else(|e| e.into_inner());
        let m = t.lookup(&token).cloned().ok_or((
            StatusCode::UNAUTHORIZED,
            "unknown or revoked token".to_string(),
        ))?;
        // URLs end up in proxy and tunnel logs: only read only tokens may travel there.
        if in_url && m.role != Role::Observer {
            return Err((
                StatusCode::UNAUTHORIZED,
                "a token in the URL is accepted for observer tokens only, send this one in the Authorization header"
                    .to_string(),
            ));
        }
        Ok((m, token))
    }

    /// Authenticates with the `Authorization` header: `SommelierKey <member>.<nonce>.<sig>` for
    /// a key member, else a bearer token (or an observer token in the URL query).
    pub(crate) fn check_auth(
        &self,
        authorization: Option<&str>,
        query: Option<String>,
    ) -> Result<(Member, Credential), (StatusCode, String)> {
        if let Some(rest) = authorization.and_then(|v| v.strip_prefix("SommelierKey ")) {
            return self.check_key(rest.trim());
        }
        let bearer = authorization
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|v| v.trim().to_owned());
        let (m, token) = self.check_token(bearer, query)?;
        Ok((m, Credential(Cred::Token(token))))
    }

    /// Checks `<member>.<nonce>.<signature>`: the key is active, the signature covers the auth
    /// text and the nonce was handed out by this hub and is still unused.
    fn check_key(&self, header: &str) -> Result<(Member, Credential), (StatusCode, String)> {
        let unauthorized = |m: &str| (StatusCode::UNAUTHORIZED, m.to_string());
        if !self.cfg.join.enabled {
            return Err(unauthorized(
                "key sign in is switched off on this hub, the owner turns it on with `sommelier serve --join on`",
            ));
        }
        let mut parts = header.split('.');
        let (Some(member), Some(nonce), Some(sig), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(unauthorized(
                "malformed key credentials: send `Authorization: SommelierKey <member>.<nonce>.<signature>`",
            ));
        };
        let (public, joined_at) = {
            let st = lock(&self.state);
            match st.keys.lookup(member) {
                Lookup::Active(p) => {
                    let at = st.keys.get(member).map(|r| r.joined_at.clone());
                    (p, at.unwrap_or_default())
                }
                Lookup::Revoked => return Err(unauthorized("this key was revoked")),
                Lookup::Unknown => {
                    return Err(unauthorized(
                        "unknown key member, run `sommelier join` first",
                    ));
                }
            }
        };
        // A member with no tier at all is refused like a revoked key (docs/factory.md section 5).
        if crate::stamp::ops::request_tier(self, member) < 0 {
            return Err(unauthorized(
                "this key has no standing: it was revoked or a finding of bad intent took its tier",
            ));
        }
        self.challenges
            .check(nonce, now_ms())
            .map_err(|e| unauthorized(&e))?;
        let sig = crate::key::crypto::unb64(sig)
            .map_err(|_| unauthorized("the signature is not valid base64url"))?;
        let public_bytes = crate::key::crypto::unb64(&public)
            .map_err(|_| unauthorized("the stored key is damaged"))?;
        crate::key::crypto::verify_bytes(
            &public_bytes,
            crate::key::crypto::auth_text(&self.hub_id, nonce).as_bytes(),
            &sig,
        )
        .map_err(|_| unauthorized("the key signature is wrong"))?;
        self.challenges
            .mark_used(nonce, now_ms())
            .map_err(|e| unauthorized(&e))?;
        let member = Member {
            name: member.to_owned(),
            role: Role::Member,
            salt: String::new(),
            hash: String::new(),
            created: joined_at,
            token: None,
        };
        Ok((member, Credential(Cred::Key(public))))
    }

    /// Whether this credential gets the public view: no token at all, or a key below tier 2. Tokens
    /// of every role see everything, observer tokens included (the dashboard and the witness).
    fn public_view(&self, member: &str, cred: &Cred) -> bool {
        match cred {
            Cred::Public => true,
            Cred::Key(_) => crate::stamp::ops::request_tier(self, member) < 2,
            Cred::Token(_) => false,
        }
    }

    /// [`Hub::public_view`] for the credential that `check_auth` returned.
    pub(crate) fn credential_is_public(&self, member: &str, cred: &Credential) -> bool {
        self.public_view(member, &cred.0)
    }

    /// The key this connection signed in with is still the current key of an active member.
    fn key_valid(&self, member: &str, public: &str) -> bool {
        lock(&self.state).keys.is_current(member, public)
    }

    fn authorize(
        &self,
        req: &HsRequest,
        ip: std::net::IpAddr,
    ) -> Result<(Member, ConnectParams, Credential, crate::door::Slot), (StatusCode, String)> {
        let mut params = ConnectParams::from_query(req.uri().query())
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        let header = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        let too_many = |e: String| (StatusCode::TOO_MANY_REQUESTS, e);
        if header.is_none() && params.token.is_none() && self.door.public_read() {
            // An anonymous observer: it sends nothing and fills the hub as tier 0 does.
            self.door
                .charge_read(&self.door.block(ip), now_ms())
                .map_err(too_many)?;
            let slot = self.door.admit("", true).map_err(too_many)?;
            return Ok((public_member(), params, Credential(Cred::Public), slot));
        }
        let (m, cred) = self.check_auth(header, params.token.clone())?;
        let role = matches!(cred.0, Cred::Token(_)).then_some(m.role);
        let low = self.cfg.door.max_conns > 0 && crate::door::is_low(self, &m.name, role);
        let slot = self.door.admit(&m.name, low).map_err(too_many)?;
        // Kept for the periodic revocation check, the file only holds hashes.
        if let Cred::Token(t) = &cred.0 {
            params.token = Some(t.clone());
        }
        Ok((m, params, cred, slot))
    }

    /// `GET /api/challenge`: a nonce that lives a short time and is used once.
    pub fn challenge(&self) -> Result<Value, (u16, String)> {
        if !self.cfg.join.enabled {
            return Err((
                403,
                "this hub serves no challenge while joining is switched off, the owner turns it on with `sommelier serve --join on`".into(),
            ));
        }
        let now = now_ms();
        let (nonce, expires) = self
            .challenges
            .issue(now, self.cfg.nonce_ttl.as_millis() as u64)
            .map_err(|e| (429, e))?;
        Ok(
            json!({"hub_id": self.hub_id, "nonce": nonce, "expires": rfc3339(expires),
                  "pow_bits": self.cfg.join.proof_of_work_bits}),
        )
    }

    /// `POST /api/join`: registers a key. A known key gets its member id and nothing is written.
    pub fn join(&self, raw: &[u8], ip: Option<std::net::IpAddr>) -> Result<Value, (u16, String)> {
        let rules = self.cfg.join;
        if !rules.enabled {
            return Err((
                403,
                "joining is switched off on this hub, the owner turns it on with `sommelier serve --join on`".into(),
            ));
        }
        let bad = |e: String| (400u16, e);
        let block = ip.map(|ip| self.door.block(ip));
        // Garbage costs the sender a cheap, separate allowance. The join limit is charged only for a
        // request that parsed and carried a valid proof of work and signature.
        if let Some(block) = &block {
            self.door
                .check_bad_joins(block, now_ms())
                .map_err(|e| (429u16, e))?;
        }
        let fail = |e: String| {
            if let Some(block) = &block {
                self.door.note_bad_join(block, now_ms());
            }
            bad(e)
        };
        let body: Value = serde_json::from_slice(raw)
            .map_err(|e| fail(format!("the join body is not valid JSON: {e}")))?;
        let req = crate::key::join::parse_join(&body).map_err(fail)?;
        self.challenges.check(&req.nonce, now_ms()).map_err(fail)?;
        crate::key::join::verify_join(&rules, &req, &body).map_err(fail)?;
        if let Some(block) = &block {
            crate::key::join::limit_join(&self.door, block, now_ms()).map_err(|e| (429u16, e))?;
        }
        self.challenges
            .mark_used(&req.nonce, now_ms())
            .map_err(bad)?;
        let member = crate::key::crypto::member_id(&req.public);
        if self
            .tokens
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&member)
            .is_some()
        {
            return Err(bad(format!("the member id `{member}` is taken by a token")));
        }
        let mut st = lock(&self.state);
        match st.keys.standing(&member, &req.public_b64) {
            Standing::Known(existing) => {
                drop(st);
                self.note_door(&member, block.as_deref());
                return Ok(json!({"member": existing, "joined": false}));
            }
            Standing::Taken => {
                return Err(bad(format!(
                    "the member id `{member}` already belongs to another key"
                )));
            }
            Standing::New => {}
        }
        let joined_at = rfc3339(now_ms());
        let msg = Message {
            id: 0,
            ts: String::new(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Fact,
            topic: format!("key/{member}"),
            body: crate::key::book::join_body(&member, &req.public_b64, &req.handle, &joined_at),
            refs: vec![],
            to: vec![],
        };
        validate(&self.cfg.limits, &msg.topic, &msg.body, &msg.refs).map_err(bad)?;
        let id = self.append(&mut st, msg).map_err(|e| (500, e))?;
        drop(st);
        self.note_door(&member, block.as_deref());
        Ok(json!({"member": member, "joined": true, "id": id}))
    }

    /// Notes the address block of a member in the side store, without the state lock.
    fn note_door(&self, member: &str, block: Option<&str>) {
        if let Some(block) = block {
            self.door.note_seen(member, block, now_ms());
        }
    }

    /// Reads stored lines with `from <= id <= upto` that match, keeping the last `last`
    /// of them, or the first `limit`.
    pub fn read_log(
        &self,
        from: u64,
        upto: u64,
        filter: &Filter,
        last: Option<usize>,
        limit: Option<usize>,
    ) -> io::Result<Vec<Arc<Entry>>> {
        let mut out = VecDeque::new();
        let f = match File::open(&self.log_path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        for line in BufReader::new(f).lines() {
            let line = line?;
            let Ok(msg) = serde_json::from_str::<Message>(&line) else {
                continue;
            };
            if msg.id > upto {
                break;
            }
            if msg.id < from || !filter.matches(&msg) {
                continue;
            }
            out.push_back(Arc::new(Entry { msg, line }));
            if let Some(n) = last
                && out.len() > n
            {
                out.pop_front();
            }
            if last.is_none() && limit.is_some_and(|l| out.len() >= l) {
                break;
            }
        }
        Ok(out.into())
    }

    async fn read_log_async(
        self: &Arc<Self>,
        from: u64,
        upto: u64,
        filter: Filter,
        last: Option<usize>,
        limit: Option<usize>,
    ) -> Result<Vec<Arc<Entry>>, String> {
        let h = self.clone();
        tokio::task::spawn_blocking(move || h.read_log(from, upto, &filter, last, limit))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("reading the log: {e}"))
    }

    /// A cursor over the log for a replay that sends a page at a time.
    fn open_cursor(&self, from: u64, upto: u64, filter: Filter) -> io::Result<LogCursor> {
        let lines = match File::open(&self.log_path) {
            Ok(f) => Some(BufReader::new(f).lines()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        Ok(LogCursor {
            lines,
            from,
            upto,
            filter,
        })
    }

    /// The dashboard snapshot served at `/api/state`: members with presence and their
    /// latest HELLO, agent rows, claims, and the last 200 messages except STATUS.
    pub async fn snapshot(self: &Arc<Self>) -> Value {
        let now = now_ms();
        let (last_id, agents, claims, hello) = {
            let st = lock(&self.state);
            let claims: Vec<Value> = st
                .claims
                .iter()
                .map(|(t, c)| json!({"topic": t, "holder": c.holder, "since": c.since_ts, "since_id": c.since_id}))
                .collect();
            (
                st.next_id - 1,
                st.roster.snapshot(now, None),
                claims,
                st.hello.clone(),
            )
        };
        // Earliest open connection per name.
        let mut online: HashMap<String, String> = HashMap::new();
        for c in lock(&self.conns).values() {
            let e = online
                .entry(c.name.clone())
                .or_insert_with(|| c.since.clone());
            if c.since < *e {
                *e = c.since.clone();
            }
        }
        let members: Vec<Value> = {
            let t = self.tokens.read().unwrap_or_else(|e| e.into_inner());
            t.members
                .iter()
                .map(|m| {
                    json!({"name": m.name, "role": m.role.as_str(), "online": online.contains_key(&m.name),
                           "since": online.get(&m.name), "hello": hello.get(&m.name)})
                })
                .collect()
        };
        let not_status = Filter {
            kinds: Kind::ALL
                .into_iter()
                .filter(|k| *k != Kind::Status)
                .collect(),
            ..Default::default()
        };
        let recent: Vec<Value> = self
            .read_log_async(1, last_id, not_status, Some(200), None)
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|e| serde_json::to_value(&e.msg).ok())
            .collect();
        let mut root = Map::new();
        root.insert(
            "hub".into(),
            json!({"id": last_id, "now": rfc3339(now), "as_of": now / 1000}),
        );
        root.insert("as_of".into(), json!(now / 1000));
        root.insert("members".into(), json!(members));
        root.insert(
            "agents".into(),
            serde_json::to_value(agents).unwrap_or_default(),
        );
        root.insert("claims".into(), json!(claims));
        root.insert("recent".into(), json!(recent));
        crate::hooks::api(self, now, &mut root);
        Value::Object(root)
    }

    /// Answers one text frame from a client.
    async fn handle_text(self: &Arc<Self>, member: &Member, tier0: bool, text: &str) -> String {
        let max = self.cfg.limits.max_frame;
        if text.len() > max {
            return reply_err(
                "?",
                &None,
                &format!("frame is {} bytes, the limit is {max}", text.len()),
            );
        }
        let env = parse_request(text);
        let (op, rid) = (env.op.as_str(), &env.rid);
        if let Err(e) = self.charge(&member.name) {
            return reply_err(op, rid, &e);
        }
        let req = match env.req {
            Ok(r) => r,
            Err(e) => return reply_err(op, rid, &e),
        };
        // A key member is held to its tier, read now: a demotion takes effect at once.
        let tier = tier0.then(|| crate::stamp::ops::request_tier(self, &member.name));
        let coding = self.cfg.stamp.enabled && self.cfg.stamp.tier0_coding;
        if let Some(tier) = tier
            && let Some(what) = tier_denied(&req, tier, coding)
        {
            return reply_err(op, rid, &crate::key::tier_refusal(tier, &what, coding));
        }
        let mut out = Map::new();
        match req {
            Request::Send {
                policy_hash,
                kind,
                topic,
                body,
                refs,
                to,
            } => match self.publish_message(
                Message {
                    id: 0,
                    ts: String::new(),
                    from: member.name.clone(),
                    role: member.role,
                    kind,
                    topic,
                    body,
                    refs,
                    to,
                },
                policy_hash.as_deref(),
            ) {
                Ok((id, warning)) => {
                    out.insert("id".into(), id.into());
                    if let Some(warning) = warning {
                        out.insert("warning".into(), warning.into());
                    }
                }
                Err(e) => return reply_err(op, rid, &e),
            },
            Request::Heartbeat => {
                let now = now_ms();
                let mut st = lock(&self.state);
                st.claims.touch(&member.name, now);
                st.last_seen.insert(member.name.clone(), now);
                let held = st
                    .claims
                    .iter()
                    .filter(|(_, c)| c.holder == member.name)
                    .count();
                out.insert("claims".into(), held.into());
            }
            Request::Whoami => {
                out.insert("name".into(), member.name.clone().into());
                out.insert("role".into(), member.role.as_str().into());
                if let Some(tier) = tier {
                    out.insert("tier".into(), tier.into());
                }
                out.insert("last_id".into(), self.last_id().into());
            }
            Request::Claims => {
                let now = now_ms();
                let st = lock(&self.state);
                let list: Vec<Value> = st
                    .claims
                    .iter()
                    .map(|(t, c)| {
                        json!({"topic": t, "holder": c.holder, "since_id": c.since_id, "since_ts": c.since_ts,
                               "idle_secs": now.saturating_sub(c.last_seen_ms) / 1000})
                    })
                    .collect();
                out.insert("claims".into(), list.into());
                out.insert("ttl_secs".into(), self.cfg.claim_ttl.as_secs().into());
            }
            Request::Roster { owner } => {
                let owner = owner.map(|o| {
                    if o.eq_ignore_ascii_case("me") {
                        member.name.clone()
                    } else {
                        o.to_ascii_lowercase()
                    }
                });
                let agents = lock(&self.state)
                    .roster
                    .snapshot(now_ms(), owner.as_deref());
                out.insert(
                    "agents".into(),
                    serde_json::to_value(agents).unwrap_or_default(),
                );
            }
            Request::Idle => {
                let snap = self.derived_snapshot(now_ms());
                out.insert("as_of".into(), json!(snap.as_of));
                out.insert(
                    "idle".into(),
                    serde_json::to_value(&snap.idle).unwrap_or_default(),
                );
                out.insert(
                    "slots".into(),
                    serde_json::to_value(&snap.slots).unwrap_or_default(),
                );
                out.insert(
                    "workers".into(),
                    serde_json::to_value(&snap.workers).unwrap_or_default(),
                );
                out.insert(
                    "counts".into(),
                    serde_json::to_value(&snap.counts).unwrap_or_default(),
                );
            }
            Request::Who => {
                let conns = lock(&self.conns);
                let mut list: Vec<Value> = conns
                    .values()
                    .map(|c| json!({"name": c.name, "role": c.role.as_str(), "live": c.live, "since": c.since}))
                    .collect();
                list.sort_by_key(|v| v["name"].as_str().unwrap_or_default().to_owned());
                out.insert("connections".into(), list.into());
            }
            Request::Log {
                since,
                last,
                limit,
                filter,
            } => {
                let limit = limit.unwrap_or(100).clamp(1, 1000) as usize;
                let upto = self.last_id();
                let r = match since {
                    Some(s) => {
                        self.read_log_async(
                            s,
                            upto,
                            filter.to_filter(&member.name),
                            None,
                            Some(limit),
                        )
                        .await
                    }
                    None => {
                        let n = last.map_or(limit, |l| (l as usize).min(limit));
                        self.read_log_async(1, upto, filter.to_filter(&member.name), Some(n), None)
                            .await
                    }
                };
                match r {
                    Ok(entries) => {
                        // A key below tier 2 reads the public view here too, not only on its stream.
                        let public = tier.is_some_and(|t| t < 2);
                        let msgs: Vec<Value> = entries
                            .iter()
                            .filter_map(|e| {
                                if public {
                                    serde_json::to_value(crate::door::public_message(&e.msg)?).ok()
                                } else {
                                    serde_json::to_value(&e.msg).ok()
                                }
                            })
                            .collect();
                        out.insert("messages".into(), msgs.into());
                    }
                    Err(e) => return reply_err(op, rid, &e),
                }
            }
            Request::ModuleOp { op: mod_op, body } => {
                let now = now_ms();
                let mut caller = crate::hooks::OpCaller::from_member(member);
                if tier0 {
                    caller.trust = crate::key::ops::TRUST;
                }
                if mod_op.starts_with("stamp.") && !self.cfg.stamp.enabled {
                    return reply_err(op, rid, "standing is switched off on this hub");
                }
                let mut store = match crate::store::Store::open(&self.cfg.data_dir) {
                    Ok(mut s) => {
                        let _ = s.catch_up_from_log(&self.log_path);
                        s
                    }
                    Err(e) => return reply_err(op, rid, &e),
                };
                let mut emit =
                    |s: &mut crate::store::Store, mut msg: Message| -> Result<u64, String> {
                        if msg.from.is_empty() {
                            msg.from = member.name.clone();
                            msg.role = member.role;
                        }
                        let id = self.append_hook_message(msg)?;
                        let _ = s.catch_up_from_log(&self.log_path);
                        Ok(id)
                    };
                let mut ctx = crate::hooks::OpContext::new(caller, &mut store, now, &mut emit)
                    .with_config(&self.cfg);
                match crate::hooks::on_op(&mut ctx, &mod_op, &body) {
                    Ok(extra) => {
                        if let Some(ref mut cached) = lock(&self.state).store {
                            let _ = cached.catch_up_from_log(&self.log_path);
                        }
                        out.extend(extra);
                    }
                    Err(e) => return reply_err(op, rid, &e),
                }
            }
        }
        reply_ok(op, rid, out)
    }
}

/// The topic prefixes of the factory's FACTs. Only the hub itself writes them: a client FACT on one is
/// refused in `publish_message`, and a hook may emit one only as the hub in `append_hook_message`. Each
/// projection also checks the sender role, as the second guard. `land/bundle:` is the landing row of a
/// submission, which reaches the queue through `land.enqueue` and its lock only.
pub const HUB_ONLY_TOPICS: [&str; 10] = [
    "key/",
    "submission/",
    "gate/",
    "verify/",
    "review/",
    "report/",
    "runorder/",
    "stamp/",
    "tier/",
    "land/bundle:",
];

/// The prefix of [`HUB_ONLY_TOPICS`] that `topic` starts with.
fn hub_only_prefix(topic: &str) -> Option<&'static str> {
    HUB_ONLY_TOPICS
        .iter()
        .copied()
        .find(|p| topic.starts_with(p))
}

/// What a member of `tier` asked for that it may not use, as the text for the refusal.
fn tier_denied(req: &Request, tier: i64, tier0_coding: bool) -> Option<String> {
    let (name, shown) = match req {
        Request::Send { kind, .. } => (
            kind.as_str().to_owned(),
            format!("the message kind {}", kind.as_str()),
        ),
        Request::Heartbeat => ("heartbeat".into(), "heartbeat".into()),
        Request::Whoami => ("whoami".into(), "whoami".into()),
        Request::Claims => ("claims".into(), "claims".into()),
        Request::Who => ("who".into(), "who".into()),
        Request::Idle => ("idle".into(), "idle".into()),
        Request::Roster { .. } => ("roster".into(), "roster".into()),
        Request::Log { .. } => ("log".into(), "log".into()),
        Request::ModuleOp { op, .. } => (op.clone(), format!("the op `{op}`")),
    };
    (!crate::key::tier_allows(tier, &name, tier0_coding)).then_some(shown)
}

/// Opens the hub in `cfg.data_dir` and serves on `listener` until the process ends.
pub async fn run(cfg: HubConfig, listener: TcpListener) -> io::Result<()> {
    serve(Hub::open(cfg)?, listener).await
}

pub async fn serve(hub: Arc<Hub>, listener: TcpListener) -> io::Result<()> {
    let h = hub.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(h.cfg.sweep_every);
        loop {
            t.tick().await;
            h.sweep();
        }
    });
    let h = hub.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(h.cfg.token_reload_every);
        loop {
            t.tick().await;
            h.reload_tokens();
            if let Err(e) = h.reload_policy() {
                eprintln!("sommelier: could not apply policy publication: {e}");
            }
        }
    });
    let mut stop = hub.stop.subscribe();
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            () = stopped(&mut stop) => return Ok(()),
        };
        match accepted {
            Ok((stream, _)) => {
                let h = hub.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(h.clone(), stream).await
                        && h.cfg.verbose
                    {
                        eprintln!("sommelier: connection: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("sommelier: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

fn error_response(code: StatusCode, msg: String) -> ErrorResponse {
    let mut r = ErrorResponse::new(Some(msg));
    *r.status_mut() = code;
    r
}

// The callback type is fixed by tungstenite, its error is an HTTP response.
#[allow(clippy::result_large_err)]
async fn handle(hub: Arc<Hub>, stream: TcpStream) -> Result<(), String> {
    let _ = stream.set_nodelay(true);
    let peer = stream.peer_addr().map_err(|e| format!("peer: {e}"))?.ip();
    // Every connection is counted from the first byte until its answer or its upgrade. A peer that
    // is not on this machine is counted by its own block at once, one behind a tunnel after the head.
    let slot = hub
        .door
        .admit_http(
            (!peer.to_canonical().is_loopback())
                .then(|| hub.door.block(peer))
                .as_deref(),
        )
        .map_err(|e| format!("door: {e}"))?;
    let (stream, head) = crate::web::read_head(
        stream,
        Duration::from_secs(hub.door.config().head_timeout_secs.max(1)),
    )
    .await
    .map_err(|e| format!("request head: {e}"))?;
    if !head.is_websocket() {
        return crate::web::serve(&hub, stream, head, slot)
            .await
            .map_err(|e| format!("http: {e}"));
    }
    let ip = hub.door.client_ip(
        peer,
        hub.door
            .trust_header()
            .and_then(|h| head.headers.get(&h))
            .map(String::as_str),
    );
    let mut auth = None;
    let cb = |req: &HsRequest, resp: HsResponse| -> Result<HsResponse, ErrorResponse> {
        match hub.authorize(req, ip) {
            Ok(a) => {
                auth = Some(a);
                Ok(resp)
            }
            Err((code, msg)) => Err(error_response(code, msg)),
        }
    };
    let wscfg = WebSocketConfig::default()
        .max_message_size(Some(64 * 1024))
        .max_frame_size(Some(64 * 1024));
    let ws = tokio_tungstenite::accept_hdr_async_with_config(stream, cb, Some(wscfg))
        .await
        .map_err(|e| format!("handshake refused: {e}"))?;
    // An upgraded connection is counted by `authorize`, not as plain HTTP any more.
    drop(slot);
    let Some((member, params, cred, _slot)) = auth else {
        return Err("no credentials".into());
    };
    let public = matches!(cred.0, Cred::Public);
    let id = hub.next_conn.fetch_add(1, Ordering::Relaxed);
    if !public {
        let since = rfc3339(now_ms());
        lock(&hub.conns).insert(
            id,
            ConnInfo {
                name: member.name.clone(),
                role: member.role,
                since,
                live: params.live,
            },
        );
        hub.note_door(&member.name, Some(&hub.door.block(ip)));
    }
    // Only subscribers are announced: one shot requests (`sommelier send`) would flood the console.
    let announce = hub.cfg.verbose && params.live && !public;
    if announce {
        eprintln!("sommelier: + {} ({})", member.name, member.role.as_str());
    }
    let r = connection(&hub, ws, &member, params, &cred.0).await;
    if !public {
        lock(&hub.conns).remove(&id);
    }
    if announce {
        eprintln!("sommelier: - {}", member.name);
    }
    r
}

/// Resolves once the hub shuts down (the borrow guard is dropped before returning).
async fn stopped(rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await.map(|_| ());
}

type Sink = SplitSink<WebSocketStream<crate::web::Sock>, WsMsg>;

async fn send(sink: &mut Sink, text: String) -> Result<(), String> {
    tokio::time::timeout(SEND_TIMEOUT, sink.send(WsMsg::text(text)))
        .await
        .map_err(|_| "send: the client takes no data".to_string())?
        .map_err(|e| format!("send: {e}"))
}

async fn close(sink: &mut Sink, code: CloseCode, reason: String) {
    let _ = sink
        .send(WsMsg::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// The line of `e` for this reader: as stored, or its public view, `None` when it is hidden.
fn view(e: &Entry, redact: bool) -> Option<String> {
    if redact {
        crate::door::public_line(&e.msg, &e.line)
    } else {
        Some(e.line.clone())
    }
}

async fn recv_live(
    rx: &mut Option<broadcast::Receiver<Arc<Entry>>>,
) -> Result<Arc<Entry>, broadcast::error::RecvError> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn connection(
    hub: &Arc<Hub>,
    ws: WebSocketStream<crate::web::Sock>,
    member: &Member,
    params: ConnectParams,
    cred: &Cred,
) -> Result<(), String> {
    let (mut sink, mut stream) = ws.split();
    let tier0 = matches!(cred, Cred::Key(_));
    let filter = params.filter.to_filter(&member.name);
    // The public view for a reader without a token and for a key below tier 2, read again every 2 s.
    let mut redact = hub.public_view(&member.name, cred);
    // Subscribe and note the newest id under the state lock: older ids come from the
    // log, newer ones from the channel.
    let (rx, upto) = {
        let st = lock(&hub.state);
        (hub.tx.subscribe(), st.next_id - 1)
    };
    let mut live = params.live.then_some(rx);
    let mut last_sent = 0;
    if params.since.is_some() || params.last.is_some() {
        let from = params.since.unwrap_or(1);
        if let Some(n) = params.last {
            let n = usize::try_from(n).unwrap_or(usize::MAX).min(REPLAY_PAGE);
            let entries = hub
                .read_log_async(from, upto, filter.clone(), Some(n), None)
                .await?;
            for e in entries {
                last_sent = e.msg.id;
                if let Some(line) = view(&e, redact) {
                    send(&mut sink, line).await?;
                }
            }
        } else {
            // A page at a time: the next page is read when the last one was taken by the client.
            let mut cursor = hub
                .open_cursor(from, upto, filter.clone())
                .map_err(|e| format!("reading the log: {e}"))?;
            loop {
                let (back, page) = tokio::task::spawn_blocking(move || {
                    let page = cursor.page(REPLAY_PAGE);
                    (cursor, page)
                })
                .await
                .map_err(|e| e.to_string())?;
                cursor = back;
                let page = page.map_err(|e| format!("reading the log: {e}"))?;
                if page.is_empty() {
                    break;
                }
                for e in page {
                    last_sent = e.msg.id;
                    if let Some(line) = view(&e, redact) {
                        send(&mut sink, line).await?;
                    }
                }
            }
        }
    }
    let mut ping = tokio::time::interval(hub.cfg.ping_every);
    ping.tick().await;
    let mut check = tokio::time::interval(Duration::from_secs(2));
    check.tick().await;
    let mut stop = hub.stop.subscribe();
    let public = matches!(cred, Cred::Public);
    loop {
        tokio::select! {
            () = stopped(&mut stop) => {
                close(&mut sink, CloseCode::Restart, "hub is shutting down".into()).await;
                return Ok(());
            }
            incoming = stream.next() => match incoming {
                None | Some(Err(_)) | Some(Ok(WsMsg::Close(_))) => return Ok(()),
                Some(Ok(WsMsg::Text(_) | WsMsg::Binary(_))) if public => {
                    let e = "this reader has no key or token and can only read, join with a key or ask the owner for a token to send";
                    send(&mut sink, reply_err("?", &None, e)).await?;
                }
                Some(Ok(WsMsg::Text(t))) => {
                    let reply = hub.handle_text(member, tier0, t.as_str()).await;
                    send(&mut sink, reply).await?;
                }
                Some(Ok(WsMsg::Binary(_))) => {
                    let e = "binary frames are not accepted, send one JSON object per text frame";
                    send(&mut sink, reply_err("?", &None, e)).await?;
                }
                Some(Ok(_)) => {}
            },
            ev = recv_live(&mut live) => match ev {
                Ok(e) => {
                    if e.msg.id > upto && filter.matches(&e.msg) {
                        last_sent = e.msg.id;
                        if let Some(line) = view(&e, redact) {
                            send(&mut sink, line).await?;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let why = format!("too slow, reconnect with since={}", last_sent.max(upto) + 1);
                    close(&mut sink, CloseCode::Again, why).await;
                    return Err("lagged".into());
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = ping.tick() => {
                sink.send(WsMsg::Ping(Default::default())).await.map_err(|e| format!("ping: {e}"))?;
            }
            _ = check.tick() => {
                match cred {
                    Cred::Token(token) if !hub.token_valid(member, token) => {
                        close(&mut sink, CloseCode::Policy, "token revoked".into()).await;
                        return Err("token revoked".into());
                    }
                    Cred::Key(public) if !hub.key_valid(&member.name, public) => {
                        close(&mut sink, CloseCode::Policy, "key revoked".into()).await;
                        return Err("key revoked".into());
                    }
                    Cred::Key(_) if crate::stamp::ops::request_tier(hub, &member.name) < 0 => {
                        close(&mut sink, CloseCode::Policy, "no tier".into()).await;
                        return Err("no tier".into());
                    }
                    _ => {}
                }
                redact = hub.public_view(&member.name, cred);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_announces_offline_publications_once() {
        use crate::policy::{Change, DEFAULT_POLICY};

        let data = tempfile::tempdir().unwrap();
        let path = data.path();
        let mut tokens = TokenFile::default();
        let owner = tokens.add("owner", Role::Orchestrator).unwrap();
        tokens.save(&path.join("tokens.json")).unwrap();
        let open = || Hub::open(HubConfig::new(path)).unwrap();
        assert_eq!(open().last_id(), 0);

        let candidate = path.join("candidate.toml");
        for version in [2, 3] {
            let source = DEFAULT_POLICY.replace("version = 1", &format!("version = {version}"));
            std::fs::write(&candidate, source).unwrap();
            PolicyStore::publish(path, &owner, Change::Publish(&candidate)).unwrap();
        }

        let hub = open();
        assert_eq!(hub.last_id(), 2);
        assert_eq!(hub.policy_response()["version"], 3);
        drop(hub);
        let hub = open();
        assert_eq!(hub.last_id(), 2);
        assert_eq!(hub.policy_response()["version"], 3);
    }

    #[test]
    fn bucket_refills() {
        let t0 = Instant::now();
        let mut b = Bucket {
            tokens: 2.0,
            last: t0,
        };
        assert!(b.take(1.0, 2.0, t0).is_ok());
        assert!(b.take(1.0, 2.0, t0).is_ok());
        let wait = b.take(1.0, 2.0, t0).unwrap_err();
        assert!((wait - 1.0).abs() < 1e-9);
        assert!(b.take(1.0, 2.0, t0 + Duration::from_millis(1001)).is_ok());
    }
}
