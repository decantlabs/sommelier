//! Exception commands (`alerts`, `alert`, `digest`, section 5.1, 8.3).
//!
//! Implements:
//! - `sommelier alerts [--open] [--code C1,C2] [--wait SECS] [--limit N] [--data DIR] [--url URL] [--token T | --token-file F] [--json]`
//! - `sommelier alert ack <KEY> [--epoch N] [--who WHO] [--role R] [--data DIR] [--url URL] [--token T | --token-file F] [--json]`

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::client::{self, Opts};
use crate::exit;
use crate::hooks::OpContext;
use crate::msg::{Kind, Message, Role};
use crate::role::{self, RoleCommand, RoleStore};
use crate::store::{AlertTableRow, Store, query_alerts};
use crate::time::{now_ms, rfc3339};

const ALERTS_USAGE: &str = "\
usage:
  sommelier alerts [--open] [--code C1,C2] [--wait SECS] [--limit N] [--data DIR] [--url URL] [--token T | --token-file F] [--json]
";

const ALERT_USAGE: &str = "\
usage:
  sommelier alert ack <KEY> [--epoch N] [--who WHO] [--role R] [--data DIR] [--url URL] [--token T | --token-file F] [--json]
";

struct ParsedArgs {
    pos: Vec<String>,
    vals: HashMap<String, Vec<String>>,
    bools: Vec<String>,
}

impl ParsedArgs {
    fn parse(raw: &[String], value_flags: &[&str], bool_flags: &[&str]) -> Result<Self, String> {
        let mut a = Self {
            pos: Vec::new(),
            vals: HashMap::new(),
            bools: Vec::new(),
        };
        let mut it = raw.iter();
        while let Some(s) = it.next() {
            if s == "--" {
                a.pos.extend(it.by_ref().cloned());
                break;
            }
            let Some(flag) = s.strip_prefix("--") else {
                a.pos.push(s.clone());
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (flag, None),
            };
            if bool_flags.contains(&name) {
                a.bools.push(name.into());
            } else if value_flags.contains(&name) {
                let v = match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .cloned()
                        .ok_or_else(|| format!("--{name} needs a value"))?,
                };
                a.vals.entry(name.into()).or_default().push(v);
            } else {
                return Err(format!("unknown flag --{name}"));
            }
        }
        Ok(a)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.vals
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.vals.get(name).cloned().unwrap_or_default()
    }

    fn flag(&self, name: &str) -> bool {
        self.bools.iter().any(|b| b == name)
    }
}

fn client_opts(a: &ParsedArgs) -> Result<Opts, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = a
        .get("url")
        .map(str::to_owned)
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
    let token = match (
        a.get("token")
            .map(str::to_owned)
            .or_else(|| env("SOMMELIER_TOKEN")),
        a.get("token-file"),
    ) {
        (Some(t), _) => t,
        (None, file) => {
            let path = file
                .map(str::to_owned)
                .or_else(|| env("SOMMELIER_TOKEN_FILE"))
                .or_else(crate::key::client::default_marker_file)
                .ok_or("no token: pass --token, --token-file, set SOMMELIER_TOKEN, or run `sommelier join`")?;
            fs::read_to_string(&path)
                .map_err(|e| format!("cannot read token file {path}: {e}"))?
                .trim()
                .to_owned()
        }
    };
    Ok(Opts { url, token })
}

async fn call_op(opts: &Opts, op: &str, body: Value) -> Result<Value, String> {
    let mut payload = match body {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    payload.insert("op".into(), json!(op));
    payload.insert("rid".into(), json!("cli-alerts"));
    client::request_ok(opts, Value::Object(payload)).await
}

pub async fn run(cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    match cmd {
        "alerts" => cmd_alerts(args).await,
        "alert" => cmd_alert(args).await,
        _ => super::unimplemented(cmd),
    }
}

fn parse_codes(raw: &[String]) -> Vec<String> {
    raw.iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_uppercase())
        .collect()
}

/// Filters and sorts alerts oldest-first (`opened_at ASC, msg_id ASC, code ASC, key ASC`), capped at `limit`.
pub fn filter_and_sort_alerts(
    rows: Vec<AlertTableRow>,
    open_only: bool,
    codes: &[String],
    limit: usize,
) -> Vec<AlertTableRow> {
    let mut filtered: Vec<AlertTableRow> = rows
        .into_iter()
        .filter(|r| {
            let state_ok = if open_only {
                r.state.eq_ignore_ascii_case("open")
            } else {
                true
            };
            let code_ok = codes.is_empty() || codes.iter().any(|c| c.eq_ignore_ascii_case(&r.code));
            state_ok && code_ok
        })
        .collect();
    filtered.sort_by(|a, b| {
        a.opened_at
            .cmp(&b.opened_at)
            .then_with(|| a.msg_id.cmp(&b.msg_id))
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.key.cmp(&b.key))
    });
    if limit > 0 && filtered.len() > limit {
        filtered.truncate(limit);
    }
    filtered
}

fn matches_alert_target(row: &AlertTableRow, target: &str) -> bool {
    let t = target.trim();
    if t.is_empty() {
        return false;
    }
    let full = format!("{}/{}", row.code, row.key);
    let topic = format!("alert/{}/{}", row.code, row.key);
    let msg_id_str = row.msg_id.to_string();
    let hash_msg_id = format!("#{}", row.msg_id);
    if full.eq_ignore_ascii_case(t) || topic.eq_ignore_ascii_case(t) {
        return true;
    }
    if row.key.eq_ignore_ascii_case(t) || row.code.eq_ignore_ascii_case(t) {
        return true;
    }
    if row.msg_id > 0 && (t == msg_id_str || t == hash_msg_id) {
        return true;
    }
    false
}

fn select_alert_to_ack(rows: &[AlertTableRow], target: &str) -> Option<AlertTableRow> {
    let matching: Vec<&AlertTableRow> = rows
        .iter()
        .filter(|r| matches_alert_target(r, target))
        .collect();
    if matching.is_empty() {
        return None;
    }
    // Prefer an open alert when multiple codes share the same key.
    matching
        .iter()
        .find(|r| r.state.eq_ignore_ascii_case("open"))
        .or_else(|| matching.first())
        .copied()
        .cloned()
}

fn check_epoch_fence_in_store(
    conn: &rusqlite::Connection,
    data_dir: Option<&Path>,
    role_name: &str,
    who: Option<&str>,
    given_epoch: u64,
) -> Result<(), String> {
    let sql_row = role::query_row(conn, role_name, "global").map_err(|e| e.to_string())?;
    let row = match sql_row {
        Some(r) => Some(r),
        None => data_dir
            .and_then(|d| RoleStore::new(d).load().ok())
            .and_then(|t| t.get(role_name).cloned()),
    };
    let Some(row) = row else {
        return Err(format!(
            "you are no longer {role_name} (your epoch {given_epoch}, current 0), stop"
        ));
    };
    role::check_fence(&row, who, given_epoch, Some(RoleCommand::AckAlert)).map_err(|r| r.message)
}

fn build_ack_message(row: &AlertTableRow, by: &str, now_sec: u64) -> Message {
    let ts = rfc3339(now_sec.saturating_mul(1000));
    Message {
        id: 0,
        ts,
        from: by.to_owned(),
        role: Role::Hub,
        kind: Kind::Alert,
        topic: format!("alert/{}/{}", row.code, row.key),
        body: json!({
            "v": 1,
            "code": row.code,
            "key": row.key,
            "severity": row.severity,
            "to": row.to,
            "state": "acked",
            "opened_at": row.opened_at,
            "last_at": now_sec,
            "count": row.count,
            "text": row.text,
            "data": row.data,
        })
        .to_string(),
        refs: vec!["state:acked".into()],
        to: Vec::new(),
    }
}

/// Module ops wired into `src/hooks.rs`: `alert.list` / `alerts.list` and `alert.ack`.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    let now_sec = body["now"].as_u64().unwrap_or(ctx.now_ms / 1000);
    let mut out = Map::new();

    match op {
        "alert.list" | "alerts.list" => {
            let open_only = body["open"].as_bool().unwrap_or(false);
            let limit = body["limit"].as_u64().unwrap_or(20) as usize;
            let codes: Vec<String> = match body.get("codes") {
                Some(Value::Array(arr)) => arr
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_ascii_uppercase())
                    .filter(|s| !s.is_empty())
                    .collect(),
                Some(Value::String(s)) => parse_codes(std::slice::from_ref(s)),
                _ => Vec::new(),
            };
            let all = query_alerts(ctx.store.connection())?;
            let alerts = filter_and_sort_alerts(all, open_only, &codes, limit);
            out.insert("as_of".into(), json!(now_sec));
            out.insert("alerts".into(), json!(alerts));
            Ok(out)
        }
        "alert.ack" => {
            let key_target = body["key"]
                .as_str()
                .or_else(|| body["id"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "alert.ack requires `key`".to_owned())?;

            if let Some(given_epoch) = body.get("epoch").and_then(Value::as_u64) {
                let role_name = body["role"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("coordinator");
                let who = body["who"].as_str().filter(|s| !s.is_empty());
                let data_dir = ctx.store.path().and_then(Path::parent);
                check_epoch_fence_in_store(
                    ctx.store.connection(),
                    data_dir,
                    role_name,
                    who,
                    given_epoch,
                )?;
            }

            let all = query_alerts(ctx.store.connection())?;
            let Some(target_row) = select_alert_to_ack(&all, key_target) else {
                return Err(format!("alert `{key_target}` not found"));
            };

            let mut msg = build_ack_message(&target_row, &ctx.caller.name, now_sec);
            msg.from = "hub".into();
            msg.role = Role::Hub;
            let msg_id = ctx.emit(msg)?;

            let mut updated = target_row;
            updated.state = "acked".into();
            updated.last_at = now_sec;
            updated.msg_id = msg_id;

            out.insert("alert".into(), json!(updated));
            out.insert("code".into(), json!(updated.code));
            out.insert("key".into(), json!(updated.key));
            out.insert("state".into(), json!("acked"));
            out.insert("msg_id".into(), json!(msg_id));
            Ok(out)
        }
        other => Err(format!("unknown alert op `{other}`")),
    }
}

fn load_local_alerts(
    dir: &Path,
    open_only: bool,
    codes: &[String],
    limit: usize,
) -> Result<Vec<AlertTableRow>, String> {
    let mut store = Store::open(dir)?;
    let log_path = dir.join("bus.jsonl");
    if log_path.is_file() {
        let _ = store.catch_up_from_log(&log_path);
    }
    let all = query_alerts(store.connection())?;
    Ok(filter_and_sort_alerts(all, open_only, codes, limit))
}

fn print_alerts(alerts: &[AlertTableRow], now_sec: u64, json_out: bool) {
    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "code": exit::OK,
                "as_of": now_sec,
                "alerts": alerts,
            })
        );
    } else if alerts.is_empty() {
        println!("no alerts");
    } else {
        for a in alerts {
            let age = now_sec.saturating_sub(a.opened_at);
            println!(
                "{}/{} [{}] {} state={} age={}s count={} DATA: {}",
                a.code, a.key, a.severity, a.to, a.state, age, a.count, a.text
            );
        }
    }
}

async fn cmd_alerts(raw: &[String]) -> Result<ExitCode, String> {
    if raw.iter().any(|a| a == "--help" || a == "-h") {
        print!("{ALERTS_USAGE}");
        return Ok(exit::code(exit::OK));
    }
    let a = match ParsedArgs::parse(
        raw,
        &[
            "code",
            "wait",
            "limit",
            "data",
            "url",
            "token",
            "token-file",
        ],
        &["open", "json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    if !a.pos.is_empty() {
        eprintln!("sommelier: unexpected positional arguments for `sommelier alerts`");
        return Ok(exit::code(exit::WRONG_USAGE));
    }

    let json_out = a.flag("json");
    let codes = parse_codes(&a.all("code"));
    let wait_opt: Option<u64> = match a.get("wait") {
        Some(v) => match v.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                eprintln!("sommelier: --wait needs a number, got `{v}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        },
        None => None,
    };
    let limit: usize = match a.get("limit") {
        Some(v) => match v.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("sommelier: --limit needs a number, got `{v}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        },
        None => 20,
    };
    // When `--open` is passed or when `--wait` / `--code` loop is used (`sommelier alerts --code BACKLOG_LOW,CONSULT --wait 3600`),
    // filter to open alerts if `--open` or `--wait` is active.
    let open_only = a.flag("open") || wait_opt.is_some();
    let wait_secs = wait_opt.unwrap_or(0);
    let deadline = Instant::now() + Duration::from_secs(wait_secs);

    let use_local = a.get("data").is_some()
        && a.get("url").is_none()
        && std::env::var("SOMMELIER_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none();

    if use_local {
        let dir = PathBuf::from(a.get("data").unwrap());
        loop {
            let alerts = load_local_alerts(&dir, open_only, &codes, limit)?;
            let now_sec = now_ms() / 1000;
            if !alerts.is_empty() {
                print_alerts(&alerts, now_sec, json_out);
                return Ok(exit::code(exit::OK));
            }
            if wait_opt.is_none() {
                print_alerts(&alerts, now_sec, json_out);
                return Ok(exit::code(exit::OK));
            }
            if Instant::now() >= deadline {
                if json_out {
                    println!(
                        "{}",
                        json!({
                            "ok": false,
                            "code": exit::NO_WORK,
                            "reason": "no open alerts",
                            "alerts": [],
                        })
                    );
                } else {
                    println!("no open alerts");
                }
                return Ok(exit::code(exit::NO_WORK));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    let opts = match client_opts(&a) {
        Ok(o) => o,
        Err(e) => {
            if let Some(d) = a.get("data") {
                let dir = PathBuf::from(d);
                let alerts = load_local_alerts(&dir, open_only, &codes, limit)?;
                let now_sec = now_ms() / 1000;
                if alerts.is_empty() && wait_opt.is_some() {
                    if json_out {
                        println!(
                            "{}",
                            json!({
                                "ok": false,
                                "code": exit::NO_WORK,
                                "reason": "no open alerts",
                                "alerts": [],
                            })
                        );
                    } else {
                        println!("no open alerts");
                    }
                    return Ok(exit::code(exit::NO_WORK));
                }
                print_alerts(&alerts, now_sec, json_out);
                return Ok(exit::code(exit::OK));
            }
            return Err(e);
        }
    };

    loop {
        let reply = call_op(
            &opts,
            "alert.list",
            json!({
                "open": open_only,
                "codes": codes,
                "limit": limit,
            }),
        )
        .await?;
        let alerts: Vec<AlertTableRow> =
            serde_json::from_value(reply["alerts"].clone()).unwrap_or_default();
        let now_sec = reply["as_of"].as_u64().unwrap_or_else(|| now_ms() / 1000);
        if !alerts.is_empty() {
            print_alerts(&alerts, now_sec, json_out);
            return Ok(exit::code(exit::OK));
        }
        if wait_opt.is_none() {
            print_alerts(&alerts, now_sec, json_out);
            return Ok(exit::code(exit::OK));
        }
        if Instant::now() >= deadline {
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": false,
                        "code": exit::NO_WORK,
                        "reason": "no open alerts",
                        "alerts": [],
                    })
                );
            } else {
                println!("no open alerts");
            }
            return Ok(exit::code(exit::NO_WORK));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn cmd_alert(raw: &[String]) -> Result<ExitCode, String> {
    if raw.iter().any(|a| a == "--help" || a == "-h") {
        print!("{ALERT_USAGE}");
        return Ok(exit::code(exit::OK));
    }
    let Some((sub, rest)) = raw.split_first() else {
        eprintln!("{ALERT_USAGE}");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    match sub.as_str() {
        "ack" => cmd_alert_ack(rest).await,
        other => {
            eprintln!("sommelier: unknown subcommand `alert {other}`\n{ALERT_USAGE}");
            Ok(exit::code(exit::WRONG_USAGE))
        }
    }
}

fn output_refusal(reason: &str, json_out: bool) -> ExitCode {
    if json_out {
        println!(
            "{}",
            json!({
                "ok": false,
                "code": exit::REFUSED,
                "reason": reason,
            })
        );
    } else {
        eprintln!("sommelier: {reason}");
    }
    exit::code(exit::REFUSED)
}

async fn cmd_alert_ack(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(
        raw,
        &["epoch", "who", "role", "data", "url", "token", "token-file"],
        &["json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(key_target) = a.pos.first() else {
        eprintln!("sommelier: usage: sommelier alert ack <KEY> [--epoch N] [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let json_out = a.flag("json");
    let epoch: Option<u64> = match a.get("epoch") {
        Some(v) => match v.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                eprintln!("sommelier: --epoch needs a number, got `{v}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        },
        None => None,
    };
    let role_name = a.get("role").unwrap_or("coordinator");
    let who = a.get("who");

    let use_local = a.get("data").is_some()
        && a.get("url").is_none()
        && std::env::var("SOMMELIER_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none();

    if use_local {
        let dir = PathBuf::from(a.get("data").unwrap());
        return ack_local(&dir, key_target, role_name, who, epoch, json_out);
    }

    let opts = match client_opts(&a) {
        Ok(o) => o,
        Err(e) => {
            if let Some(d) = a.get("data") {
                return ack_local(Path::new(d), key_target, role_name, who, epoch, json_out);
            }
            return Err(e);
        }
    };

    let mut req = json!({
        "key": key_target,
        "role": role_name,
    });
    if let Some(ep) = epoch {
        req["epoch"] = json!(ep);
    }
    if let Some(w) = who {
        req["who"] = json!(w);
    }

    match call_op(&opts, "alert.ack", req).await {
        Ok(reply) => {
            let code = reply["code"].as_str().unwrap_or("");
            let key = reply["key"].as_str().unwrap_or(key_target);
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "alert": reply["alert"],
                        "alert_code": code,
                        "key": key,
                        "state": "acked",
                        "msg_id": reply["msg_id"],
                    })
                );
            } else {
                println!("ok alert {code}/{key} acked");
            }
            Ok(exit::code(exit::OK))
        }
        Err(e) => Ok(output_refusal(&e, json_out)),
    }
}

fn ack_local(
    dir: &Path,
    key_target: &str,
    role_name: &str,
    who: Option<&str>,
    epoch: Option<u64>,
    json_out: bool,
) -> Result<ExitCode, String> {
    let mut store = Store::open(dir)?;
    let log_path = dir.join("bus.jsonl");
    if log_path.is_file() {
        let _ = store.catch_up_from_log(&log_path);
    }
    if let Some(given_epoch) = epoch
        && let Err(reason) =
            check_epoch_fence_in_store(store.connection(), Some(dir), role_name, who, given_epoch)
    {
        return Ok(output_refusal(&reason, json_out));
    }
    let all = query_alerts(store.connection())?;
    let Some(target_row) = select_alert_to_ack(&all, key_target) else {
        return Ok(output_refusal(
            &format!("alert `{key_target}` not found"),
            json_out,
        ));
    };

    let now_sec = now_ms() / 1000;
    let next_id = store.cursor()? + 1;
    let mut msg = build_ack_message(&target_row, who.unwrap_or("coordinator"), now_sec);
    msg.id = next_id;
    msg.from = "hub".into();
    msg.role = Role::Hub;

    store.apply(&msg)?;
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
    let line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
    writeln!(log_file, "{line}").map_err(|e| e.to_string())?;

    let mut updated = target_row;
    updated.state = "acked".into();
    updated.last_at = now_sec;
    updated.msg_id = next_id;

    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "code": exit::OK,
                "alert": updated,
                "alert_code": updated.code,
                "key": updated.key,
                "state": "acked",
                "msg_id": next_id,
            })
        );
    } else {
        println!("ok alert {}/{} acked", updated.code, updated.key);
    }
    Ok(exit::code(exit::OK))
}
