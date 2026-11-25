//! The key facts of the log: parsing, the in memory book of the hub and the KEY table.
//!
//! Only a FACT from the hub with topic `key/<member>` counts. Both the book and the table
//! fold the same parsed events, so a replay of the log gives the same keys.

use rusqlite::{Connection, Transaction, params};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};

use super::crypto::{b64, is_member_id, member_id, unb64, valid_handle, verify_request};
use crate::msg::{Kind, Message, Role};
use crate::store::StoreModule;

/// Version of the FACT body.
pub const FACT_VERSION: u64 = 1;

/// The state of a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    Active,
    Rotated,
    Revoked,
}

impl KeyState {
    /// The word stored in the table and shown in `/api/state`.
    pub fn as_str(self) -> &'static str {
        match self {
            KeyState::Active => "active",
            KeyState::Rotated => "rotated",
            KeyState::Revoked => "revoked",
        }
    }
}

/// What a key fact says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A new member with its first key.
    Join,
    /// The key `prev` is replaced by the key `public`.
    Rotate { prev: String },
    /// The key `public` is revoked.
    Revoke { reason: String },
}

/// One parsed and checked key fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub member: String,
    /// The public key of the fact in base64url: the new key after a rotation.
    pub public: String,
    pub handle: String,
    pub joined_at: String,
    pub change: Change,
    pub msg_id: u64,
}

/// The text a rotation signs, built by the member and rebuilt by the hub.
pub fn rotate_request(member: &str, old_pub: &str, new_pub: &str) -> Value {
    json!({"op": "key.rotate", "member": member, "old_pub": old_pub, "new_pub": new_pub})
}

/// The text a revocation signs.
pub fn revoke_request(member: &str, public: &str, reason: &str) -> Value {
    json!({"op": "key.revoke", "member": member, "pub": public, "reason": reason})
}

/// Body of the FACT that registers a member.
pub fn join_body(member: &str, public: &str, handle: &str, joined_at: &str) -> String {
    json!({"v": FACT_VERSION, "member": member, "pub": public, "handle": handle,
           "state": "active", "joined_at": joined_at})
    .to_string()
}

/// Body of the FACT that replaces the key of a member. `sig` is the signature of the old
/// key over [`rotate_request`].
pub fn rotate_body(
    member: &str,
    prev: &str,
    public: &str,
    handle: &str,
    joined_at: &str,
    sig: &str,
) -> String {
    json!({"v": FACT_VERSION, "member": member, "pub": public, "handle": handle,
           "state": "active", "joined_at": joined_at, "op": "rotate", "prev": prev, "sig": sig})
    .to_string()
}

/// Body of the FACT that revokes a key. `proof` is the signature of the member over
/// [`revoke_request`], or the name of the orchestrator that revoked it.
pub fn revoke_body(
    member: &str,
    public: &str,
    handle: &str,
    joined_at: &str,
    reason: &str,
    proof: Proof<'_>,
) -> String {
    let mut v = json!({"v": FACT_VERSION, "member": member, "pub": public, "handle": handle,
           "state": "revoked", "joined_at": joined_at, "op": "revoke", "reason": reason});
    match proof {
        Proof::Signature(sig) => v["sig"] = json!(sig),
        Proof::Owner(name) => v["by"] = json!(name),
    }
    v.to_string()
}

/// Who stands behind a revocation.
pub enum Proof<'a> {
    Signature(&'a str),
    Owner(&'a str),
}

fn decode_public(text: &str) -> Option<Vec<u8>> {
    unb64(text).ok().filter(|b| b.len() == 32)
}

/// Reads a key fact. `None` for anything that is not a well formed key fact of the hub, so
/// a forged or damaged line changes nothing.
pub fn parse_fact(msg: &Message) -> Option<KeyEvent> {
    if msg.kind != Kind::Fact || msg.role != Role::Hub || msg.from != "hub" {
        return None;
    }
    let topic_member = msg.topic.strip_prefix("key/")?;
    let v: Value = serde_json::from_str(&msg.body).ok()?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    if v.get("v").and_then(Value::as_u64) != Some(FACT_VERSION) {
        return None;
    }
    let member = text("member")?;
    if member != topic_member || !is_member_id(&member) {
        return None;
    }
    let public = text("pub")?;
    decode_public(&public)?;
    let handle = text("handle")?;
    if !valid_handle(&handle) {
        return None;
    }
    let joined_at = text("joined_at")?;
    let change = match text("op").as_deref() {
        None => {
            if text("state")? != "active" || member_id(&decode_public(&public)?) != member {
                return None;
            }
            Change::Join
        }
        Some("rotate") => {
            let prev = text("prev")?;
            let prev_bytes = decode_public(&prev)?;
            let request = rotate_request(&member, &prev, &public);
            let mut signed = request;
            signed["sig"] = json!(text("sig")?);
            verify_request(&signed, &prev_bytes).ok()?;
            Change::Rotate { prev }
        }
        Some("revoke") => {
            let reason = text("reason")?;
            if let Some(sig) = text("sig") {
                let mut signed = revoke_request(&member, &public, &reason);
                signed["sig"] = json!(sig);
                verify_request(&signed, &decode_public(&public)?).ok()?;
            } else if text("by").is_none() {
                return None;
            }
            Change::Revoke { reason }
        }
        Some(_) => return None,
    };
    Some(KeyEvent {
        member,
        public,
        handle,
        joined_at,
        change,
        msg_id: msg.id,
    })
}

/// The current key of a member as the hub sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub public: String,
    pub handle: String,
    pub joined_at: String,
    pub state: KeyState,
    pub reason: String,
}

/// Why a key may not connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The current public key, base64url.
    Active(String),
    Revoked,
    Unknown,
}

/// How a key stands with the book when it asks to join.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Standing {
    /// The key is known (now or in the past) and belongs to this member.
    Known(String),
    /// The member id is in use by another key.
    Taken,
    /// Neither the key nor the member id is known.
    New,
}

/// The keys of all members, folded from the log. The hub keeps one and checks it at every
/// connect, so no database is read on that path.
#[derive(Clone, Debug, Default)]
pub struct KeyBook {
    members: BTreeMap<String, Record>,
    /// Every key that ever belonged to a member (the first and the rotated ones).
    by_pub: HashMap<String, String>,
}

impl KeyBook {
    /// Folds one message of the log.
    pub fn apply(&mut self, msg: &Message) {
        if let Some(ev) = parse_fact(msg) {
            self.apply_event(&ev);
        }
    }

    fn apply_event(&mut self, ev: &KeyEvent) {
        match &ev.change {
            Change::Join => {
                if self.members.contains_key(&ev.member) || self.by_pub.contains_key(&ev.public) {
                    return;
                }
                self.by_pub.insert(ev.public.clone(), ev.member.clone());
                self.members.insert(
                    ev.member.clone(),
                    Record {
                        public: ev.public.clone(),
                        handle: ev.handle.clone(),
                        joined_at: ev.joined_at.clone(),
                        state: KeyState::Active,
                        reason: String::new(),
                    },
                );
            }
            Change::Rotate { prev } => {
                if self.by_pub.contains_key(&ev.public) {
                    return;
                }
                let Some(r) = self.members.get_mut(&ev.member) else {
                    return;
                };
                if r.state != KeyState::Active || r.public != *prev {
                    return;
                }
                r.public = ev.public.clone();
                self.by_pub.insert(ev.public.clone(), ev.member.clone());
            }
            Change::Revoke { reason } => {
                let Some(r) = self.members.get_mut(&ev.member) else {
                    return;
                };
                if r.state != KeyState::Active || r.public != ev.public {
                    return;
                }
                r.state = KeyState::Revoked;
                r.reason = reason.clone();
            }
        }
    }

    /// The member that owns `public` (now or in the past), base64url.
    pub fn member_of_pub(&self, public: &str) -> Option<&str> {
        self.by_pub.get(public).map(String::as_str)
    }

    /// Whether `public` may join as `member`.
    pub fn standing(&self, member: &str, public: &str) -> Standing {
        if let Some(existing) = self.member_of_pub(public) {
            return Standing::Known(existing.to_owned());
        }
        if self.members.contains_key(member) {
            return Standing::Taken;
        }
        Standing::New
    }

    /// The record of a member.
    pub fn get(&self, member: &str) -> Option<&Record> {
        self.members.get(member)
    }

    /// What a connection of `member` may use.
    pub fn lookup(&self, member: &str) -> Lookup {
        match self.members.get(member) {
            None => Lookup::Unknown,
            Some(r) if r.state == KeyState::Active => Lookup::Active(r.public.clone()),
            Some(_) => Lookup::Revoked,
        }
    }

    /// True while `public` is the current key of an active `member`.
    pub fn is_current(&self, member: &str, public: &str) -> bool {
        self.lookup(member) == Lookup::Active(public.to_owned())
    }

    /// Number of members.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// True without any member.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS "KEY" (
    member TEXT NOT NULL,
    public_key TEXT NOT NULL,
    handle TEXT NOT NULL,
    joined_at TEXT NOT NULL,
    state TEXT NOT NULL,
    revoked_reason TEXT NOT NULL DEFAULT '',
    msg_id INTEGER NOT NULL,
    PRIMARY KEY (member, public_key)
);
"#;

/// Store projection module of this folder.
pub struct KeyModule;

impl KeyModule {
    fn apply_event(tx: &Transaction<'_>, ev: &KeyEvent) -> Result<(), String> {
        let e = |x: rusqlite::Error| x.to_string();
        match &ev.change {
            Change::Join => {
                let known: i64 = tx
                    .query_row(
                        r#"SELECT COUNT(*) FROM "KEY" WHERE member = ?1 OR public_key = ?2"#,
                        params![ev.member, ev.public],
                        |r| r.get(0),
                    )
                    .map_err(e)?;
                if known == 0 {
                    tx.execute(
                        r#"INSERT INTO "KEY" (member, public_key, handle, joined_at, state, revoked_reason, msg_id)
                           VALUES (?1, ?2, ?3, ?4, 'active', '', ?5)"#,
                        params![ev.member, ev.public, ev.handle, ev.joined_at, ev.msg_id as i64],
                    )
                    .map_err(e)?;
                }
            }
            Change::Rotate { prev } => {
                let taken: i64 = tx
                    .query_row(
                        r#"SELECT COUNT(*) FROM "KEY" WHERE public_key = ?1"#,
                        params![ev.public],
                        |r| r.get(0),
                    )
                    .map_err(e)?;
                if taken > 0 {
                    return Ok(());
                }
                let n = tx
                    .execute(
                        r#"UPDATE "KEY" SET state = 'rotated'
                           WHERE member = ?1 AND public_key = ?2 AND state = 'active'"#,
                        params![ev.member, prev],
                    )
                    .map_err(e)?;
                if n > 0 {
                    tx.execute(
                        r#"INSERT INTO "KEY" (member, public_key, handle, joined_at, state, revoked_reason, msg_id)
                           VALUES (?1, ?2, ?3, ?4, 'active', '', ?5)"#,
                        params![ev.member, ev.public, ev.handle, ev.joined_at, ev.msg_id as i64],
                    )
                    .map_err(e)?;
                }
            }
            Change::Revoke { reason } => {
                tx.execute(
                    r#"UPDATE "KEY" SET state = 'revoked', revoked_reason = ?3
                       WHERE member = ?1 AND public_key = ?2 AND state = 'active'"#,
                    params![ev.member, ev.public, reason],
                )
                .map_err(e)?;
            }
        }
        Ok(())
    }
}

impl StoreModule for KeyModule {
    fn name(&self) -> &'static str {
        "key"
    }

    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn apply(&self, tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
        match parse_fact(msg) {
            Some(ev) => Self::apply_event(tx, &ev),
            None => Ok(()),
        }
    }

    fn api(
        &self,
        conn: &Connection,
        _now_sec: u64,
        state: &mut Map<String, Value>,
    ) -> Result<(), String> {
        let rows = read_keys(conn)?;
        state.insert("keys".into(), Value::Array(rows));
        Ok(())
    }
}

/// One row per member: its handle, the state of its newest key and when it joined.
fn read_keys(conn: &Connection) -> Result<Vec<Value>, String> {
    let mut stmt = conn
        .prepare(
            r#"SELECT member, handle, state, joined_at FROM "KEY" k
               WHERE msg_id = (SELECT MAX(msg_id) FROM "KEY" WHERE member = k.member)
               ORDER BY member"#,
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(
                json!({"member": r.get::<_, String>(0)?, "handle": r.get::<_, String>(1)?,
                      "state": r.get::<_, String>(2)?, "joined_at": r.get::<_, String>(3)?}),
            )
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// The current key row of a member, read from the table: `(public, handle, joined_at, state)`.
pub fn current_row(
    conn: &Connection,
    member: &str,
) -> Result<Option<(String, String, String, KeyState)>, String> {
    let mut stmt = conn
        .prepare(
            r#"SELECT public_key, handle, joined_at, state FROM "KEY"
               WHERE member = ?1 ORDER BY msg_id DESC LIMIT 1"#,
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query(params![member]).map_err(|e| e.to_string())?;
    let Some(r) = rows.next().map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let state = match r.get::<_, String>(3).map_err(|e| e.to_string())?.as_str() {
        "active" => KeyState::Active,
        "rotated" => KeyState::Rotated,
        _ => KeyState::Revoked,
    };
    Ok(Some((
        r.get(0).map_err(|e| e.to_string())?,
        r.get(1).map_err(|e| e.to_string())?,
        r.get(2).map_err(|e| e.to_string())?,
        state,
    )))
}

/// True when some row of the table holds `public`.
pub fn public_known(conn: &Connection, public: &str) -> Result<bool, String> {
    conn.query_row(
        r#"SELECT COUNT(*) FROM "KEY" WHERE public_key = ?1"#,
        params![public],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .map_err(|e| e.to_string())
}

/// Base64url of 32 bytes, for callers that hold raw bytes.
pub fn public_text(bytes: &[u8]) -> String {
    b64(bytes)
}
