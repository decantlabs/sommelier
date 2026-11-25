//! Worker execution wrapper (`sommelier agent exec`, sections 4.2, 5.1, 5.4, and step 7).
//!
//! Wraps one worker child process:
//! 1. Runs the disk guard before starting (refuses with exit code 3 when free space is low).
//! 2. Starts the child command in the foreground and records its PID in `ChildrenStore`.
//! 3. Sends the initial `STATUS` (`state:running`) from the process facts (spooled to the outbox if hub is unreachable).
//! 4. Sends a heartbeat and refreshes the status every `heartbeat_interval` (default 60 s) while the process lives.
//! 5. On exit, captures `last.txt`, `err.txt`, `events.jsonl`, and `.sommelier/checkpoint.md`, classifies the exit reason
//!    per section 5.4, sends the exit `FACT` (`exit/<owner>/<worker>`) and final `STATUS`, and removes the active child entry.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::capture::ExitCapture;
use super::children::{ChildRecord, ChildrenStore, kill_pid};
use super::classify::{Classification, ClassifyInput, classify_exit};
use super::disk::{DiskGuardConfig, DiskRefusal, check_disk};
use super::outbox::Outbox;
use crate::client::Opts;
use crate::time::now_ms;

/// Configuration for `sommelier agent exec`.
#[derive(Clone, Debug)]
pub struct ExecConfig {
    pub name: String,
    pub class: String,
    pub model: String,
    pub branch: String,
    pub task_id: Option<String>,
    pub text: String,
    pub command: Vec<String>,
    pub worktree: Option<PathBuf>,
    pub out_dir: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub client_opts: Option<Opts>,
    pub heartbeat_interval: Duration,
    pub timeout: Option<Duration>,
    pub disk_guard: DiskGuardConfig,
    pub custom_limit_patterns: Vec<String>,
}

impl ExecConfig {
    pub fn new(
        name: impl Into<String>,
        class: impl Into<String>,
        branch: impl Into<String>,
        command: Vec<String>,
        data_dir: PathBuf,
    ) -> Self {
        let class_s = class.into();
        let branch_s = branch.into();
        Self {
            name: name.into(),
            model: class_s.clone(),
            class: class_s,
            text: format!("working on {branch_s}"),
            branch: branch_s,
            task_id: std::env::var("SOMMELIER_TASK")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            command,
            worktree: std::env::var("SOMMELIER_WORKTREE")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from),
            out_dir: std::env::var("SOMMELIER_OUT")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from),
            data_dir,
            client_opts: None,
            heartbeat_interval: Duration::from_secs(60),
            timeout: None,
            disk_guard: DiskGuardConfig::default(),
            custom_limit_patterns: Vec::new(),
        }
    }
}

/// Outcome returned when `exec_worker` finishes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutcome {
    pub name: String,
    pub class: String,
    pub branch: String,
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub classification: Classification,
    pub capture: ExitCapture,
    pub hub_spooled: bool,
}

/// Errors that prevent `exec_worker` from running the child.
#[derive(Clone, Debug)]
pub enum ExecError {
    DiskLow(DiskRefusal),
    InvalidCommand(String),
    SpawnFailed(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::DiskLow(r) => write!(f, "{r}"),
            ExecError::InvalidCommand(s) => write!(f, "{s}"),
            ExecError::SpawnFailed(s) => write!(f, "{s}"),
        }
    }
}

impl ExecError {
    pub fn exit_code(&self) -> u8 {
        match self {
            ExecError::DiskLow(r) => r.code,
            ExecError::InvalidCommand(_) => crate::exit::WRONG_USAGE,
            ExecError::SpawnFailed(_) => crate::exit::INTERNAL_ERROR,
        }
    }
}

/// Executes one supervised worker child process with disk guard, status row,
/// periodic heartbeat, exit capture, classification, and outbox spooling.
pub async fn exec_worker(cfg: &ExecConfig) -> Result<ExecOutcome, ExecError> {
    if cfg.command.is_empty() || cfg.command[0].trim().is_empty() {
        return Err(ExecError::InvalidCommand(
            "agent exec requires a command after `--`".into(),
        ));
    }

    let check_path = cfg
        .worktree
        .as_deref()
        .or(cfg.out_dir.as_deref())
        .unwrap_or(&cfg.data_dir);
    if let Err(refusal) = check_disk(check_path, &cfg.disk_guard) {
        return Err(ExecError::DiskLow(refusal));
    }

    let children = ChildrenStore::new(&cfg.data_dir);
    let _ = children.remove(&cfg.name);
    let outbox = Outbox::new(&cfg.data_dir);

    let out_dir = cfg
        .out_dir
        .clone()
        .unwrap_or_else(|| cfg.data_dir.join("agent").join("out").join(&cfg.name));
    let _ = std::fs::create_dir_all(&out_dir);

    let mut cmd = Command::new(&cfg.command[0]);
    if cfg.command.len() > 1 {
        cmd.args(&cfg.command[1..]);
    }
    if let Some(wt) = &cfg.worktree
        && wt.is_dir()
    {
        cmd.current_dir(wt);
        cmd.env("SOMMELIER_WORKTREE", wt);
    }
    cmd.env("SOMMELIER_OUT", &out_dir);
    cmd.env("SOMMELIER_BRANCH", &cfg.branch);
    cmd.env("SOMMELIER_MODEL", &cfg.model);
    if let Some(task_id) = &cfg.task_id {
        cmd.env("SOMMELIER_TASK", task_id);
    }
    cmd.stdin(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| ExecError::SpawnFailed(format!("cannot spawn `{}`: {e}", cfg.command[0])))?;
    let pid = child.id();
    let start_sec = now_ms() / 1000;

    let mut record = ChildRecord {
        name: cfg.name.clone(),
        class: cfg.class.clone(),
        model: cfg.model.clone(),
        branch: cfg.branch.clone(),
        task_id: cfg.task_id.clone(),
        pid,
        started_at_sec: start_sec,
        heartbeat_at_sec: start_sec,
        out_dir: Some(out_dir.clone()),
        worktree: cfg.worktree.clone(),
        state: "running".into(),
        text: cfg.text.clone(),
    };
    let _ = children.save(&record);

    let mut hub_spooled = false;
    let running_req =
        build_status_request(&cfg.name, "running", &cfg.model, &cfg.branch, &cfg.text);
    if !send_or_queue(&outbox, cfg.client_opts.as_ref(), running_req).await {
        hub_spooled = true;
    }

    let hb_every = cfg.heartbeat_interval.max(Duration::from_millis(20));
    let mut last_hb = Instant::now();
    let start_instant = Instant::now();
    let poll_interval = Duration::from_millis(25);

    let mut killed_by_supervisor = false;
    let exit_code: Option<i32>;

    loop {
        if let Ok(Some(status)) = child.try_wait() {
            exit_code = status.code();
            break;
        }

        if children.is_marked_stopped(&cfg.name) {
            killed_by_supervisor = true;
            let _ = kill_pid(pid);
            let _ = child.kill();
            exit_code = child.wait().ok().and_then(|st| st.code());
            break;
        }

        if let Some(limit) = cfg.timeout
            && start_instant.elapsed() >= limit
        {
            killed_by_supervisor = true;
            let _ = kill_pid(pid);
            let _ = child.kill();
            exit_code = child.wait().ok().and_then(|st| st.code());
            break;
        }

        if last_hb.elapsed() >= hb_every {
            last_hb = Instant::now();
            let now_sec = now_ms() / 1000;
            record.heartbeat_at_sec = now_sec;
            let _ = children.save(&record);
            if let Some(opts) = &cfg.client_opts {
                let _ = outbox.flush(opts).await;
                let hb_req = json!({"op": "heartbeat"});
                let _ = crate::client::request_ok(opts, hb_req).await;
                let refresh_req =
                    build_status_request(&cfg.name, "running", &cfg.model, &cfg.branch, &cfg.text);
                let _ = crate::client::request_ok(opts, refresh_req).await;
            }
        }

        tokio::time::sleep(poll_interval).await;
    }

    if children.is_marked_stopped(&cfg.name) {
        killed_by_supervisor = true;
    }

    let end_sec = now_ms() / 1000;
    let capture = ExitCapture::collect(
        &out_dir,
        Some(&cfg.name),
        cfg.worktree.as_deref(),
        &cfg.branch,
    );
    let task_for_check = cfg
        .task_id
        .as_deref()
        .unwrap_or_else(|| cfg.branch.split('/').nth(1).unwrap_or(&cfg.name));
    let classification = classify_exit(&ClassifyInput {
        capture: &capture,
        branch: &cfg.branch,
        task: task_for_check,
        exit_code,
        killed_by_supervisor,
        supervisor_restarted: false,
        now_sec: end_sec,
        custom_limit_patterns: &cfg.custom_limit_patterns,
    });

    let fact_req = build_exit_fact_request(
        &cfg.name,
        &cfg.class,
        &cfg.model,
        &cfg.branch,
        cfg.task_id.as_deref(),
        exit_code,
        &classification,
        &capture,
        end_sec,
    );
    if !send_or_queue(&outbox, cfg.client_opts.as_ref(), fact_req).await {
        hub_spooled = true;
    }

    let headline = capture.status_headline(exit_code, classification.reason.as_str());
    let final_status_req =
        build_status_request(&cfg.name, "idle", &cfg.model, &cfg.branch, &headline);
    if !send_or_queue(&outbox, cfg.client_opts.as_ref(), final_status_req).await {
        hub_spooled = true;
    }

    let _ = children.remove(&cfg.name);

    Ok(ExecOutcome {
        name: cfg.name.clone(),
        class: cfg.class.clone(),
        branch: cfg.branch.clone(),
        pid,
        exit_code,
        classification,
        capture,
        hub_spooled,
    })
}

/// Builds a `STATUS` bus request for `agent/<name>`.
pub fn build_status_request(
    name: &str,
    state: &str,
    model: &str,
    branch: &str,
    text: &str,
) -> Value {
    let mut refs = vec![format!("state:{state}")];
    if !model.is_empty() {
        refs.push(format!("model:{model}"));
    }
    if !branch.is_empty() {
        refs.push(format!("branch:{branch}"));
    }
    let clean_text: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(300)
        .collect();
    json!({
        "op": "send",
        "kind": "STATUS",
        "topic": format!("agent/{name}"),
        "body": clean_text,
        "refs": refs,
    })
}

/// Builds a `FACT` bus request for `exit/<worker>` (section 5.2).
#[allow(clippy::too_many_arguments)]
pub fn build_exit_fact_request(
    name: &str,
    class: &str,
    model: &str,
    branch: &str,
    task_id: Option<&str>,
    exit_code: Option<i32>,
    classification: &Classification,
    capture: &ExitCapture,
    at_sec: u64,
) -> Value {
    let body = json!({
        "v": 1,
        "worker": name,
        "class": class,
        "model": model,
        "branch": branch,
        "task_id": task_id,
        "exit_code": exit_code,
        "reason": classification.reason.as_str(),
        "vendor": classification.vendor,
        "resets_at": classification.resets_at,
        "reset_secs": classification.reset_secs,
        "detail": classification.detail,
        "head_sha": capture.head_sha,
        "resume_note": capture.resume_note(),
        "at": at_sec,
    })
    .to_string();
    let mut refs = vec![format!("reason:{}", classification.reason.as_str())];
    if !branch.is_empty() {
        refs.push(format!("branch:{branch}"));
    }
    json!({
        "op": "send",
        "kind": "FACT",
        "topic": format!("exit/{name}"),
        "body": body,
        "refs": refs,
    })
}

async fn send_or_queue(outbox: &Outbox, opts: Option<&Opts>, req: Value) -> bool {
    match opts {
        Some(o) => matches!(outbox.send_or_spool(o, req).await, Ok(Some(_))),
        None => {
            let _ = outbox.enqueue(req);
            false
        }
    }
}
