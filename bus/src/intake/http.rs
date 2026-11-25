//! The HTTP side of the intake: `PUT /api/submit/<task>/<attempt>`, `GET /api/bundle/<sha256>` and
//! `GET /api/order/<task>/done_when`. See PROTOCOL.md, section Submissions.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use serde_json::json;

use super::facts::{
    SubRow, count_since, is_sha256, query_live_for, query_newest_passed_for_task, query_submission,
    submission_fact,
};
use crate::brief::{HttpOutcome, sha256_hex, validate_brief_id};
use crate::factory::Part;
use crate::gate::Risk;
use crate::gate::glob::any_match;
use crate::hub::Hub;
use crate::key::book::{KeyState, current_row};
use crate::key::crypto::{unb64, verify_request};
use crate::msg::Role;
use crate::proto::percent_decode;
use crate::stamp::tier_table::tier_from_table;
use crate::task::state::split_csv;
use crate::task::{TaskRow, query_active_lease_for_task, query_task};
use crate::time::now_ms;
use crate::tokens::Member;

/// States of a submission that passed the static gates.
pub const PASSED_GATES: [&str; 8] = [
    "gated",
    "verifying",
    "verify_failed",
    "verified",
    "reviewing",
    "review_failed",
    "approved",
    "landed",
];

fn err(status: u16, msg: impl Into<String>) -> HttpOutcome {
    HttpOutcome::json(status, &json!({"ok": false, "error": msg.into()}))
}

/// Routes the three paths of the intake. `None` means the path is not one of them.
pub fn on_http(
    hub: &Hub,
    method: &str,
    path: &str,
    query: Option<&str>,
    body: &[u8],
    caller: Option<&Member>,
) -> Option<HttpOutcome> {
    if let Some(rest) = path.strip_prefix("/api/submit/") {
        return Some(submit(hub, method, rest, query, body, caller));
    }
    if let Some(rest) = path.strip_prefix("/api/bundle/") {
        return Some(read_only(method, || bundle(hub, method, rest)));
    }
    if let Some(rest) = path.strip_prefix("/api/order/") {
        return Some(read_only(method, || order(hub, rest)));
    }
    None
}

fn read_only(method: &str, f: impl FnOnce() -> HttpOutcome) -> HttpOutcome {
    if matches!(method, "GET" | "HEAD") {
        return f();
    }
    let mut out = err(405, "only GET and HEAD are allowed here");
    out.headers.push(("Allow", "GET, HEAD".into()));
    out
}

fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    query?
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)
        .and_then(|(_, v)| percent_decode(v).ok())
}

/// Parses `<task>/<attempt>`.
fn task_and_attempt(rest: &str) -> Result<(String, u32), String> {
    let usage = "the path is /api/submit/<task>/<attempt>";
    let (task, attempt) = rest.split_once('/').ok_or(usage)?;
    let task = validate_brief_id(task)
        .map_err(|_| usage.to_owned())?
        .to_owned();
    let attempt: u32 = attempt.parse().map_err(|_| usage.to_owned())?;
    if attempt == 0 {
        return Err(usage.into());
    }
    Ok((task, attempt))
}

/// A refusal: the HTTP status and the text.
type Refusal = (u16, String);

/// Who the caller is for one order, read from the tables.
struct Who {
    task: Option<TaskRow>,
    tier: i64,
    /// The public key of a key member, `None` for a token member.
    key: Option<String>,
}

/// A key member carries no token: `check_key` gives it an empty hash and no plain token.
fn is_key_member(m: &Member) -> bool {
    m.hash.is_empty() && m.token.is_none()
}

/// Checks the head of an upload: the caller, the key row, the tier and the lease on `(task, attempt)`.
/// It reads no body and writes nothing.
fn who(
    hub: &Hub,
    caller: Option<&Member>,
    task: &str,
    attempt: u32,
) -> Result<(Who, Member), Refusal> {
    let Some(caller) = caller else {
        return Err((
            401,
            "missing credentials: send a token or sign in with your key".into(),
        ));
    };
    if caller.role == Role::Observer {
        return Err((
            403,
            "observers are read only, this token cannot submit".into(),
        ));
    }
    let key_member = is_key_member(caller);
    let read = hub.with_store(|store| {
        let conn = store.connection();
        // A key credential never falls back to the rules of a token: its row must be active.
        let key = if key_member {
            match current_row(conn, &caller.name) {
                Ok(Some((public, _, _, KeyState::Active))) => Ok(Some(public)),
                Ok(_) => Err((401, "your key is not active on this hub".to_owned())),
                Err(_) => Err((
                    500,
                    "the hub could not read its key table, try again".to_owned(),
                )),
            }
        } else {
            Ok(None)
        };
        let role = if key_member { None } else { Some(caller.role) };
        (
            key,
            query_task(conn, task).ok().flatten(),
            query_active_lease_for_task(conn, task).ok().flatten(),
            tier_from_table(conn, &caller.name, role),
        )
    });
    let Some((key, task_row, lease, tier)) = read else {
        return Err((500, "the hub could not read its tables, try again".into()));
    };
    let key = key?;
    if tier < 0 {
        return Err((403, "this member has no tier and may not submit".into()));
    }
    let lease = match lease {
        Some(l) if l.owner == caller.name => l,
        Some(_) => {
            return Err((403, format!("order `{task}` is leased to another member")));
        }
        None => {
            return Err((403, format!("you hold no active lease of order `{task}`")));
        }
    };
    if lease.attempt != attempt {
        return Err((
            409,
            format!(
                "the active lease of `{task}` is attempt {}, the request names attempt {attempt}",
                lease.attempt
            ),
        ));
    }
    Ok((
        Who {
            task: task_row,
            tier,
            key,
        },
        caller.clone(),
    ))
}

/// The bytes that the files of the store hold now.
fn store_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .fold(0u64, u64::saturating_add)
}

/// The limits of a member and of the store, checked before a body is read and again before it is stored.
fn admission(hub: &Hub, member: &str, tier: i64) -> Result<(), Refusal> {
    let cfg = &hub.config().intake;
    let day_ago = (now_ms() / 1000).saturating_sub(86_400);
    let made = hub
        .with_store(|s| count_since(s.connection(), member, day_ago))
        .unwrap_or(0);
    let cap = cfg.daily_cap_for_tier(tier);
    if made >= cap {
        return Err((
            429,
            format!(
                "you made {made} submissions in the last 24 hours, the limit for tier {tier} is {cap}: try again later"
            ),
        ));
    }
    let dir = cfg.store_path(&hub.config().data_dir);
    if store_bytes(&dir) >= cfg.max_store_bytes {
        return Err((
            507,
            "the hub store is full: old files are deleted after a few days, try again later".into(),
        ));
    }
    Ok(())
}

/// What an upload may weigh, decided from the request head alone, before the hub reads any body.
///
/// It checks that intake is on (the settings and the live policy), the caller, the key row of a key
/// member, the tier, the lease on the order and attempt of `path` (`/api/submit/<task>/<attempt>`), the
/// daily cap of the member and the size of the store. The answer is the largest body the caller may
/// send, the cap of the caller's own tier. The rule of one live submission per order and attempt needs
/// the hash and is applied by [`on_http`] after the body, because a repeat of the same bundle must still
/// be answered as known. The signature of a key member is checked there too.
pub fn upload_allowance(hub: &Hub, caller: Option<&Member>, path: &str) -> Result<u64, String> {
    upload_allowance_status(hub, caller, path).map_err(|(_, text)| text)
}

/// [`upload_allowance`] with the HTTP status of the refusal.
pub fn upload_allowance_status(
    hub: &Hub,
    caller: Option<&Member>,
    path: &str,
) -> Result<u64, (u16, String)> {
    let cfg = &hub.config().intake;
    if !crate::factory::part_on(hub.config(), Part::Intake) {
        return Err((
            503,
            "this hub does not take submissions: intake is switched off".into(),
        ));
    }
    let rest = path
        .strip_prefix("/api/submit/")
        .ok_or((400, "the path is /api/submit/<task>/<attempt>".to_owned()))?;
    let (task, attempt) = task_and_attempt(rest).map_err(|e| (400, e))?;
    let (w, member) = who(hub, caller, &task, attempt)?;
    admission(hub, &member.name, w.tier)?;
    Ok(cfg.cap_for_tier(w.tier))
}

/// True when the scope of an order meets a hold path: one pattern covers the other.
fn meets_hold_path(scope: &[String], hold_paths: &[String]) -> bool {
    scope.iter().any(|s| {
        any_match(hold_paths, s, true)
            || hold_paths
                .iter()
                .any(|h| any_match(std::slice::from_ref(s), h, true))
    })
}

/// The risk of an order before the gates run: guarded when the scope meets a hold path.
pub fn order_risk(scope: &str, hold_paths: &[String]) -> Risk {
    let globs: Vec<String> = split_csv(scope).into_iter().map(str::to_owned).collect();
    if meets_hold_path(&globs, hold_paths) {
        Risk::Guarded
    } else {
        Risk::Normal
    }
}

fn risk_word(r: Risk) -> &'static str {
    match r {
        Risk::Low => "low",
        Risk::Normal => "normal",
        Risk::Guarded => "guarded",
    }
}

fn submit(
    hub: &Hub,
    method: &str,
    rest: &str,
    query: Option<&str>,
    body: &[u8],
    caller: Option<&Member>,
) -> HttpOutcome {
    let cfg = &hub.config().intake;
    if method != "PUT" {
        let mut out = err(405, "only PUT is allowed on /api/submit/<task>/<attempt>");
        out.headers.push(("Allow", "PUT".into()));
        return out;
    }
    if !crate::factory::part_on(hub.config(), Part::Intake) {
        return err(
            503,
            "this hub does not take submissions: intake is switched off",
        );
    }
    let (task, attempt) = match task_and_attempt(rest) {
        Ok(v) => v,
        Err(e) => return err(400, e),
    };
    let Some(sha) = query_param(query, "sha256").filter(|s| is_sha256(s)) else {
        return err(
            400,
            "send `?sha256=<64 lower case hex characters>` with the SHA 256 of the bundle",
        );
    };
    let (w, member) = match who(hub, caller, &task, attempt) {
        Ok(v) => v,
        Err((code, e)) => return err(code, e),
    };
    if let Some(public) = &w.key {
        let Some(sig) = query_param(query, "sig") else {
            return err(
                401,
                "a key member must send `sig`, the signature of the request, see PROTOCOL.md Submissions",
            );
        };
        let request =
            json!({"op": "submit", "task": task, "attempt": attempt, "sha256": sha, "sig": sig});
        let Ok(public) = unb64(public) else {
            return err(500, "the stored key is damaged");
        };
        if let Err(e) = verify_request(&request, &public) {
            return err(401, format!("bad signature on the submission: {e}"));
        }
    }
    let cap = cfg.cap_for_tier(w.tier);
    if body.len() as u64 > cap {
        return err(
            413,
            format!(
                "the bundle has {} bytes, the limit for tier {} is {cap}: send a smaller change",
                body.len(),
                w.tier
            ),
        );
    }
    if sha256_hex(body) != sha {
        return err(
            400,
            "the SHA 256 of the body differs from `sha256`: send the file again",
        );
    }
    let Some(task_row) = w.task.clone() else {
        return err(404, format!("order `{task}` is not known"));
    };
    let _one_at_a_time = cfg.runtime.upload.lock().unwrap_or_else(|e| e.into_inner());
    // The tables again under the lock: another upload may have just finished.
    let seen = hub.with_store(|s| {
        let conn = s.connection();
        (
            query_submission(conn, &sha),
            query_live_for(conn, &task, attempt),
        )
    });
    let Some((known, live)) = seen else {
        return err(500, "the hub could not read its tables, try again");
    };
    if let Some(row) = known {
        if row.member == member.name && row.task == task && row.attempt == attempt {
            return known_reply(&row, true);
        }
        return err(
            409,
            "this bundle was already submitted for another order or by another member",
        );
    }
    // One live submission per order and attempt. A member who cannot get a bundle through has to wait
    // for the first one to fail, so one lease never grinds the review pick with many bundles.
    if let Some(live) = live {
        return err(
            409,
            format!(
                "order `{task}` attempt {attempt} already has the submission {} in the state `{}`: send a new bundle only after it failed and the order came back",
                live.sha256, live.state
            ),
        );
    }
    if let Err((code, e)) = admission(hub, &member.name, w.tier) {
        return err(code, e);
    }
    let dir = cfg.store_path(&hub.config().data_dir);
    let head = match store_bundle(&dir, &sha, body) {
        Ok(h) => h,
        Err((code, e)) => return err(code, e),
    };
    let row = SubRow {
        sha256: sha.clone(),
        task: task.clone(),
        attempt,
        member: member.name.clone(),
        base: task_row.base_sha.clone(),
        head,
        bytes: body.len() as u64,
        state: "received".into(),
        risk: risk_word(order_risk(&task_row.scope, &cfg.gate_rules.hold_paths)).into(),
        tier: w.tier,
        received_at: now_ms() / 1000,
        updated_at: now_ms() / 1000,
    };
    if let Err(e) = hub.append_hook_message(submission_fact(&row, "received", now_ms() / 1000)) {
        let _ = fs::remove_file(dir.join(format!("{sha}.bundle")));
        return err(500, format!("the hub could not record the submission: {e}"));
    }
    known_reply(&row, false)
}

fn known_reply(row: &SubRow, known: bool) -> HttpOutcome {
    HttpOutcome::json(
        200,
        &json!({
            "ok": true, "submission": row.sha256, "task": row.task, "attempt": row.attempt,
            "state": row.state, "bytes": row.bytes, "known": known,
        }),
    )
}

/// Writes the bundle under its hash through a temporary file and returns the head named in its
/// header. A body that is no git bundle is refused and leaves nothing behind.
fn store_bundle(dir: &Path, sha: &str, body: &[u8]) -> Result<String, (u16, String)> {
    let io = |e: std::io::Error| (500, format!("the hub could not store the bundle: {e}"));
    fs::create_dir_all(dir).map_err(io)?;
    let tmp = dir.join(format!("{sha}.bundle.part"));
    let mut f = fs::File::create(&tmp).map_err(io)?;
    f.write_all(body).map_err(io)?;
    f.sync_all().map_err(io)?;
    drop(f);
    let head = match crate::bundle::parse::read_header(&tmp) {
        Ok(h) if h.refs.len() == 1 => h.refs[0].0.clone(),
        Ok(h) => {
            let _ = fs::remove_file(&tmp);
            return Err((
                400,
                format!(
                    "a bundle must hold exactly one ref, this one holds {}: pack one head only",
                    h.refs.len()
                ),
            ));
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err((400, e.to_string()));
        }
    };
    fs::rename(&tmp, dir.join(format!("{sha}.bundle"))).map_err(io)?;
    Ok(head)
}

fn bundle(hub: &Hub, method: &str, rest: &str) -> HttpOutcome {
    let cfg = &hub.config().intake;
    if !cfg.enabled || !is_sha256(rest) {
        return err(404, "no such bundle");
    }
    let passed = hub
        .with_store(|s| query_submission(s.connection(), rest))
        .flatten()
        .is_some_and(|r| PASSED_GATES.contains(&r.state.as_str()));
    if !passed {
        return err(404, "no such bundle");
    }
    let file = cfg
        .store_path(&hub.config().data_dir)
        .join(format!("{rest}.bundle"));
    let mut out = HttpOutcome {
        status: 200,
        content_type: "application/octet-stream",
        body: Vec::new(),
        headers: vec![
            ("Cache-Control", "no-store".into()),
            ("X-Bundle-Sha256", rest.to_owned()),
        ],
    };
    if method == "HEAD" {
        // A HEAD never reads the file: the size comes from the folder entry.
        return match fs::metadata(&file) {
            Ok(m) if m.is_file() => {
                out.headers.push(("X-Bundle-Bytes", m.len().to_string()));
                out
            }
            _ => err(404, "no such bundle"),
        };
    }
    // The read is bounded by the largest bundle any tier may send, so a file that grew behind our back
    // cannot fill the memory.
    let limit = cfg.max_body_bytes().saturating_add(1);
    let read = fs::File::open(&file).and_then(|f| {
        let mut buf = Vec::new();
        f.take(limit).read_to_end(&mut buf)?;
        Ok(buf)
    });
    match read {
        Ok(bytes) if (bytes.len() as u64) < limit => {
            out.body = bytes;
            out
        }
        _ => err(404, "no such bundle"),
    }
}

fn order(hub: &Hub, rest: &str) -> HttpOutcome {
    let cfg = &hub.config().intake;
    let Some(task) = rest.strip_suffix("/done_when") else {
        return err(404, "the path is /api/order/<task>/done_when");
    };
    if !cfg.enabled || validate_brief_id(task).is_err() {
        return err(404, "no such order");
    }
    // One row by the task, not a scan of every submission under the store lock.
    let found = hub
        .with_store(|s| {
            let conn = s.connection();
            let sub = query_newest_passed_for_task(conn, task, &PASSED_GATES)?;
            Some((sub, query_task(conn, task).ok().flatten()?))
        })
        .flatten();
    let Some((sub, row)) = found else {
        return err(404, "no such order");
    };
    let scope: Vec<&str> = split_csv(&row.scope);
    HttpOutcome::json(
        200,
        &json!({"task": row.id, "base": sub.base, "scope": scope, "done_when": row.done_when}),
    )
}
