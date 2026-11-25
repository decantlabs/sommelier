//! Run orders of the test network: the RUN_ORDER table and the match of an order to a machine.
//!
//! A run order says "this build, this title profile, this scenario, on hardware like this, wanted
//! so many reports". The table is a projection of FACT messages of the hub with topic
//! `runorder/<id>`. The first FACT of an id creates the row. Later FACTs of the same id change only
//! its state: `open` to `settled` or `closed`, `settled` to `closed`. `closed` is final. Testers are
//! not leased workers, so these orders have nothing to do with the task queue.

use rusqlite::{Connection, Transaction, params};
use serde_json::{Map, Value, json};

use super::types::MachineClass;
use super::{machine_key, privacy::find_private};
use crate::msg::{Kind, Message, Role};

/// DDL of the RUN_ORDER table.
pub const ORDER_SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS RUN_ORDER (
    id TEXT NOT NULL PRIMARY KEY,
    build TEXT NOT NULL,
    channel TEXT NOT NULL,
    profile TEXT NOT NULL,
    scenario TEXT NOT NULL,
    needs TEXT NOT NULL,
    wanted INTEGER NOT NULL,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT 0,
    msg_id INTEGER NOT NULL DEFAULT 0
);
";

/// Version of the FACT body.
pub const ORDER_FACT_VERSION: u64 = 1;

/// Most reports one order may want.
pub const MAX_WANTED: usize = 50;

const MAX_ID: usize = 64;

/// What hardware an order needs. A missing field means any.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    pub os: Option<String>,
    pub gpu: Option<String>,
}

impl Needs {
    /// The JSON form, only the fields that are set.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(os) = &self.os {
            m.insert("os".into(), json!(os));
        }
        if let Some(gpu) = &self.gpu {
            m.insert("gpu".into(), json!(gpu));
        }
        Value::Object(m)
    }

    /// Reads `{"os"?, "gpu"?}`. The error is one line.
    pub fn from_json(v: &Value) -> Result<Needs, String> {
        let Some(obj) = v.as_object() else {
            return Err("`needs` must be an object with optional `os` and `gpu`".into());
        };
        for k in obj.keys() {
            if k != "os" && k != "gpu" {
                return Err(format!("`needs.{k}` is unknown, use `os` and `gpu`"));
            }
        }
        let text = |k: &str| -> Result<Option<String>, String> {
            match obj.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => {
                    let s = s.trim().to_lowercase();
                    if s.is_empty() || s.len() > 64 || s.chars().any(char::is_control) {
                        Err(format!("`needs.{k}` is 1 to 64 characters"))
                    } else if !find_private(&s).is_empty() {
                        Err(format!("`needs.{k}` holds private data, remove it"))
                    } else {
                        Ok(Some(s))
                    }
                }
                Some(_) => Err(format!("`needs.{k}` must be text")),
            }
        };
        let os = text("os")?;
        if let Some(os) = &os
            && os != "windows"
            && os != "linux"
        {
            return Err("`needs.os` must be `windows` or `linux`".into());
        }
        Ok(Needs {
            os,
            gpu: text("gpu")?,
        })
    }

    /// True when the machine class fits. The GPU matches when the normalised card name of the
    /// machine contains the normalised need, so `4080` and `rtx 4080` both fit an RTX 4080.
    pub fn matches(&self, m: &MachineClass) -> bool {
        if let Some(os) = &self.os
            && os.trim().to_lowercase() != m.os.trim().to_lowercase()
        {
            return false;
        }
        if let Some(gpu) = &self.gpu {
            let have = gpu_part(m);
            let want = gpu_part(&MachineClass {
                os: String::new(),
                os_build: String::new(),
                gpu: gpu.clone(),
                gpu_memory_mb: 0,
                driver: String::new(),
                memory_mb: 0,
            });
            if !have.contains(&want) {
                return false;
            }
        }
        true
    }
}

/// The normalised GPU part of the machine key.
fn gpu_part(m: &MachineClass) -> String {
    let key = machine_key(m);
    key.split_once('/')
        .map(|(_, g)| g.to_string())
        .unwrap_or(key)
}

/// One row of the RUN_ORDER table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderRow {
    pub id: String,
    pub build: String,
    pub channel: String,
    pub profile: String,
    pub scenario: String,
    pub needs: Needs,
    pub wanted: usize,
    /// `open`, `settled` or `closed`.
    pub state: String,
    /// UTC epoch seconds.
    pub created_at: u64,
    /// Message id of the first FACT: the order in which builds were put to the test.
    pub msg_id: u64,
}

impl OrderRow {
    /// The FACT body of this order with a state.
    pub fn fact_body(&self, state: &str) -> String {
        json!({
            "v": ORDER_FACT_VERSION,
            "id": self.id,
            "build": self.build,
            "channel": self.channel,
            "profile": self.profile,
            "scenario": self.scenario,
            "needs": self.needs.to_json(),
            "wanted": self.wanted,
            "state": state,
            "created_at": self.created_at,
        })
        .to_string()
    }
}

fn plain_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ID
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A commit: 7 to 64 hex digits, lower case.
pub fn plain_commit(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Checks the text fields of an order. The error is one line.
pub fn check_order_fields(
    build: &str,
    channel: &str,
    profile: &str,
    scenario: &str,
    channels: &[String],
) -> Result<(), String> {
    if !plain_commit(build) {
        return Err("`build` must be a commit of 7 to 64 lower case hex digits".into());
    }
    if !channels.iter().any(|c| c == channel) {
        return Err(format!("`channel` must be one of: {}", channels.join(", ")));
    }
    for (name, v) in [("profile", profile), ("scenario", scenario)] {
        if !plain_id(v) {
            return Err(format!(
                "`{name}` must be 1 to {MAX_ID} characters of letters, digits, `.`, `_` or `-`"
            ));
        }
    }
    Ok(())
}

/// Reads a run order FACT of the hub. `None` for anything else.
pub fn parse_order_fact(msg: &Message) -> Option<OrderRow> {
    if msg.kind != Kind::Fact || msg.role != Role::Hub || msg.from != "hub" {
        return None;
    }
    let topic_id = msg.topic.strip_prefix("runorder/")?;
    let v: Value = serde_json::from_str(msg.body.trim()).ok()?;
    if v.get("v").and_then(Value::as_u64) != Some(ORDER_FACT_VERSION) {
        return None;
    }
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let id = text("id")?;
    if id != topic_id || !plain_id(&id) {
        return None;
    }
    let (build, channel, profile, scenario) = (
        text("build")?,
        text("channel")?,
        text("profile")?,
        text("scenario")?,
    );
    if !plain_commit(&build) || !plain_id(&channel) || !plain_id(&profile) || !plain_id(&scenario) {
        return None;
    }
    let state = text("state")?;
    if !matches!(state.as_str(), "open" | "settled" | "closed") {
        return None;
    }
    let wanted = v.get("wanted").and_then(Value::as_u64)? as usize;
    if wanted == 0 || wanted > MAX_WANTED {
        return None;
    }
    let needs = Needs::from_json(v.get("needs").unwrap_or(&json!({}))).ok()?;
    Some(OrderRow {
        id,
        build,
        channel,
        profile,
        scenario,
        needs,
        wanted,
        state,
        created_at: v.get("created_at").and_then(Value::as_u64).unwrap_or(0),
        msg_id: msg.id,
    })
}

/// Projects one message into RUN_ORDER. Messages that are no order FACT of the hub change nothing.
pub fn apply(tx: &Transaction<'_>, msg: &Message) -> Result<(), String> {
    let Some(o) = parse_order_fact(msg) else {
        return Ok(());
    };
    tx.execute(
        "INSERT INTO RUN_ORDER (id, build, channel, profile, scenario, needs, wanted, state,
                                created_at, msg_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET state = excluded.state
         WHERE RUN_ORDER.state = 'open'
            OR (RUN_ORDER.state = 'settled' AND excluded.state = 'closed')",
        params![
            o.id,
            o.build,
            o.channel,
            o.profile,
            o.scenario,
            o.needs.to_json().to_string(),
            o.wanted as i64,
            o.state,
            o.created_at as i64,
            o.msg_id as i64,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// All rows in the order they were created.
pub fn query_orders(conn: &Connection) -> Result<Vec<OrderRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, build, channel, profile, scenario, needs, wanted, state, created_at, msg_id
             FROM RUN_ORDER ORDER BY msg_id ASC, id ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            let needs: String = r.get(5)?;
            let needs = serde_json::from_str::<Value>(&needs)
                .ok()
                .and_then(|v| Needs::from_json(&v).ok())
                .unwrap_or_default();
            Ok(OrderRow {
                id: r.get(0)?,
                build: r.get(1)?,
                channel: r.get(2)?,
                profile: r.get(3)?,
                scenario: r.get(4)?,
                needs,
                wanted: r.get::<_, i64>(6)?.max(1) as usize,
                state: r.get(7)?,
                created_at: r.get::<_, i64>(8)?.max(0) as u64,
                msg_id: r.get::<_, i64>(9)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// How many reports each run order has, by run id.
pub fn report_counts(conn: &Connection) -> Result<Vec<(String, usize)>, String> {
    let mut stmt = conn
        .prepare("SELECT run, COUNT(*) FROM REPORT GROUP BY run")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)?.max(0) as usize)))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// The runs a member already reported on.
pub fn reported_by(conn: &Connection, member: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT run FROM REPORT WHERE member = ?1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![member], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<String>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// The open orders that fit a machine and the profiles of a tester, best first: the order with
/// the fewest reports so far, then the oldest. `reported` are runs the member already answered.
pub fn candidates<'a>(
    orders: &'a [OrderRow],
    counts: &[(String, usize)],
    reported: &[String],
    machine: &MachineClass,
    profiles: &[String],
) -> Vec<&'a OrderRow> {
    let count_of = |id: &str| {
        counts
            .iter()
            .find(|(r, _)| r == id)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    };
    let mut out: Vec<&OrderRow> = orders
        .iter()
        .filter(|o| o.state == "open")
        .filter(|o| profiles.iter().any(|p| p == &o.profile))
        .filter(|o| o.needs.matches(machine))
        .filter(|o| !reported.iter().any(|r| r == &o.id))
        .collect();
    out.sort_by(|a, b| (count_of(&a.id), a.msg_id, &a.id).cmp(&(count_of(&b.id), b.msg_id, &b.id)));
    out
}
