//! Supervisor commands (`sommelier agent`, `sommelier watch`, steps 7 and 12a).
//!
//! Step 7 implements:
//! - `sommelier agent exec --name N --class C --branch B [--model M] [--task ID] [--out DIR] [--worktree DIR] [--text TEXT] [--min-free-gb N] [--inject-free-gb N] [--heartbeat-sec N] [--timeout-sec N] [--json] -- COMMAND...`
//! - `sommelier agent status [--data DIR] [--json]`
//! - `sommelier agent stop [NAME] [--data DIR] [--json]`
//!
//! Step 12a implements:
//! - `sommelier agent run [--config FILE] [--class C[:N]] [--mode auto|own] [--gpu-slots N] [--accepts K1,K2] [--repo DIR] [--worktrees DIR] [--hooks-dir DIR] [--hook FILE] [--runs-dir DIR] [--fork REMOTE] [--role-seat ROLE] [--wait SECS] [--once | --max-iterations N] [--data DIR] [--url URL] [--token T | --token-file F] [--json]`

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::json;

use crate::agent::children::ChildrenStore;
use crate::agent::disk::DiskGuardConfig;
use crate::agent::exec::{ExecConfig, exec_worker, watch_children_once};
use crate::agent::lease_loop::{SupervisorConfig, run_supervisor};
use crate::client::Opts;
use crate::derive::CapacityMode;
use crate::exit;

pub async fn run(cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    if cmd != "agent" {
        return super::unimplemented(cmd);
    }
    let Some((sub, rest)) = args.split_first() else {
        eprintln!(
            "usage: sommelier agent run [--config FILE] | exec --name N --class C --branch B -- COMMAND... | watch [--once] | status | stop [NAME]"
        );
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    match sub.as_str() {
        "run" => cmd_run(rest).await,
        "exec" => cmd_exec(rest).await,
        "watch" => cmd_watch(rest).await,
        "status" => cmd_status(rest),
        "stop" => cmd_stop(rest),
        other => {
            eprintln!("sommelier: unknown agent subcommand `{other}` (run, exec, watch, status, stop)");
            Ok(exit::code(exit::WRONG_USAGE))
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn cmd_run(args: &[String]) -> Result<ExitCode, String> {
    let mut config_path: Option<PathBuf> = None;
    let mut owner: Option<String> = None;
    let mut machine: Option<String> = None;
    let mut os: Option<String> = None;
    let mut gpu: Option<String> = None;
    let mut mode: Option<CapacityMode> = None;
    let mut gpu_slots: Option<u32> = None;
    let mut classes: BTreeMap<String, u32> = BTreeMap::new();
    let mut accepts: Vec<String> = Vec::new();
    let mut repo_clone: Option<PathBuf> = None;
    let mut worktrees_dir: Option<PathBuf> = None;
    let mut hooks_dir: Option<PathBuf> = None;
    let mut hook_override: Option<PathBuf> = None;
    let mut fork_remote: Option<String> = None;
    let mut runs_dir: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut url: Option<String> = None;
    let mut token: Option<String> = None;
    let mut token_file: Option<String> = None;
    let mut wait_secs: Option<u64> = None;
    let mut heartbeat_ms: Option<u64> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut max_iterations: Option<usize> = None;
    let mut role_seat: Option<String> = None;
    let mut min_free_gb: Option<u64> = None;
    let mut inject_free_gb: Option<u64> = None;
    let mut json_out = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--json" {
            json_out = true;
            continue;
        }
        if arg == "--once" {
            max_iterations = Some(1);
            continue;
        }
        let Some(stripped) = arg.strip_prefix("--") else {
            eprintln!("sommelier: unexpected argument `{arg}` for `agent run`");
            return Ok(exit::code(exit::WRONG_USAGE));
        };
        let (flag, val) = match stripped.split_once('=') {
            Some((k, v)) => (k, v.to_owned()),
            None => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier: --{stripped} needs a value");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                (stripped, v.clone())
            }
        };

        match flag {
            "config" => config_path = Some(PathBuf::from(val)),
            "owner" => owner = Some(val),
            "machine" => machine = Some(val),
            "os" => os = Some(val),
            "gpu" => gpu = Some(val),
            "mode" => {
                let Some(m) = CapacityMode::parse(&val) else {
                    eprintln!("sommelier: --mode must be auto or own");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                mode = Some(m);
            }
            "gpu-slots" => {
                gpu_slots = Some(val.parse().map_err(|_| "--gpu-slots requires an integer")?);
            }
            "class" => {
                for item in val.split(',') {
                    let item = item.trim();
                    if item.is_empty() {
                        continue;
                    }
                    if let Some((c, n_str)) = item.split_once(':') {
                        let n: u32 = n_str
                            .parse()
                            .map_err(|_| format!("invalid class slot count in `{item}`"))?;
                        classes.insert(c.trim().to_owned(), n);
                    } else {
                        classes.insert(item.to_owned(), 1);
                    }
                }
            }
            "accepts" => {
                for k in val.split(',') {
                    let k = k.trim();
                    if !k.is_empty() {
                        accepts.push(k.to_owned());
                    }
                }
            }
            "repo" => repo_clone = Some(PathBuf::from(val)),
            "worktrees" => worktrees_dir = Some(PathBuf::from(val)),
            "hooks-dir" => hooks_dir = Some(PathBuf::from(val)),
            "hook" => hook_override = Some(PathBuf::from(val)),
            "fork" => fork_remote = Some(val),
            "runs-dir" => runs_dir = Some(PathBuf::from(val)),
            "data" => data_dir = Some(PathBuf::from(val)),
            "url" => url = Some(val),
            "token" => token = Some(val),
            "token-file" => token_file = Some(val),
            "wait" => {
                wait_secs = Some(val.parse().map_err(|_| "--wait requires an integer")?);
            }
            "heartbeat-ms" => {
                heartbeat_ms = Some(
                    val.parse()
                        .map_err(|_| "--heartbeat-ms requires an integer")?,
                );
            }
            "timeout-ms" => {
                timeout_ms = Some(
                    val.parse()
                        .map_err(|_| "--timeout-ms requires an integer")?,
                );
            }
            "max-iterations" => {
                max_iterations = Some(
                    val.parse()
                        .map_err(|_| "--max-iterations requires an integer")?,
                );
            }
            "role-seat" => role_seat = Some(val),
            "min-free-gb" => {
                min_free_gb = Some(
                    val.parse()
                        .map_err(|_| "--min-free-gb requires an integer")?,
                );
            }
            "inject-free-gb" => {
                inject_free_gb = Some(
                    val.parse()
                        .map_err(|_| "--inject-free-gb requires an integer")?,
                );
            }
            other => {
                eprintln!("sommelier: unknown flag --{other} for `agent run`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }

    let mut cfg = if let Some(path) = config_path {
        SupervisorConfig::from_file(&path)?
    } else {
        SupervisorConfig::default()
    };

    if let Some(v) = owner {
        cfg.owner = v;
    }
    if let Some(v) = machine {
        cfg.machine = v;
    }
    if let Some(v) = os {
        cfg.os = v;
    }
    if let Some(v) = gpu {
        cfg.gpu = v;
    }
    if let Some(v) = mode {
        cfg.mode = v;
    }
    if let Some(v) = gpu_slots {
        cfg.gpu_slots = v;
    }
    if !classes.is_empty() {
        cfg.classes = classes;
    }
    if !accepts.is_empty() {
        cfg.accepts = accepts;
    }
    if let Some(v) = repo_clone {
        cfg.repo_clone = v;
    }
    if let Some(v) = data_dir {
        cfg.data_dir = v;
    }
    if let Some(v) = worktrees_dir {
        cfg.worktrees_dir = v;
    }
    if let Some(v) = hooks_dir {
        cfg.hooks_dir = v;
    }
    if let Some(v) = hook_override {
        cfg.hook_override = Some(v);
    }
    if let Some(v) = fork_remote {
        cfg.fork_remote = (!v.is_empty()).then_some(v);
    }
    if let Some(v) = runs_dir {
        cfg.runs_dir = Some(v);
    }
    if let Some(v) = wait_secs {
        cfg.wait_secs = v;
    }
    if let Some(v) = heartbeat_ms {
        cfg.heartbeat_ms = v;
    }
    if let Some(v) = timeout_ms {
        cfg.child_timeout_ms = Some(v);
    }
    if let Some(v) = max_iterations {
        cfg.max_iterations = Some(v);
    }
    if let Some(v) = role_seat {
        cfg.role_seat = Some(v);
    }
    if let Some(v) = min_free_gb {
        cfg.min_free_gb = v;
    }
    if let Some(v) = inject_free_gb {
        cfg.injected_free_gb = Some(v);
    }
    cfg.client_opts = resolve_client_opts(url, token, token_file);

    match run_supervisor(&cfg).await {
        Ok(summary) => {
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "summary": summary,
                    })
                );
            } else {
                println!(
                    "ok agent run finished: {} task(s), paused={}, gentle={}, gpu_slots={}",
                    summary.tasks_run.len(),
                    summary.paused,
                    summary.gentle,
                    summary.gpu_slots_reported
                );
            }
            Ok(exit::code(exit::OK))
        }
        Err(err) => {
            let c = if err.contains("DISK_LOW") {
                exit::REFUSED
            } else {
                exit::INTERNAL_ERROR
            };
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": false,
                        "code": c,
                        "reason": err,
                    })
                );
            } else {
                eprintln!("sommelier: {err}");
            }
            Ok(exit::code(c))
        }
    }
}

async fn cmd_exec(args: &[String]) -> Result<ExitCode, String> {
    let mut name: Option<String> = None;
    let mut class: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut model: Option<String> = None;
    let mut task_id: Option<String> = None;
    let mut text: Option<String> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut worktree: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut url: Option<String> = None;
    let mut token: Option<String> = None;
    let mut token_file: Option<String> = None;
    let mut min_free_gb: Option<u64> = None;
    let mut inject_free_gb: Option<u64> = None;
    let mut inject_free_bytes: Option<u64> = None;
    let mut heartbeat_sec: u64 = 60;
    let mut timeout_sec: Option<u64> = None;
    let mut json_out = false;
    let mut command: Vec<String> = Vec::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            command.extend(it.cloned());
            break;
        }
        if arg == "--json" {
            json_out = true;
            continue;
        }
        let (flag, val) = if let Some(stripped) = arg.strip_prefix("--") {
            match stripped.split_once('=') {
                Some((k, v)) => (k, v.to_owned()),
                None => {
                    let Some(v) = it.next() else {
                        eprintln!("sommelier: --{stripped} needs a value");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    };
                    (stripped, v.clone())
                }
            }
        } else {
            command.push(arg.clone());
            command.extend(it.cloned());
            break;
        };

        match flag {
            "name" => name = Some(val),
            "class" => class = Some(val),
            "branch" => branch = Some(val),
            "model" => model = Some(val),
            "task" => task_id = Some(val),
            "text" => text = Some(val),
            "out" => out_dir = Some(PathBuf::from(val)),
            "worktree" => worktree = Some(PathBuf::from(val)),
            "data" => data_dir = Some(PathBuf::from(val)),
            "url" => url = Some(val),
            "token" => token = Some(val),
            "token-file" => token_file = Some(val),
            "min-free-gb" => {
                min_free_gb = Some(
                    val.parse()
                        .map_err(|_| "--min-free-gb requires an integer")?,
                )
            }
            "inject-free-gb" => {
                inject_free_gb = Some(
                    val.parse()
                        .map_err(|_| "--inject-free-gb requires an integer")?,
                )
            }
            "inject-free-bytes" => {
                inject_free_bytes = Some(
                    val.parse()
                        .map_err(|_| "--inject-free-bytes requires an integer")?,
                )
            }
            "heartbeat-sec" => {
                heartbeat_sec = val
                    .parse()
                    .map_err(|_| "--heartbeat-sec requires an integer")?
            }
            "timeout-sec" => {
                timeout_sec = Some(
                    val.parse()
                        .map_err(|_| "--timeout-sec requires an integer")?,
                )
            }
            other => {
                eprintln!("sommelier: unknown flag --{other} for `agent exec`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }

    let (Some(name), Some(class), Some(branch)) = (name, class, branch) else {
        eprintln!(
            "sommelier: usage: sommelier agent exec --name N --class C --branch B -- COMMAND..."
        );
        return Ok(exit::code(exit::WRONG_USAGE));
    };

    if command.is_empty() {
        eprintln!("sommelier: `agent exec` requires a command after `--`");
        return Ok(exit::code(exit::WRONG_USAGE));
    }

    let resolved_data = data_dir.unwrap_or_else(crate::default_data_dir);
    let mut cfg = ExecConfig::new(name, class, branch, command, resolved_data);
    if let Some(m) = model {
        cfg.model = m;
    }
    if let Some(t) = task_id {
        cfg.task_id = Some(t);
    }
    if let Some(txt) = text {
        cfg.text = txt;
    }
    if let Some(out) = out_dir {
        cfg.out_dir = Some(out);
    }
    if let Some(wt) = worktree {
        cfg.worktree = Some(wt);
    }
    cfg.heartbeat_interval = Duration::from_secs(heartbeat_sec.max(1));
    cfg.timeout = timeout_sec.map(Duration::from_secs);

    let mut disk_cfg = DiskGuardConfig::default();
    if let Some(min_gb) = min_free_gb {
        disk_cfg = disk_cfg.with_min_free_gb(min_gb);
    }
    if let Some(gb) = inject_free_gb {
        disk_cfg = disk_cfg.with_injected_free_gb(gb);
    }
    if let Some(bytes) = inject_free_bytes {
        disk_cfg = disk_cfg.with_injected_free_bytes(bytes);
    }
    cfg.disk_guard = disk_cfg;
    cfg.client_opts = resolve_client_opts(url, token, token_file);

    match exec_worker(&cfg).await {
        Ok(outcome) => {
            if json_out {
                let out = json!({
                    "ok": true,
                    "code": exit::OK,
                    "name": outcome.name,
                    "class": outcome.class,
                    "branch": outcome.branch,
                    "pid": outcome.pid,
                    "exit_code": outcome.exit_code,
                    "reason": outcome.classification.reason.as_str(),
                    "vendor": outcome.classification.vendor,
                    "resets_at": outcome.classification.resets_at,
                    "reset_secs": outcome.classification.reset_secs,
                    "detail": outcome.classification.detail,
                    "hub_spooled": outcome.hub_spooled,
                });
                println!("{out}");
            } else {
                println!(
                    "ok worker {} branch {} exited ({}, code {:?})",
                    outcome.name,
                    outcome.branch,
                    outcome.classification.reason.as_str(),
                    outcome.exit_code
                );
            }
            Ok(exit::code(exit::OK))
        }
        Err(err) => {
            let c = err.exit_code();
            if json_out {
                let out = json!({
                    "ok": false,
                    "code": c,
                    "reason": err.to_string(),
                });
                println!("{out}");
            } else {
                eprintln!("sommelier: {err}");
            }
            Ok(exit::code(c))
        }
    }
}

async fn cmd_watch(args: &[String]) -> Result<ExitCode, String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut url: Option<String> = None;
    let mut token: Option<String> = None;
    let mut token_file: Option<String> = None;
    let mut interval_sec: u64 = 45;
    let mut once = false;
    let mut json_out = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--once" => once = true,
            "--json" => json_out = true,
            other => {
                let Some(stripped) = other.strip_prefix("--") else {
                    eprintln!("sommelier: unknown argument `{other}` for `agent watch`");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                let (flag, val) = match stripped.split_once('=') {
                    Some((k, v)) => (k, v.to_owned()),
                    None => {
                        let Some(v) = it.next() else {
                            eprintln!("sommelier: --{stripped} needs a value");
                            return Ok(exit::code(exit::WRONG_USAGE));
                        };
                        (stripped, v.clone())
                    }
                };
                match flag {
                    "data" => data_dir = Some(PathBuf::from(val)),
                    "url" => url = Some(val),
                    "token" => token = Some(val),
                    "token-file" => token_file = Some(val),
                    "interval-sec" => {
                        interval_sec = val
                            .parse()
                            .map_err(|_| "--interval-sec requires an integer")?;
                    }
                    unknown => {
                        eprintln!("sommelier: unknown flag --{unknown} for `agent watch`");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    }
                }
            }
        }
    }

    let dir = data_dir.unwrap_or_else(crate::default_data_dir);
    let opts = resolve_client_opts(url, token, token_file);

    loop {
        let report = watch_children_once(&dir, opts.as_ref()).await?;
        if json_out {
            println!(
                "{}",
                json!({
                    "ok": true,
                    "code": exit::OK,
                    "alive": report.alive,
                    "gone": report.gone,
                })
            );
        } else if once || !report.gone.is_empty() {
            println!(
                "watch: {} alive, {} gone",
                report.alive.len(),
                report.gone.len()
            );
        }
        if once {
            break;
        }
        tokio::time::sleep(Duration::from_secs(interval_sec.max(5))).await;
    }

    Ok(exit::code(exit::OK))
}

fn cmd_status(args: &[String]) -> Result<ExitCode, String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut json_out = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier: --data needs a value");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                data_dir = Some(PathBuf::from(v));
            }
            other if other.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(other.trim_start_matches("--data=")));
            }
            other => {
                eprintln!("sommelier: unknown argument `{other}` for `agent status`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }
    let dir = data_dir.unwrap_or_else(crate::default_data_dir);
    let store = ChildrenStore::new(&dir);
    let statuses = store.list_statuses().map_err(|e| e.to_string())?;
    if json_out {
        let rows: Vec<_> = statuses
            .iter()
            .map(|s| {
                json!({
                    "name": s.record.name,
                    "class": s.record.class,
                    "model": s.record.model,
                    "branch": s.record.branch,
                    "task_id": s.record.task_id,
                    "pid": s.record.pid,
                    "pid_alive": s.pid_alive,
                    "state": s.record.state,
                    "started_at": s.record.started_at_sec,
                    "heartbeat_at": s.record.heartbeat_at_sec,
                    "text": s.record.text,
                })
            })
            .collect();
        println!(
            "{}",
            json!({
                "ok": true,
                "code": exit::OK,
                "children": rows,
            })
        );
    } else if statuses.is_empty() {
        println!("no active workers");
    } else {
        for s in statuses {
            let alive_word = if s.pid_alive { "alive" } else { "gone" };
            println!(
                "{} {} {} pid {} ({alive_word}) : {}",
                s.record.name, s.record.class, s.record.branch, s.record.pid, s.record.text
            );
        }
    }
    Ok(exit::code(exit::OK))
}

fn cmd_stop(args: &[String]) -> Result<ExitCode, String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut json_out = false;
    let mut target: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier: --data needs a value");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                data_dir = Some(PathBuf::from(v));
            }
            other if other.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(other.trim_start_matches("--data=")));
            }
            other if !other.starts_with('-') && target.is_none() => {
                target = Some(other.to_owned());
            }
            other => {
                eprintln!("sommelier: unknown argument `{other}` for `agent stop`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }
    let dir = data_dir.unwrap_or_else(crate::default_data_dir);
    let store = ChildrenStore::new(&dir);
    let stopped = store.stop(target.as_deref()).map_err(|e| e.to_string())?;
    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "code": exit::OK,
                "stopped": stopped,
            })
        );
    } else if stopped.is_empty() {
        println!("ok no matching workers running");
    } else {
        println!("ok stopped: {}", stopped.join(", "));
    }
    Ok(exit::code(exit::OK))
}

fn resolve_client_opts(
    url: Option<String>,
    token: Option<String>,
    token_file: Option<String>,
) -> Option<Opts> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let resolved_url = url
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
    let resolved_token = match (token.or_else(|| env("SOMMELIER_TOKEN")), token_file) {
        (Some(t), _) => Some(t),
        (None, file) => {
            let path = file
                .or_else(|| env("SOMMELIER_TOKEN_FILE"))
                .or_else(crate::key::client::default_marker_file)?;
            std::fs::read_to_string(path)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        }
    }?;
    Some(Opts {
        url: resolved_url,
        token: resolved_token,
    })
}
