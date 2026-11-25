//! The HTTP side of the intake: `PUT /api/submit/<task>/<attempt>`, `GET /api/bundle/<sha256>` and
//! `GET /api/order/<task>/done_when`. See PROTOCOL.md, section Submissions.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde_json::json;

use super::facts::{SubRow, is_sha256, query_by_states, query_submission, submission_fact};
use crate::brief::{HttpOutcome, sha256_hex, validate_brief_id};
use crate::gate::Risk;
use crate::gate::glob::any_match;
use crate::hub::Hub;
use crate::key::book::{KeyState, current_row};
use crate::key::crypto::{unb64, verify_request};
use crate::msg::Role;
use crate::proto::percent_decode;
use crate::stamp::tier_table::tier_from_table;
use crate::task::state::split_csv;
use crate::task::{LeaseRow, TaskRow, query_active_lease_for_task, query_task};
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
        return Some(read_only(method, || bundle(hub, rest)));
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

/// What the upload needs from the tables.
struct Facts {
    task: Option<TaskRow>,
    lease: Option<LeaseRow>,
    tier: i64,
    /// The public key of a key member, `None` for a token member.
    key: Option<String>,
    known: Option<SubRow>,
}

fn load(hub: &Hub, caller: &Member, task: &str, sha: &str) -> Option<Facts> {
    hub.with_store(|store| {
        let conn = store.connection();
        let key = if caller.hash.is_empty() && caller.token.is_none() {
            match current_row(conn, &caller.name) {
                Ok(Some((public, _, _, KeyState::Active))) => Some(public),
                _ => None,
            }
        } else {
            None
        };
        let role = if key.is_some() {
            None
        } else {
            Some(caller.role)
        };
        Facts {
            task: query_task(conn, task).ok().flatten(),
            lease: query_active_lease_for_task(conn, task).ok().flatten(),
            tier: tier_from_table(conn, &caller.name, role),
            key,
            known: query_submission(conn, sha),
        }
    })
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
    if !cfg.enabled {
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
    let Some(caller) = caller else {
        return err(
            401,
            "missing credentials: send a token or sign in with your key",
        );
    };
    if caller.role == Role::Observer {
        return err(403, "observers are read only, this token cannot submit");
    }
    let Some(f) = load(hub, caller, &task, &sha) else {
        return err(500, "the hub could not read its tables, try again");
    };
    if f.tier < 0 {
        return err(403, "this member has no tier and may not submit");
    }
    let lease = match f.lease {
        Some(l) if l.owner == caller.name => l,
        Some(_) => return err(403, format!("order `{task}` is leased to another member")),
        None => return err(403, format!("you hold no active lease of order `{task}`")),
    };
    if lease.attempt != attempt {
        return err(
            409,
            format!(
                "the active lease of `{task}` is attempt {}, the request names attempt {attempt}",
                lease.attempt
            ),
        );
    }
    if let Some(public) = &f.key {
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
    let cap = cfg.cap_for_tier(f.tier);
    if body.len() as u64 > cap {
        return err(
            413,
            format!(
                "the bundle has {} bytes, the limit for tier {} is {cap}: send a smaller change",
                body.len(),
                f.tier
            ),
        );
    }
    if sha256_hex(body) != sha {
        return err(
            400,
            "the SHA 256 of the body differs from `sha256`: send the file again",
        );
    }
    let Some(task_row) = f.task else {
        return err(404, format!("order `{task}` is not known"));
    };
    let _one_at_a_time = cfg.runtime.upload.lock().unwrap_or_else(|e| e.into_inner());
    // A second look under the lock: another upload of the same hash may have just finished.
    let known = hub
        .with_store(|s| query_submission(s.connection(), &sha))
        .flatten()
        .or(f.known);
    if let Some(row) = known {
        if row.member == caller.name && row.task == task && row.attempt == attempt {
            return known_reply(&row, true);
        }
        return err(
            409,
            "this bundle was already submitted for another order or by another member",
        );
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
        member: caller.name.clone(),
        base: task_row.base_sha.clone(),
        head,
        bytes: body.len() as u64,
        state: "received".into(),
        risk: risk_word(order_risk(&task_row.scope, &cfg.gate_rules.hold_paths)).into(),
        tier: f.tier,
        received_at: now_ms() / 1000,
        updated_at: now_ms() / 1000,
    };
    if let Err(e) = hub.append_hook_message(submission_fact(&row, "received", now_ms() / 1000)) {
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

fn bundle(hub: &Hub, rest: &str) -> HttpOutcome {
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
    match fs::read(&file) {
        Ok(bytes) => {
            let mut out = HttpOutcome {
                status: 200,
                content_type: "application/octet-stream",
                body: bytes,
                headers: vec![("Cache-Control", "no-store".into())],
            };
            out.headers.push(("X-Bundle-Sha256", rest.to_owned()));
            out
        }
        Err(_) => err(404, "no such bundle"),
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
    let found = hub
        .with_store(|s| {
            let conn = s.connection();
            let sub = query_by_states(conn, &PASSED_GATES)
                .into_iter()
                .rfind(|r| r.task == task)?;
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
