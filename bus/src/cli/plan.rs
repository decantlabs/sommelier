//! Scheduler commands (`plan`, `sim`, `idle`, `pause`, `resume`, steps 1 and 11).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::client::{self, Opts};
use crate::exit;
use crate::msg::{Kind, Message, Role};
use crate::plan::{
    PauseScope, PlanPolicy, SafeModeState, load_plan_state_from_store, plan, plan_explain,
    sim::simulate_log_file,
};
use crate::store::Store;
use crate::time::{now_ms, rfc3339};

pub async fn run(cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    match cmd {
        "idle" => cmd_idle(args).await,
        "plan" => cmd_plan(args).await,
        "sim" => cmd_sim(args).await,
        "pause" => cmd_pause_resume(args, true).await,
        "resume" => cmd_pause_resume(args, false).await,
        _ => super::unimplemented(cmd),
    }
}

fn resolve_opts(
    url: Option<String>,
    token: Option<String>,
    token_file: Option<String>,
) -> Result<Opts, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let resolved_url = url
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
    let resolved_token = match (token.or_else(|| env("SOMMELIER_TOKEN")), token_file) {
        (Some(t), _) => t,
        (None, file) => {
            let path = file
                .or_else(|| env("SOMMELIER_TOKEN_FILE"))
                .ok_or("no token: pass --token, --token-file, or set SOMMELIER_TOKEN")?;
            fs::read_to_string(&path)
                .map_err(|e| format!("cannot read token file {path}: {e}"))?
                .trim()
                .to_owned()
        }
    };
    Ok(Opts {
        url: resolved_url,
        token: resolved_token,
    })
}

fn load_policy_file(path: &Path) -> Result<PlanPolicy, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("cannot read policy {}: {e}", path.display()))?;
    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        serde_json::from_str::<PlanPolicy>(&text)
            .map_err(|e| format!("invalid JSON policy {}: {e}", path.display()))
    } else {
        PlanPolicy::parse_toml(&text)
    }
}

async fn cmd_plan(args: &[String]) -> Result<ExitCode, String> {
    let mut dry = false;
    let mut explain: Option<String> = None;
    let mut policy_path: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut now_sec: Option<u64> = None;
    let mut json_out = false;
    let mut url: Option<String> = None;
    let mut token: Option<String> = None;
    let mut token_file: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dry" => dry = true,
            "--json" => json_out = true,
            "--explain" => {
                explain = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--explain needs a task id".to_owned())?,
                );
            }
            "--policy" => {
                policy_path = Some(PathBuf::from(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--policy needs a file path".to_owned())?,
                ));
            }
            "--data" => {
                data_dir = Some(PathBuf::from(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--data needs a directory path".to_owned())?,
                ));
            }
            "--now" => {
                let raw = it
                    .next()
                    .ok_or_else(|| "--now needs epoch seconds".to_owned())?;
                now_sec = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("--now must be an integer, got `{raw}`"))?,
                );
            }
            "--url" => {
                url = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--url needs a value".to_owned())?,
                );
            }
            "--token" => {
                token = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--token needs a value".to_owned())?,
                );
            }
            "--token-file" => {
                token_file = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--token-file needs a value".to_owned())?,
                );
            }
            other if other.starts_with("--explain=") => {
                explain = Some(other.trim_start_matches("--explain=").to_owned());
            }
            other if other.starts_with("--policy=") => {
                policy_path = Some(PathBuf::from(other.trim_start_matches("--policy=")));
            }
            other if other.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(other.trim_start_matches("--data=")));
            }
            other if other.starts_with("--now=") => {
                let raw = other.trim_start_matches("--now=");
                now_sec = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("--now must be an integer, got `{raw}`"))?,
                );
            }
            other if other.starts_with("--url=") => {
                url = Some(other.trim_start_matches("--url=").to_owned());
            }
            other if other.starts_with("--token=") => {
                token = Some(other.trim_start_matches("--token=").to_owned());
            }
            other if other.starts_with("--token-file=") => {
                token_file = Some(other.trim_start_matches("--token-file=").to_owned());
            }
            other => {
                eprintln!("sommelier: unknown argument `{other}` for `sommelier plan`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }

    let now = now_sec.unwrap_or_else(|| now_ms() / 1000);

    // Offline / local store mode when --data is given without --url.
    if let Some(dir) = data_dir
        && url.is_none()
    {
        let mut store = Store::open(&dir)?;
        let log_path = dir.join("bus.jsonl");
        if log_path.is_file() {
            let _ = store.catch_up_from_log(&log_path);
        }
        let policy = match policy_path.as_deref() {
            Some(p) => load_policy_file(p)?,
            None => PlanPolicy::default(),
        };
        let state = load_plan_state_from_store(store.connection(), &policy, 0)?;

        if let Some(tid) = explain {
            let Some(exp) = plan_explain(&state, &policy, &tid, now) else {
                if json_out {
                    println!(
                        "{}",
                        json!({"ok": false, "code": exit::REFUSED, "reason": format!("task `{tid}` not found")})
                    );
                } else {
                    eprintln!("sommelier: task `{tid}` not found");
                }
                return Ok(exit::code(exit::REFUSED));
            };
            if json_out {
                println!(
                    "{}",
                    json!({"ok": true, "code": exit::OK, "dry": true, "task": tid, "explanation": exp})
                );
            } else {
                println!(
                    "task {} state {} assigned={} wait={}",
                    exp.task_id,
                    exp.state,
                    exp.assigned_slot.as_deref().unwrap_or("-"),
                    exp.wait_reason.as_deref().unwrap_or("-")
                );
            }
            return Ok(exit::code(exit::OK));
        }

        let out = plan(&state, &policy, now);
        if json_out {
            println!(
                "{}",
                json!({
                    "ok": true,
                    "code": exit::OK,
                    "dry": dry || true,
                    "policy_hash": policy.policy_hash(),
                    "effective_mode": out.effective_mode.as_str(),
                    "actions": out.actions,
                    "wait_reasons": out.wait_reasons,
                    "safe_mode": out.safe_mode,
                })
            );
        } else {
            println!(
                "plan mode={} actions={} waiting={}",
                out.effective_mode.as_str(),
                out.actions.len(),
                out.wait_reasons.len()
            );
            for act in &out.actions {
                println!("  {}", serde_json::to_string(act).unwrap_or_default());
            }
        }
        return Ok(exit::code(exit::OK));
    }

    // Online hub mode.
    let opts = resolve_opts(url, token, token_file)?;
    let policy_toml = match policy_path.as_deref() {
        Some(p) => Some(
            fs::read_to_string(p)
                .map_err(|e| format!("cannot read policy {}: {e}", p.display()))?,
        ),
        None => None,
    };

    if let Some(tid) = explain {
        let mut req = json!({"op": "plan.explain", "rid": "cli-plan", "task": tid, "now": now});
        if let Some(pt) = policy_toml {
            req["policy_toml"] = json!(pt);
        }
        let reply = client::request_ok(&opts, req).await?;
        if json_out {
            println!(
                "{}",
                json!({"ok": true, "code": exit::OK, "task": reply["task"], "explanation": reply["explanation"]})
            );
        } else {
            let exp = &reply["explanation"];
            println!(
                "task {} state {} assigned={} wait={}",
                exp["task_id"].as_str().unwrap_or(&tid),
                exp["state"].as_str().unwrap_or(""),
                exp["assigned_slot"].as_str().unwrap_or("-"),
                exp["wait_reason"].as_str().unwrap_or("-")
            );
        }
        return Ok(exit::code(exit::OK));
    }

    let mut req = json!({"op": "plan.dry", "rid": "cli-plan", "dry": dry || true, "now": now});
    if let Some(pt) = policy_toml {
        req["policy_toml"] = json!(pt);
    }
    let reply = client::request_ok(&opts, req).await?;
    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "code": exit::OK,
                "dry": dry || true,
                "policy_hash": reply["policy_hash"],
                "effective_mode": reply["effective_mode"],
                "actions": reply["actions"],
                "wait_reasons": reply["wait_reasons"],
                "safe_mode": reply["safe_mode"],
            })
        );
    } else {
        let acts = reply["actions"].as_array().map_or(0, Vec::len);
        let waits = reply["wait_reasons"]
            .as_object()
            .map_or(0, serde_json::Map::len);
        println!(
            "plan mode={} actions={} waiting={}",
            reply["effective_mode"].as_str().unwrap_or("shadow"),
            acts,
            waits
        );
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_sim(args: &[String]) -> Result<ExitCode, String> {
    let mut log_path: Option<PathBuf> = None;
    let mut policy_path: Option<PathBuf> = None;
    let mut json_out = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--policy" => {
                policy_path = Some(PathBuf::from(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--policy needs a file path".to_owned())?,
                ));
            }
            other if other.starts_with("--policy=") => {
                policy_path = Some(PathBuf::from(other.trim_start_matches("--policy=")));
            }
            other if other.starts_with("--") => {
                eprintln!("sommelier: unknown flag `{other}` for `sommelier sim`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
            other => {
                if log_path.is_none() {
                    log_path = Some(PathBuf::from(other));
                } else {
                    eprintln!("sommelier: unexpected positional argument `{other}` for `sommelier sim`");
                    return Ok(exit::code(exit::WRONG_USAGE));
                }
            }
        }
    }

    let Some(log) = log_path else {
        eprintln!("usage: sommelier sim <LOG> [--policy FILE] [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };

    let report = simulate_log_file(&log, policy_path.as_deref())?;
    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "code": exit::OK,
                "report": report,
            })
        );
    } else {
        println!(
            "sim replayed {} messages, {} ticks ({}/{} recorded decisions matched, {} leases changed, shadow agreement {:.1}%)",
            report.messages_replayed,
            report.ticks_simulated,
            report.exact_decision_matches,
            report.recorded_decisions,
            report.total_leases_changed,
            report.shadow_comparison.agreement_ratio * 100.0
        );
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_pause_resume(args: &[String], pause: bool) -> Result<ExitCode, String> {
    let cmd_name = if pause { "pause" } else { "resume" };
    let mut pos: Vec<String> = Vec::new();
    let mut json_out = false;
    let mut data_dir: Option<PathBuf> = None;
    let mut url: Option<String> = None;
    let mut token: Option<String> = None;
    let mut token_file: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => {
                data_dir = Some(PathBuf::from(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--data needs a value".to_owned())?,
                ));
            }
            "--url" => {
                url = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--url needs a value".to_owned())?,
                );
            }
            "--token" => {
                token = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--token needs a value".to_owned())?,
                );
            }
            "--token-file" => {
                token_file = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--token-file needs a value".to_owned())?,
                );
            }
            other if other.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(other.trim_start_matches("--data=")));
            }
            other if other.starts_with("--url=") => {
                url = Some(other.trim_start_matches("--url=").to_owned());
            }
            other if other.starts_with("--token=") => {
                token = Some(other.trim_start_matches("--token=").to_owned());
            }
            other if other.starts_with("--token-file=") => {
                token_file = Some(other.trim_start_matches("--token-file=").to_owned());
            }
            other if other.starts_with("--") => {
                eprintln!("sommelier: unknown flag `{other}` for `sommelier {cmd_name}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
            other => pos.push(other.to_owned()),
        }
    }

    if pos.is_empty() {
        eprintln!("usage: sommelier {cmd_name} all | class <C> | pool <P> | owner <O> [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    }
    let scope_input = pos.join(" ");
    let parsed = match PauseScope::parse(&scope_input) {
        Ok(p) => p,
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    json!({"ok": false, "code": exit::WRONG_USAGE, "reason": e})
                );
            } else {
                eprintln!("sommelier: {e}");
            }
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let scope_key = parsed.as_key();

    // Local store mode when --data is passed without --url.
    if let Some(dir) = data_dir
        && url.is_none()
    {
        let mut store = Store::open(&dir)?;
        let log_path = dir.join("bus.jsonl");
        if log_path.is_file() {
            let _ = store.catch_up_from_log(&log_path);
        }
        let now = now_ms() / 1000;
        let ts = rfc3339(now.saturating_mul(1000));
        let mut next_id = store.cursor()? + 1;
        let mut msgs = vec![Message {
            id: next_id,
            ts: ts.clone(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Fact,
            topic: format!("pause/{scope_key}"),
            body: json!({
                "v": 1,
                "scope": scope_key,
                "paused": pause,
                "by": "owner",
                "at": now,
            })
            .to_string(),
            refs: vec![if pause { "pause:on" } else { "pause:off" }.into()],
            to: Vec::new(),
        }];
        next_id += 1;
        if !pause && parsed == PauseScope::All {
            msgs.push(Message {
                id: next_id,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Fact,
                topic: "scheduler/safe_mode".into(),
                body: serde_json::to_string(&SafeModeState::default())
                    .unwrap_or_else(|_| "{}".into()),
                refs: vec!["safe_mode:off".into()],
                to: Vec::new(),
            });
        }
        store.apply_batch(&msgs)?;
        let mut log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
        for m in &msgs {
            let line = serde_json::to_string(m).map_err(|e| e.to_string())?;
            writeln!(log_file, "{line}").map_err(|e| e.to_string())?;
        }
        if json_out {
            println!(
                "{}",
                json!({"ok": true, "code": exit::OK, "scope": scope_key, "paused": pause})
            );
        } else {
            println!("ok {cmd_name} {scope_key}");
        }
        return Ok(exit::code(exit::OK));
    }

    let opts = resolve_opts(url, token, token_file)?;
    let op = if pause { "plan.pause" } else { "plan.resume" };
    let reply = client::request_ok(
        &opts,
        json!({"op": op, "rid": "cli-plan", "scope": scope_key}),
    )
    .await?;
    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "code": exit::OK,
                "scope": reply["scope"],
                "paused": reply["paused"],
            })
        );
    } else {
        println!("ok {cmd_name} {scope_key}");
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_idle(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut url: Option<String> = None;
    let mut token: Option<String> = None;
    let mut token_file: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--url" => {
                url = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--url needs a value".to_owned())?,
                )
            }
            "--token" => {
                token = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--token needs a value".to_owned())?,
                )
            }
            "--token-file" => {
                token_file = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| "--token-file needs a value".to_owned())?,
                )
            }
            other if other.starts_with("--url=") => {
                url = Some(other.trim_start_matches("--url=").to_owned())
            }
            other if other.starts_with("--token=") => {
                token = Some(other.trim_start_matches("--token=").to_owned())
            }
            other if other.starts_with("--token-file=") => {
                token_file = Some(other.trim_start_matches("--token-file=").to_owned())
            }
            other => return Err(format!("unknown argument `{other}` for `sommelier idle`")),
        }
    }

    let opts = resolve_opts(url, token, token_file)?;
    let reply = client::request_ok(&opts, json!({"op": "idle"})).await?;

    if json_out {
        let out = json!({
            "as_of": reply["as_of"],
            "idle": reply["idle"],
            "slots": reply["slots"],
            "counts": reply["counts"],
        });
        println!("{out}");
    } else {
        let idle = reply["idle"].as_array().cloned().unwrap_or_default();
        if idle.is_empty() {
            println!("no idle slots");
        } else {
            for s in &idle {
                let str_val = |k: &str| s[k].as_str().unwrap_or("");
                let reason = s["reason"]
                    .as_str()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default();
                println!(
                    "{} {} {} idle {} s{}",
                    str_val("key"),
                    str_val("class"),
                    str_val("state"),
                    s["idle_secs"].as_u64().unwrap_or(0),
                    reason
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn format_idle_json(v: &Value) -> Value {
    json!({
        "as_of": v["as_of"],
        "idle": v["idle"],
        "slots": v["slots"],
        "counts": v["counts"],
    })
}
