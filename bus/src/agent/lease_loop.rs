//! Supervisor lease loop (`sommelier agent run`, sections 4.2, 5.4, 7.3, 9, 10.3 step 12a, 13.3, 13.4).
//!
//! Responsibilities of `sommelier agent run`:
//! 1. Reads `.pause` and `.gentle` in `runs_dir` (e.g. `$SOMMELIER_RUNS/.pause` and `.gentle`):
//!    when `.pause` exists, reports capacity 0 for game runs (`gpu_slots = 0`, `game_run: 0`,
//!    and removes `"run"` from `accepts`), and records `gentle` when `.gentle` exists.
//! 2. Sends `HELLO` with declared capacity (`classes`, `mode`, `gpu_slots`, `accepts`, `machine`, `os`, `gpu`).
//! 3. Starts a role seat when configured or granted (prompt = `guide for the role + takeover page (13.4)`).
//! 4. Pulls ready work with `task.next` (`next --wait`) and claims it atomically with `task.work`.
//!    Two supervisors never run the same task because `task.work` grants an exclusive lease on the hub.
//! 5. Creates the worktree, fetches or loads the brief, and writes the prompt file (`guide + brief + resume note`).
//! 6. Runs the class launcher hook (`hook-<vendor>.sh`) under section 5.4 environment contract.
//! 7. While the child runs: heartbeats every `heartbeat_interval`, watches `.sommelier/checkpoint.md`
//!    and forwards new versions (`task.checkpoint`).
//! 8. On exit: captures output, creates a rescue ref `wip/<task>-<attempt>` if the worktree is dirty (7.3),
//!    classifies the exit reason (5.4), and applies exit handling of 4.2 (`task.exit`):
//!    - `done` -> `check_result` passes ? `Deliver(task)` and `LandEnqueue(ref, sha)` : `Stuck(task, "result invalid")`
//!    - `stuck` -> `Stuck(task)`, `Alert(STUCK)`
//!    - `quota` -> `PausePool(l.pools, capture.resets_at)`, `Requeue(task, exclude that pool until reset)`
//!    - `context_full` -> `Requeue(task)`
//!    - `crash`, `lost`, `killed` -> `task.attempt < max_attempts` ? `Requeue(task)` : `Stuck(task)`, `Alert(TASK_FAILING)`
//! 9. Runs `worktree_sweep()` (section 4.3).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::capture::{ExitCapture, MAX_NOTE_CHARS, truncate_chars};
use super::checkpoint::CheckpointWatcher;
use super::children::{ChildRecord, ChildrenStore, kill_pid};
use super::classify::{ClassifyInput, ExitReason, classify_exit};
use super::disk::{DiskGuardConfig, check_disk};
use super::exec::{build_exit_fact_request, build_status_request};
use super::launch::{
    HookEnv, assemble_role_seat_prompt, assemble_worker_prompt, guide_vendor, prepare_worktree,
    resolve_hook_script, spawn_hook_with, vendor_for_class,
};
use super::outbox::Outbox;
use super::rescue::{make_rescue_ref, worktree_head_sha};
use super::sweep::{SweepContext, SweepReport, sweep_and_report};
use crate::brief;
use crate::client::{self, Opts};
use crate::derive::CapacityMode;
use crate::guide::PolicyInfo;
use crate::hooks::OpContext;
use crate::msg::{Kind, Message, Role};
use crate::quota::meter::StateEvent;
use crate::task::{
    CheckpointRow, latest_checkpoint, next_checkpoint_seq, normalize_checkpoint_note,
    query_active_lease_for_task, query_task, validate_done_result,
};
use crate::time::{now_ms, rfc3339};

/// State of `$SOMMELIER_RUNS/.pause` and `.gentle` markers (section 9 and 10.3 step 12a).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseGentleState {
    pub paused: bool,
    pub gentle: bool,
}

/// Reads `.pause` and `.gentle` in `runs_dir` (section 9: "`$SOMMELIER_RUNS/.pause` and `.gentle` keep
/// their meaning. The supervisor reads them and reports capacity 0 for game runs").
pub fn read_pause_and_gentle(runs_dir: Option<&Path>) -> PauseGentleState {
    let Some(dir) = runs_dir else {
        return PauseGentleState::default();
    };
    PauseGentleState {
        paused: dir.join(".pause").exists(),
        gentle: dir.join(".gentle").exists(),
    }
}

/// Configuration for the supervisor loop (`sommelier agent run --config FILE` or CLI flags).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SupervisorConfig {
    #[serde(default = "default_owner")]
    pub owner: String,
    #[serde(default = "default_machine")]
    pub machine: String,
    #[serde(default = "default_os")]
    pub os: String,
    #[serde(default)]
    pub gpu: String,
    #[serde(default)]
    pub mode: CapacityMode,
    #[serde(default = "default_gpu_slots")]
    pub gpu_slots: u32,
    /// Declared parallel slots per class (`class -> useful_parallel`).
    #[serde(default = "default_classes")]
    pub classes: BTreeMap<String, u32>,
    /// Accepted task kinds (`fix`, `review`, `run`, `scout`, ...).
    #[serde(default = "default_accepts")]
    pub accepts: Vec<String>,
    /// Base git repository clone from which worktrees are created.
    #[serde(default)]
    pub repo_clone: PathBuf,
    /// Directory where task worktrees are created (`<worktrees_dir>/wt-<task>`).
    #[serde(default)]
    pub worktrees_dir: PathBuf,
    /// Directory containing `hook-codex.sh`, `hook-agy.sh`, `hook-claude.sh`.
    #[serde(default)]
    pub hooks_dir: PathBuf,
    /// Optional explicit hook script override (used by tests with a fake hook).
    #[serde(default)]
    pub hook_override: Option<PathBuf>,
    /// Optional git remote to push rescue refs (`wip/<task>-<attempt>`), e.g. `"origin"`.
    #[serde(default)]
    pub fork_remote: Option<String>,
    /// Directory containing `.pause` and `.gentle` markers (e.g. `$SOMMELIER_RUNS`).
    #[serde(default)]
    pub runs_dir: Option<PathBuf>,
    /// Local sommelier data directory.
    #[serde(default = "crate::default_data_dir")]
    pub data_dir: PathBuf,
    /// Optional hub WebSocket URL and token.
    #[serde(skip)]
    pub client_opts: Option<Opts>,
    /// Seconds to wait in `next --wait`.
    #[serde(default = "default_wait_secs")]
    pub wait_secs: u64,
    /// Heartbeat and checkpoint poll interval in milliseconds.
    #[serde(default = "default_heartbeat_ms")]
    pub heartbeat_ms: u64,
    /// Optional child execution timeout in milliseconds.
    #[serde(default)]
    pub child_timeout_ms: Option<u64>,
    /// Maximum loop iterations (`None` = run until `.sommelier/stop`, `Some(n)` = run at most `n` cycles).
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// Optional role seat to start through the launcher hook (`coordinator`, `brain`, `lead`),
    /// assembling `guide + takeover page` as its prompt (section 10.3 step 12a & 13.3).
    #[serde(default)]
    pub role_seat: Option<String>,
    /// Minimum free disk space in GiB for the disk guard.
    #[serde(default = "default_min_free_gb")]
    pub min_free_gb: u64,
    /// Optional injected free disk space in GiB for deterministic tests.
    #[serde(default)]
    pub injected_free_gb: Option<u64>,
    /// Sandbox level of the hooks: `container`, `user` or `none`. Wins over `SOMMELIER_SANDBOX`. Checked
    /// when the supervisor starts, so an unknown value fails there and not at the first hook start.
    #[serde(default)]
    pub sandbox: Option<String>,
}

fn default_owner() -> String {
    std::env::var("SOMMELIER_OWNER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local".into())
}

fn default_machine() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "box1".into())
}

fn default_os() -> String {
    if cfg!(windows) {
        "windows".into()
    } else {
        "linux".into()
    }
}

fn default_gpu_slots() -> u32 {
    1
}

fn default_classes() -> BTreeMap<String, u32> {
    let mut m = BTreeMap::new();
    m.insert("codex".into(), 2);
    m.insert("gemini".into(), 2);
    m
}

fn default_accepts() -> Vec<String> {
    vec!["fix".into(), "review".into(), "run".into(), "scout".into()]
}

fn default_wait_secs() -> u64 {
    2
}

fn default_heartbeat_ms() -> u64 {
    60_000
}

fn default_min_free_gb() -> u64 {
    40
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        let data_dir = crate::default_data_dir();
        Self {
            owner: default_owner(),
            machine: default_machine(),
            os: default_os(),
            gpu: String::new(),
            mode: CapacityMode::Auto,
            gpu_slots: default_gpu_slots(),
            classes: default_classes(),
            accepts: default_accepts(),
            repo_clone: PathBuf::from("."),
            worktrees_dir: data_dir.join("worktrees"),
            hooks_dir: PathBuf::from("swarm"),
            hook_override: None,
            fork_remote: Some("origin".into()),
            runs_dir: None,
            data_dir,
            client_opts: None,
            wait_secs: default_wait_secs(),
            heartbeat_ms: default_heartbeat_ms(),
            child_timeout_ms: None,
            max_iterations: None,
            role_seat: None,
            min_free_gb: default_min_free_gb(),
            injected_free_gb: Some(100),
            sandbox: None,
        }
    }
}

impl SupervisorConfig {
    /// Checks the sandbox level of the config. `Ok(None)` when the config names none.
    pub fn sandbox_level(&self) -> Result<Option<&str>, String> {
        match self.sandbox.as_deref().map(str::trim) {
            None | Some("") => Ok(None),
            Some(v) => crate::agent::sandbox::SandboxLevel::parse(v)
                .map(|_| Some(v))
                .map_err(|_| {
                    format!("config sandbox `{v}` is unknown: use container, user or none")
                }),
        }
    }

    /// Loads a `SupervisorConfig` from a TOML or JSON file.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            serde_json::from_str(&text).map_err(|e| format!("invalid JSON config: {e}"))
        } else {
            toml::from_str(&text).map_err(|e| format!("invalid TOML config: {e}"))
        }
    }

    /// Computes effective classes, gpu_slots, and accepted task kinds after reading `.pause` and `.gentle`.
    pub fn effective_capacity(
        &self,
    ) -> (BTreeMap<String, u32>, u32, Vec<String>, PauseGentleState) {
        let pg = read_pause_and_gentle(self.runs_dir.as_deref());
        let mut classes = self.classes.clone();
        let mut accepts = self.accepts.clone();
        let mut gpu_slots = self.gpu_slots;

        if pg.paused {
            gpu_slots = 0;
            if let Some(slot) = classes.get_mut("game_run") {
                *slot = 0;
            }
            if let Some(slot) = classes.get_mut("run") {
                *slot = 0;
            }
            accepts
                .retain(|k| !matches!(k.as_str(), "run" | "game_run" | "reference_run" | "bisect"));
        }

        (classes, gpu_slots, accepts, pg)
    }

    /// Builds the `HELLO` bus request declaring machine capacity (sections 3, 5.1, 9).
    pub fn build_hello_request(&self) -> Value {
        let (classes, gpu_slots, accepts, pg) = self.effective_capacity();
        let class_str = classes
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join(",");
        let accepts_str = accepts.join(",");
        let mut refs = vec![
            format!("machine:{}", self.machine),
            format!("os:{}", self.os),
            format!("mode:{}", self.mode.as_str()),
            format!("gpu_slots:{gpu_slots}"),
        ];
        if !self.gpu.is_empty() {
            refs.push(format!("gpu:{}", self.gpu));
        }
        if !class_str.is_empty() {
            refs.push(format!("class:{class_str}"));
        }
        if !accepts_str.is_empty() {
            refs.push(format!("accepts:{accepts_str}"));
        }
        if pg.paused {
            refs.push("paused:true".into());
        }
        if pg.gentle {
            refs.push("gentle:true".into());
        }
        let body = format!(
            "machine={} os={} mode={} gpu_slots={} class={} accepts={} paused={} gentle={}",
            self.machine,
            self.os,
            self.mode.as_str(),
            gpu_slots,
            class_str,
            accepts_str,
            pg.paused,
            pg.gentle
        );
        json!({
            "op": "send",
            "kind": "HELLO",
            "topic": "presence",
            "body": body,
            "refs": refs,
        })
    }
}

/// Summary of one executed task inside `sommelier agent run`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRunRecord {
    pub task_id: String,
    pub attempt: u32,
    pub worker_name: String,
    pub class: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub prompt_file: PathBuf,
    pub reason: ExitReason,
    pub wip_ref: Option<String>,
    pub head_sha: Option<String>,
    pub checkpoints_forwarded: usize,
}

/// Summary of starting a role seat (`coordinator`, `brain`, `lead`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSeatRecord {
    pub role: String,
    pub prompt_file: PathBuf,
    pub prompt_text: String,
    pub exit_code: Option<i32>,
}

/// Overall result of `run_supervisor`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorSummary {
    pub hello_sent: bool,
    pub paused: bool,
    pub gentle: bool,
    pub gpu_slots_reported: u32,
    pub tasks_run: Vec<TaskRunRecord>,
    pub role_seat_started: Option<RoleSeatRecord>,
    pub sweep: SweepReport,
}

/// Starts a role seat through the launcher hook with `guide + takeover page` as its prompt
/// (sections 10.3 step 12a, 13.3, 13.4).
pub async fn start_role_seat(
    cfg: &SupervisorConfig,
    role_name: &str,
    class: &str,
) -> Result<RoleSeatRecord, String> {
    let policy = PolicyInfo::load(None, &cfg.data_dir)?;
    let api_state = match &cfg.client_opts {
        Some(opts) => fetch_hub_state_http(opts)
            .await
            .unwrap_or_else(|_| json!({})),
        None => json!({}),
    };
    let vendor_name = vendor_for_class(class, class);
    let vendor = guide_vendor(vendor_name);
    let prompt_text = assemble_role_seat_prompt(role_name, vendor, &policy, &api_state);

    let seat_dir = cfg.data_dir.join("agent").join("roles").join(role_name);
    fs::create_dir_all(&seat_dir).map_err(|e| format!("create role seat dir: {e}"))?;
    let prompt_file = seat_dir.join("prompt.md");
    fs::write(&prompt_file, &prompt_text).map_err(|e| format!("write role prompt: {e}"))?;

    let hook_script =
        resolve_hook_script(&cfg.hooks_dir, class, class, cfg.hook_override.as_deref());
    let mut exit_code = None;
    if hook_script.is_file() {
        let env = HookEnv {
            task_id: format!("role-{role_name}"),
            attempt: 1,
            branch: format!("role/{role_name}"),
            worktree: seat_dir.clone(),
            prompt_file: prompt_file.clone(),
            model: class.to_owned(),
            effort: "medium".into(),
            out_dir: seat_dir.clone(),
        };
        let level = crate::agent::launch::level_in_force(cfg.sandbox_level()?)?;
        let mut child = spawn_hook_with(&hook_script, &env, cfg.sandbox_level()?)?;
        let status = child.wait().map_err(|e| format!("wait role seat: {e}"))?;
        exit_code = status.code();
        if level == crate::agent::sandbox::SandboxLevel::Container {
            crate::agent::launch::fetch_container_result(&env)?;
        }
    }

    Ok(RoleSeatRecord {
        role: role_name.to_owned(),
        prompt_file,
        prompt_text,
        exit_code,
    })
}

/// Runs the supervisor loop (`sommelier agent run`):
/// 1. Reads `.pause` and `.gentle`, sends `HELLO` with capacity.
/// 2. If `role_seat` is configured, starts the role seat with `guide + takeover page`.
/// 3. Loops up to `max_iterations` (or until `.sommelier/stop` exists):
///    - Re-reads `.pause` and `.gentle` and updates `HELLO` if changed.
///    - Polls `task.next` (up to `wait_secs`), claims the task with `task.work`.
///    - If another supervisor claimed the task first, `task.work` refuses and this supervisor moves on.
///    - Prepares the worktree, builds `prompt.md` (`guide + brief + resume note`), and launches the hook.
///    - Heartbeats and forwards `.sommelier/checkpoint.md` changes while the child runs.
///    - At exit: captures output, creates a rescue ref `wip/<task>-<attempt>` if dirty, classifies the exit,
///      and sends `task.exit` + exit `FACT` + final `STATUS`.
///    - Runs `worktree_sweep`.
pub async fn run_supervisor(cfg: &SupervisorConfig) -> Result<SupervisorSummary, String> {
    cfg.sandbox_level()?;
    let mut disk_cfg = DiskGuardConfig::default().with_min_free_gb(cfg.min_free_gb);
    if let Some(gb) = cfg.injected_free_gb {
        disk_cfg = disk_cfg.with_injected_free_gb(gb);
    }
    check_disk(&cfg.data_dir, &disk_cfg).map_err(|e| e.to_string())?;

    let outbox = Outbox::new(&cfg.data_dir);
    let children_store = ChildrenStore::new(&cfg.data_dir);
    let policy = PolicyInfo::load(None, &cfg.data_dir)?;

    let mut summary = SupervisorSummary::default();
    let (_, gpu_slots, _, pg) = cfg.effective_capacity();
    summary.paused = pg.paused;
    summary.gentle = pg.gentle;
    summary.gpu_slots_reported = gpu_slots;

    // 1. Send HELLO with capacity
    let hello_req = cfg.build_hello_request();
    if let Some(opts) = &cfg.client_opts {
        let _ = outbox.send_or_spool(opts, hello_req).await?;
        summary.hello_sent = true;
    } else {
        let _ = outbox.enqueue(hello_req);
    }

    // 2. Start role seat if requested
    if let Some(role_name) = &cfg.role_seat {
        let primary_class = cfg
            .classes
            .keys()
            .next()
            .map(String::as_str)
            .unwrap_or("claude");
        let seat = start_role_seat(cfg, role_name, primary_class).await?;
        summary.role_seat_started = Some(seat);
    }

    let Some(opts) = cfg.client_opts.clone() else {
        // Offline single pass: run worktree sweep and return
        let sweep_ctx = SweepContext::from_state_and_children(
            &cfg.worktrees_dir,
            Some(cfg.repo_clone.clone()),
            cfg.fork_remote.clone(),
            &json!({}),
            &children_store,
        );
        summary.sweep = sweep_and_report(&sweep_ctx, now_ms() / 1000, None, &outbox).await?;
        return Ok(summary);
    };

    let mut last_pg = pg;
    let mut iteration = 0usize;
    let stop_file = cfg.data_dir.join(".sommelier").join("stop");

    loop {
        if stop_file.exists() {
            break;
        }
        if let Some(max_it) = cfg.max_iterations
            && iteration >= max_it
        {
            break;
        }
        iteration += 1;

        // Re-check `.pause` and `.gentle` on every cycle
        let (_, cur_gpu_slots, _, cur_pg) = cfg.effective_capacity();
        summary.paused = cur_pg.paused;
        summary.gentle = cur_pg.gentle;
        summary.gpu_slots_reported = cur_gpu_slots;
        if cur_pg != last_pg {
            last_pg = cur_pg;
            let _ = outbox.send_or_spool(&opts, cfg.build_hello_request()).await;
        }

        // Pull next ready task (with wait_secs)
        let Some(task_val) = pull_and_claim_next_task(cfg, &opts, cur_pg.paused).await? else {
            break;
        };

        let record =
            execute_leased_task(cfg, &opts, &policy, &outbox, &children_store, &task_val).await?;
        summary.tasks_run.push(record);
    }

    // Run worktree sweep at the end of the supervisor pass
    let api_state = fetch_hub_state_http(&opts)
        .await
        .unwrap_or_else(|_| json!({}));
    let sweep_ctx = SweepContext::from_state_and_children(
        &cfg.worktrees_dir,
        Some(cfg.repo_clone.clone()),
        cfg.fork_remote.clone(),
        &api_state,
        &children_store,
    );
    summary.sweep = sweep_and_report(&sweep_ctx, now_ms() / 1000, Some(&opts), &outbox).await?;

    Ok(summary)
}

/// Polls `task.next` until a task is found (or `wait_secs` elapses) and atomically claims it
/// with `task.work`. If another supervisor races and claims the task first, `task.work` is refused
/// and we immediately retry `task.next` so two supervisors never run the same task.
async fn pull_and_claim_next_task(
    cfg: &SupervisorConfig,
    opts: &Opts,
    paused: bool,
) -> Result<Option<Value>, String> {
    let deadline = Instant::now() + Duration::from_secs(cfg.wait_secs);
    let (eff_classes, _, eff_accepts, _) = cfg.effective_capacity();
    let active_classes: Vec<String> = eff_classes
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(k, _)| k.clone())
        .collect();
    if active_classes.is_empty() {
        return Ok(None);
    }

    loop {
        let mut claimed: Option<Value> = None;
        for cls in &active_classes {
            let next_req = json!({
                "op": "task.next",
                "class": cls,
                "peek": false,
            });
            let Ok(reply) = client::request_ok(opts, next_req).await else {
                continue;
            };
            if reply["found"].as_bool() != Some(true) {
                continue;
            }
            let task = &reply["task"];
            let task_id = task["id"].as_str().unwrap_or("");
            if task_id.is_empty() {
                continue;
            }
            let task_kind = task["kind"].as_str().unwrap_or("fix");
            if paused && matches!(task_kind, "run" | "game_run" | "reference_run" | "bisect") {
                continue;
            }
            if !eff_accepts.is_empty()
                && !eff_accepts
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(task_kind))
            {
                continue;
            }

            // Check pool exclusions on the task (`exclude_pools_until`)
            let now_sec = now_ms() / 1000;
            let pools = default_pools_for_class(cls);
            if is_pool_excluded(&task["exclude_pools_until"], &pools, now_sec) {
                continue;
            }

            let worker_name = format!("{}-{}-{}", cfg.owner, cls, task_id);
            let work_req = json!({
                "op": "task.work",
                "task": task_id,
                "worker": worker_name,
                "machine": cfg.machine,
                "class": cls,
                "pools": pools,
                "supervised": true,
            });
            match client::request_ok(opts, work_req).await {
                Ok(mut work_reply) => {
                    if let Some(obj) = work_reply.as_object_mut() {
                        obj.insert("chosen_class".into(), json!(cls));
                        obj.insert("worker_name".into(), json!(worker_name));
                    }
                    claimed = Some(work_reply);
                    break;
                }
                Err(_) => {
                    // Another supervisor claimed this task first; retry immediately.
                    continue;
                }
            }
        }

        if let Some(c) = claimed {
            return Ok(Some(c));
        }

        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn is_pool_excluded(exclude_val: &Value, pools: &[String], now_sec: u64) -> bool {
    let Some(obj) = exclude_val.as_object() else {
        return false;
    };
    for p in pools {
        if let Some(until) = obj.get(p).and_then(Value::as_u64)
            && until > now_sec
        {
            return true;
        }
    }
    false
}

fn default_pools_for_class(cls: &str) -> Vec<String> {
    let c = cls.to_ascii_lowercase();
    if c.starts_with("codex") {
        vec!["codex.5h".into(), "codex.week".into()]
    } else if c.starts_with("gemini") || c.starts_with("agy") {
        vec!["agy.window".into(), "agy.week".into(), "agy.day".into()]
    } else if c.starts_with("claude") {
        vec!["claude.5h".into(), "claude.week".into()]
    } else {
        vec![format!("{c}.pool")]
    }
}

#[allow(clippy::too_many_lines)]
async fn execute_leased_task(
    cfg: &SupervisorConfig,
    opts: &Opts,
    policy: &PolicyInfo,
    outbox: &Outbox,
    children_store: &ChildrenStore,
    work_reply: &Value,
) -> Result<TaskRunRecord, String> {
    let task = &work_reply["task"];
    let task_id = task["id"].as_str().unwrap_or("task").to_owned();
    let attempt = task["attempt"].as_u64().unwrap_or(1) as u32;
    let class = work_reply["chosen_class"]
        .as_str()
        .or_else(|| work_reply["lease"]["class"].as_str())
        .unwrap_or("codex")
        .to_owned();
    let worker_name = work_reply["worker_name"]
        .as_str()
        .unwrap_or(&task_id)
        .to_owned();
    let branch = task["branch"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{class}/{task_id}"));
    let resume_note = task["resume_note"].as_str().unwrap_or("");
    let resume_sha = task["resume_sha"].as_str().unwrap_or("");
    let wip_ref_in = task["wip_ref"].as_str().unwrap_or("");
    let brief_sha = task["brief_sha256"].as_str().unwrap_or("");

    // 1. Prepare worktree
    let worktree_path = cfg.worktrees_dir.join(format!("wt-{task_id}"));
    let base_sha = task["base_sha"].as_str().filter(|s| !s.is_empty());
    prepare_worktree(&cfg.repo_clone, &worktree_path, &branch, base_sha)?;
    let _ = fs::write(worktree_path.join(".sommelier").join("task_id"), &task_id);

    // 2. Fetch or read brief
    let brief_text = fetch_task_brief(opts, &cfg.data_dir, &task_id, brief_sha)
        .await?
        .unwrap_or_else(|| {
            format!(
                "TASK {task_id} BRANCH {branch}\nTASK: {}\nSCOPE: {}\nDONE WHEN: {}\n",
                task["title"].as_str().unwrap_or(&task_id),
                task["scope"].as_str().unwrap_or("*"),
                task["done_when"].as_str().unwrap_or("tests pass")
            )
        });
    let _ = fs::write(
        worktree_path.join(".sommelier").join("brief.md"),
        &brief_text,
    );

    // 3. Build prompt from guide + brief + resume note
    let v_str = vendor_for_class(&class, &class);
    let vendor = guide_vendor(v_str);
    let prompt_text = assemble_worker_prompt(
        policy,
        vendor,
        &brief_text,
        attempt,
        resume_note,
        resume_sha,
        wip_ref_in,
    );
    let prompt_file = worktree_path.join(".sommelier").join("prompt.md");
    fs::write(&prompt_file, &prompt_text)
        .map_err(|e| format!("write prompt {}: {e}", prompt_file.display()))?;

    // 4. Resolve hook and output folder
    let out_dir = cfg
        .data_dir
        .join("agent")
        .join("workers")
        .join(format!("{task_id}-{attempt}"));
    let hook_script =
        resolve_hook_script(&cfg.hooks_dir, &class, &class, cfg.hook_override.as_deref());
    let hook_env = HookEnv {
        task_id: task_id.clone(),
        attempt,
        branch: branch.clone(),
        worktree: worktree_path.clone(),
        prompt_file: prompt_file.clone(),
        model: class.clone(),
        effort: "medium".into(),
        out_dir: out_dir.clone(),
    };

    // 5. Spawn child process via launcher hook
    let _ = children_store.remove(&worker_name);
    let level = crate::agent::launch::level_in_force(cfg.sandbox_level()?)?;
    let in_container = level == crate::agent::sandbox::SandboxLevel::Container;
    let mut child = spawn_hook_with(&hook_script, &hook_env, cfg.sandbox_level()?)?;
    let pid = child.id();
    let start_sec = now_ms() / 1000;

    let mut child_record = ChildRecord {
        name: worker_name.clone(),
        class: class.clone(),
        model: class.clone(),
        branch: branch.clone(),
        task_id: Some(task_id.clone()),
        pid,
        started_at_sec: start_sec,
        heartbeat_at_sec: start_sec,
        out_dir: Some(out_dir.clone()),
        worktree: Some(worktree_path.clone()),
        state: "running".into(),
        text: format!("working on {task_id} ({branch})"),
    };
    let _ = children_store.save(&child_record);

    let running_status =
        build_status_request(&worker_name, "running", &class, &branch, &child_record.text);
    let _ = outbox.send_or_spool(opts, running_status).await;

    // 6. Monitor loop: heartbeat, checkpoint forward, stop marker, timeout
    // In a container the worker writes its checkpoint into its own clone, not into the worktree.
    let watched = if in_container {
        crate::agent::launch::container_clone_dir(&hook_env)
    } else {
        worktree_path.clone()
    };
    let mut cp_watcher = CheckpointWatcher::new(&task_id, &branch, &watched);
    let mut checkpoints_forwarded = 0usize;
    let hb_every = Duration::from_millis(cfg.heartbeat_ms.max(25));
    let timeout = cfg.child_timeout_ms.map(Duration::from_millis);
    let start_instant = Instant::now();
    let mut last_hb = Instant::now();
    let poll_interval = Duration::from_millis(25);

    let mut killed_by_supervisor = false;
    let exit_code: Option<i32>;

    loop {
        if let Ok(Some(_)) = cp_watcher.poll_and_forward(Some(opts), outbox).await {
            checkpoints_forwarded += 1;
        }

        if let Ok(Some(status)) = child.try_wait() {
            exit_code = status.code();
            break;
        }

        if children_store.is_marked_stopped(&worker_name) {
            killed_by_supervisor = true;
            kill_process_tree(pid);
            let _ = child.kill();
            exit_code = child.wait().ok().and_then(|st| st.code());
            break;
        }

        if let Some(limit) = timeout
            && start_instant.elapsed() >= limit
        {
            killed_by_supervisor = true;
            kill_process_tree(pid);
            let _ = child.kill();
            exit_code = child.wait().ok().and_then(|st| st.code());
            break;
        }

        if last_hb.elapsed() >= hb_every {
            last_hb = Instant::now();
            child_record.heartbeat_at_sec = now_ms() / 1000;
            let _ = children_store.save(&child_record);
            let _ = client::request_ok(opts, json!({"op": "heartbeat"})).await;
            let _ =
                client::request_ok(opts, json!({"op": "task.heartbeat", "task": task_id})).await;
        }

        tokio::time::sleep(poll_interval).await;
    }

    // Poll checkpoint one final time after child exits
    if let Ok(Some(_)) = cp_watcher.poll_and_forward(Some(opts), outbox).await {
        checkpoints_forwarded += 1;
    }

    if children_store.is_marked_stopped(&worker_name) {
        killed_by_supervisor = true;
    }

    // A container run works in its own clone: take its commit back into the worktree of the task.
    if in_container && let Err(e) = crate::agent::launch::fetch_container_result(&hook_env) {
        eprintln!("sommelier agent: {task_id}: the result of the container run was not taken: {e}");
    }

    // 7. Capture output and classify exit
    let end_sec = now_ms() / 1000;
    let mut capture =
        ExitCapture::collect(&out_dir, Some(&worker_name), Some(&worktree_path), &branch);
    if capture.head_sha.is_none() {
        capture.head_sha = worktree_head_sha(&worktree_path);
    }

    let classification = classify_exit(&ClassifyInput {
        capture: &capture,
        branch: &branch,
        task: &task_id,
        exit_code,
        killed_by_supervisor,
        supervisor_restarted: false,
        now_sec: end_sec,
        custom_limit_patterns: &[],
    });

    // 8. Section 7.3: at death (non-done exit), if the worktree is dirty, make a rescue ref
    // `wip/<task>-<attempt>` and push to fork remote.
    if classification.reason != ExitReason::Done
        && let Ok(rescue) = make_rescue_ref(
            &worktree_path,
            &task_id,
            attempt,
            cfg.fork_remote.as_deref(),
        )
        && rescue.dirty
    {
        capture.wip_ref = rescue.wip_ref;
        if capture.head_sha.is_none() {
            capture.head_sha = rescue.sha;
        }
    }

    // 9. Report exit fact, task.exit transition (4.2), and final status to hub
    let fact_req = build_exit_fact_request(
        &worker_name,
        &class,
        &class,
        &branch,
        Some(&task_id),
        exit_code,
        &classification,
        &capture,
        end_sec,
    );
    let _ = outbox.send_or_spool(opts, fact_req).await;

    let exit_op_req = json!({
        "op": "task.exit",
        "task": task_id,
        "worker": worker_name,
        "class": class,
        "branch": branch,
        "reason": classification.reason.as_str(),
        "vendor": classification.vendor,
        "resets_at": classification.resets_at,
        "reset_secs": classification.reset_secs,
        "detail": classification.detail,
        "last_message": capture.last_message,
        "last_tail": capture.last_tail,
        "checkpoint": capture.checkpoint,
        "head_sha": capture.head_sha,
        "wip_ref": capture.wip_ref,
        "resume_note": capture.resume_note(),
    });
    let _ = outbox.send_or_spool(opts, exit_op_req).await;

    let headline = capture.status_headline(exit_code, classification.reason.as_str());
    let final_status = build_status_request(&worker_name, "idle", &class, &branch, &headline);
    let _ = outbox.send_or_spool(opts, final_status).await;

    let _ = children_store.remove(&worker_name);

    Ok(TaskRunRecord {
        task_id,
        attempt,
        worker_name,
        class,
        branch,
        worktree: worktree_path,
        prompt_file,
        reason: classification.reason,
        wip_ref: capture.wip_ref,
        head_sha: capture.head_sha,
        checkpoints_forwarded,
    })
}

fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .output();
    }
    let _ = kill_pid(pid);
}

/// Fetches and verifies the task brief for `task_id` against `brief_sha`.
///
/// When `brief_sha` is non-empty, rejects any cached or remote brief whose SHA-256 digest does not
/// match `brief_sha` and never falls back to unchecked task text.
pub async fn fetch_task_brief(
    opts: &Opts,
    data_dir: &Path,
    task_id: &str,
    brief_sha: &str,
) -> Result<Option<String>, String> {
    let exp = (!brief_sha.is_empty()).then_some(brief_sha);
    let mut mismatch_err: Option<String> = None;

    if let Ok(Some((text, actual))) = brief::get_brief(data_dir, task_id) {
        if exp.is_none_or(|e| e.eq_ignore_ascii_case(&actual)) {
            return Ok(Some(text));
        }
        mismatch_err = Some(format!(
            "cached brief sha256 mismatch for `{task_id}`: expected {brief_sha}, got {actual}"
        ));
    }

    if !brief_sha.is_empty()
        && let Ok(Some((text, actual))) = brief::get_brief(data_dir, brief_sha)
    {
        if actual.eq_ignore_ascii_case(brief_sha) {
            return Ok(Some(text));
        }
        mismatch_err = Some(format!(
            "cached brief sha256 mismatch for `{brief_sha}`: expected {brief_sha}, got {actual}"
        ));
    }

    if !opts.url.trim().is_empty() {
        match brief::get_brief_http(opts, task_id, exp).await {
            Ok((text, _)) => return Ok(Some(text)),
            Err(e) if e.contains("mismatch") => mismatch_err = Some(e),
            Err(_) => {}
        }
        if !brief_sha.is_empty() {
            match brief::get_brief_http(opts, brief_sha, exp).await {
                Ok((text, _)) => return Ok(Some(text)),
                Err(e) if e.contains("mismatch") => mismatch_err = Some(e),
                Err(_) => {}
            }
        }
    }

    if let Some(err) = mismatch_err {
        return Err(err);
    }
    if !brief_sha.is_empty() {
        return Err(format!(
            "verified brief `{task_id}` ({brief_sha}) not found"
        ));
    }
    Ok(None)
}

async fn fetch_hub_state_http(opts: &Opts) -> Result<Value, String> {
    let mut url = reqwest::Url::parse(opts.url.trim()).map_err(|_| "invalid hub URL".to_owned())?;
    let scheme = match url.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return Err("invalid hub URL scheme".into()),
    };
    url.set_scheme(scheme)
        .map_err(|_| "invalid hub URL scheme".to_owned())?;
    url.set_path("/api/state");
    url.set_query(None);
    url.set_fragment(None);
    crate::client::init();
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .bearer_auth(opts.token.trim())
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

/// Hub-side handler for `task.exit` / `agent.exit` (section 4.2 `on_exit(l, reason, capture)`):
///
/// ```text
/// on_exit(l, reason, capture)                    # reported by the supervisor when the child process ends
///   done         -> check_result passes ? Deliver(task) and LandEnqueue(ref, sha) : Stuck(task, "result invalid")
///   stuck        -> Stuck(task), Alert(STUCK)
///   quota        -> PausePool(l.pools, capture.resets_at), Requeue(task, exclude that pool until reset)
///   context_full -> Requeue(task)                # the agent called handover, the note is fresh
///   crash, lost, killed -> task.attempt < max_attempts ? Requeue(task) : Stuck(task), Alert(TASK_FAILING)
///
/// Requeue(t)
///   t.attempt += 1
///   t.resume_note = newest CHECKPOINT of t + capture (tail of the last message, branch, head sha, wip ref)
///   t.state = ready                              # same branch, resume boost applies, ready_at unchanged
/// ```
#[allow(clippy::too_many_lines)]
pub fn handle_task_exit_op(
    ctx: &mut OpContext<'_>,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    let now_sec = ctx.now_ms / 1000;
    let ts = rfc3339(ctx.now_ms);
    let mut out = Map::new();

    let task_id = body["task"]
        .as_str()
        .or_else(|| body["id"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "task.exit requires `task`".to_owned())?
        .to_owned();

    let task = query_task(ctx.store.connection(), &task_id)?
        .ok_or_else(|| format!("task `{task_id}` not found"))?;
    let active_lease = query_active_lease_for_task(ctx.store.connection(), &task_id)?;

    if let Some(l) = &active_lease
        && l.owner != ctx.caller.name
        && !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub)
    {
        return Err(format!(
            "task `{task_id}` is leased to {}, not {}",
            l.owner, ctx.caller.name
        ));
    }

    let reason_str = body["reason"].as_str().unwrap_or("crash").trim();
    let branch = body["branch"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&task.branch)
        .to_owned();
    let head_sha = body["head_sha"]
        .as_str()
        .or_else(|| body["sha"].as_str())
        .unwrap_or("")
        .trim()
        .to_owned();
    let wip_ref = body["wip_ref"].as_str().unwrap_or("").trim().to_owned();
    let last_message = body["last_message"].as_str().unwrap_or("");
    let last_tail = body["last_tail"].as_str().unwrap_or("");
    let checkpoint_in = body["checkpoint"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let detail = body["detail"].as_str().unwrap_or("").trim();

    let mut msgs: Vec<Message> = Vec::new();

    // If a checkpoint was captured at exit and is newer than what's in the store, record it.
    let newest_cp_in_db = latest_checkpoint(ctx.store.connection(), &task_id, Some(task.attempt))?
        .or(latest_checkpoint(ctx.store.connection(), &task_id, None)?);
    let mut effective_cp: Option<CheckpointRow> = newest_cp_in_db.clone();
    if let Some(cp_text) = checkpoint_in {
        let already_same = newest_cp_in_db
            .as_ref()
            .is_some_and(|c| c.note.trim() == cp_text);
        if !already_same {
            let seq = next_checkpoint_seq(ctx.store.connection(), &task_id, task.attempt)?;
            let note = normalize_checkpoint_note(&truncate_chars(cp_text, MAX_NOTE_CHARS))?;
            effective_cp = Some(CheckpointRow {
                task_id: task_id.clone(),
                attempt: task.attempt,
                seq,
                at: now_sec,
                branch: branch.clone(),
                sha: head_sha.clone(),
                pushed: false,
                note,
                source: "supervisor".into(),
                msg_id: 0,
            });
        }
    }

    // Helper closure to build combined resume note per section 4.2:
    // newest CHECKPOINT of t + capture (tail of the last message, branch, head sha, wip ref)
    let build_requeue_note = |cp: &Option<CheckpointRow>| -> String {
        let mut parts = Vec::new();
        if !branch.is_empty() {
            let mut h = format!("branch: {branch}");
            if !head_sha.is_empty() {
                h.push_str(&format!(" sha: {head_sha}"));
            }
            if !wip_ref.is_empty() {
                h.push_str(&format!(" wip: {wip_ref}"));
            }
            parts.push(h);
        } else if !wip_ref.is_empty() {
            parts.push(format!("wip: {wip_ref}"));
        }
        if let Some(c) = cp {
            let cp_line = c
                .note
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if !cp_line.is_empty() {
                parts.push(format!("checkpoint: {cp_line}"));
            }
        }
        let tail_line = last_tail
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !tail_line.is_empty() {
            parts.push(format!("last: {tail_line}"));
        }
        if parts.is_empty() {
            if let Some(s) = body["resume_note"].as_str().filter(|s| !s.is_empty()) {
                return truncate_chars(s, MAX_NOTE_CHARS);
            }
            return format!("requeued after {reason_str} (attempt {})", task.attempt);
        }
        truncate_chars(&parts.join(" | "), MAX_NOTE_CHARS)
    };

    let release_lease = |reason: &str| -> Option<Value> {
        active_lease.as_ref().map(|l| {
            let mut released = l.clone();
            released.state = "released".into();
            released.end_reason = reason.to_owned();
            json!(released)
        })
    };

    match reason_str {
        "done" => {
            let check_sha = if !head_sha.is_empty() {
                head_sha.clone()
            } else {
                super::capture::extract_sha_from_first_line(last_message)
                    .unwrap_or_else(|| "0000000".into())
            };
            match validate_done_result(last_message, &branch, &task_id, None, task.prior_art) {
                Ok(()) => {
                    let mut update = json!({
                        "v": 1,
                        "task": task_id,
                        "prev": task.state,
                        "state": "delivered",
                        "attempt": task.attempt,
                        "by": ctx.caller.name,
                        "reason": "done",
                        "result_ref": branch,
                        "result_sha": check_sha,
                    });
                    if let Some(l) = release_lease("done") {
                        update.as_object_mut().unwrap().insert("lease".into(), l);
                    }
                    if let Some(cp) = &effective_cp {
                        update
                            .as_object_mut()
                            .unwrap()
                            .insert("checkpoint".into(), json!(cp));
                    }
                    msgs.push(Message {
                        id: 0,
                        ts: ts.clone(),
                        from: "hub".into(),
                        role: Role::Hub,
                        kind: Kind::TaskUpdate,
                        topic: format!("task/{task_id}"),
                        body: update.to_string(),
                        refs: vec!["state:delivered".into(), branch.clone(), check_sha.clone()],
                        to: Vec::new(),
                    });
                    // LandEnqueue(ref, sha) as a FACT on `land/<ref>` (section 4.2 & 5.2)
                    msgs.push(Message {
                        id: 0,
                        ts,
                        from: "hub".into(),
                        role: Role::Hub,
                        kind: Kind::Fact,
                        topic: format!("land/{branch}"),
                        body: json!({
                            "v": 1,
                            "ref": branch,
                            "sha": check_sha,
                            "task_id": task_id,
                            "state": "queued",
                            "enqueued_at": now_sec,
                        })
                        .to_string(),
                        refs: vec!["state:queued".into(), check_sha],
                        to: Vec::new(),
                    });
                    out.insert("action".into(), json!("delivered"));
                    out.insert("state".into(), json!("delivered"));
                }
                Err(why) => {
                    let why_msg = format!("result invalid: {why}");
                    let mut update = json!({
                        "v": 1,
                        "task": task_id,
                        "prev": task.state,
                        "state": "stuck",
                        "attempt": task.attempt,
                        "by": ctx.caller.name,
                        "reason": why_msg,
                    });
                    if let Some(l) = release_lease("stuck") {
                        update.as_object_mut().unwrap().insert("lease".into(), l);
                    }
                    msgs.push(Message {
                        id: 0,
                        ts: ts.clone(),
                        from: "hub".into(),
                        role: Role::Hub,
                        kind: Kind::TaskUpdate,
                        topic: format!("task/{task_id}"),
                        body: update.to_string(),
                        refs: vec!["state:stuck".into()],
                        to: Vec::new(),
                    });
                    msgs.push(build_alert_msg(
                        "STUCK",
                        &task_id,
                        "warn",
                        &format!("task {task_id} stuck: {why_msg}"),
                        json!({"task_id": task_id, "why": why_msg}),
                        now_sec,
                        &ts,
                    ));
                    out.insert("action".into(), json!("stuck"));
                    out.insert("state".into(), json!("stuck"));
                }
            }
        }
        "stuck" => {
            let why_msg = if !detail.is_empty() {
                detail.to_owned()
            } else if !last_tail.is_empty() {
                truncate_chars(last_tail, 240)
            } else {
                "worker reported STUCK".into()
            };
            let mut update = json!({
                "v": 1,
                "task": task_id,
                "prev": task.state,
                "state": "stuck",
                "attempt": task.attempt,
                "by": ctx.caller.name,
                "reason": why_msg,
            });
            if !wip_ref.is_empty() {
                update
                    .as_object_mut()
                    .unwrap()
                    .insert("wip_ref".into(), json!(wip_ref));
            }
            if let Some(l) = release_lease("stuck") {
                update.as_object_mut().unwrap().insert("lease".into(), l);
            }
            if let Some(cp) = &effective_cp {
                update
                    .as_object_mut()
                    .unwrap()
                    .insert("checkpoint".into(), json!(cp));
            }
            msgs.push(Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{task_id}"),
                body: update.to_string(),
                refs: vec!["state:stuck".into()],
                to: Vec::new(),
            });
            msgs.push(build_alert_msg(
                "STUCK",
                &task_id,
                "warn",
                &format!("task {task_id} stuck: {why_msg}"),
                json!({"task_id": task_id, "why": why_msg}),
                now_sec,
                &ts,
            ));
            out.insert("action".into(), json!("stuck"));
            out.insert("state".into(), json!("stuck"));
        }
        "quota" => {
            // Section 4.2: quota -> PausePool(l.pools, capture.resets_at), Requeue(task, exclude that pool until reset)
            let resets_at = body["resets_at"]
                .as_u64()
                .filter(|&r| r > now_sec)
                .unwrap_or(now_sec + 3600);
            let cls = body["class"]
                .as_str()
                .or_else(|| active_lease.as_ref().map(|l| l.class.as_str()))
                .unwrap_or("gemini");
            let vendor = body["vendor"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| vendor_for_class(cls, cls));

            let mut pools: Vec<String> = active_lease
                .as_ref()
                .map(|l| l.pools.clone())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| default_pools_for_class(cls));
            if pools.is_empty() {
                pools.push(format!("{vendor}.window"));
            }

            let owner = active_lease
                .as_ref()
                .map(|l| l.owner.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&ctx.caller.name);

            // 1. PausePool(l.pools, capture.resets_at) -> emit FACT quota/<owner>/<pool>/<window> with state_event
            for pool in &pools {
                let window = pool.rsplit_once('.').map_or("5h", |(_, w)| w).to_owned();
                let se = StateEvent {
                    resets_at,
                    observed_at: now_sec,
                    message: if detail.is_empty() {
                        "quota limit reached".into()
                    } else {
                        detail.to_owned()
                    },
                    family: None,
                };
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: owner.to_owned(),
                    role: ctx.caller.role,
                    kind: Kind::Fact,
                    topic: format!("quota/{owner}/{pool}/{window}"),
                    body: json!({
                        "v": 1,
                        "owner": owner,
                        "vendor": vendor,
                        "pool": pool,
                        "window": window,
                        "resets_at": resets_at,
                        "state_event": se,
                    })
                    .to_string(),
                    refs: vec!["event:limit".into(), format!("resets_at:{resets_at}")],
                    to: Vec::new(),
                });
            }

            // 2. Requeue(task, exclude that pool until reset)
            let mut excl = task
                .exclude_pools_until
                .as_object()
                .cloned()
                .unwrap_or_default();
            for pool in &pools {
                excl.insert(pool.clone(), json!(resets_at));
            }
            let next_attempt = task.attempt.saturating_add(1);
            let note = build_requeue_note(&effective_cp);
            let resume_sha = if !head_sha.is_empty() {
                head_sha.clone()
            } else {
                effective_cp
                    .as_ref()
                    .map(|c| c.sha.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| task.resume_sha.clone())
            };
            let new_wip = if !wip_ref.is_empty() {
                wip_ref.clone()
            } else {
                task.wip_ref.clone()
            };

            let mut update = json!({
                "v": 1,
                "task": task_id,
                "prev": task.state,
                "state": "ready",
                "attempt": next_attempt,
                "by": ctx.caller.name,
                "reason": "quota",
                "resume_note": note,
                "resume_sha": resume_sha,
                "wip_ref": new_wip,
                "exclude_pools_until": Value::Object(excl),
            });
            if let Some(l) = release_lease("quota") {
                update.as_object_mut().unwrap().insert("lease".into(), l);
            }
            if let Some(cp) = &effective_cp {
                update
                    .as_object_mut()
                    .unwrap()
                    .insert("checkpoint".into(), json!(cp));
            }
            msgs.push(Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{task_id}"),
                body: update.to_string(),
                refs: vec!["state:ready".into(), "reason:quota".into()],
                to: Vec::new(),
            });

            out.insert("action".into(), json!("requeued"));
            out.insert("state".into(), json!("ready"));
            out.insert("attempt".into(), json!(next_attempt));
            out.insert("paused_pools".into(), json!(pools));
            out.insert("resets_at".into(), json!(resets_at));
        }
        "context_full" => {
            let next_attempt = task.attempt.saturating_add(1);
            let note = build_requeue_note(&effective_cp);
            let resume_sha = if !head_sha.is_empty() {
                head_sha.clone()
            } else {
                effective_cp
                    .as_ref()
                    .map(|c| c.sha.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| task.resume_sha.clone())
            };
            let new_wip = if !wip_ref.is_empty() {
                wip_ref.clone()
            } else {
                task.wip_ref.clone()
            };
            let mut update = json!({
                "v": 1,
                "task": task_id,
                "prev": task.state,
                "state": "ready",
                "attempt": next_attempt,
                "by": ctx.caller.name,
                "reason": "context_full",
                "resume_note": note,
                "resume_sha": resume_sha,
                "wip_ref": new_wip,
            });
            if let Some(l) = release_lease("context_full") {
                update.as_object_mut().unwrap().insert("lease".into(), l);
            }
            if let Some(cp) = &effective_cp {
                update
                    .as_object_mut()
                    .unwrap()
                    .insert("checkpoint".into(), json!(cp));
            }
            msgs.push(Message {
                id: 0,
                ts,
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{task_id}"),
                body: update.to_string(),
                refs: vec!["state:ready".into(), "reason:context_full".into()],
                to: Vec::new(),
            });
            out.insert("action".into(), json!("requeued"));
            out.insert("state".into(), json!("ready"));
            out.insert("attempt".into(), json!(next_attempt));
        }
        // crash, lost, killed
        other_reason => {
            let max_attempts = task.max_attempts.max(1);
            if task.attempt < max_attempts {
                let next_attempt = task.attempt.saturating_add(1);
                let note = build_requeue_note(&effective_cp);
                let resume_sha = if !head_sha.is_empty() {
                    head_sha.clone()
                } else {
                    effective_cp
                        .as_ref()
                        .map(|c| c.sha.clone())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| task.resume_sha.clone())
                };
                let new_wip = if !wip_ref.is_empty() {
                    wip_ref.clone()
                } else {
                    task.wip_ref.clone()
                };
                let mut update = json!({
                    "v": 1,
                    "task": task_id,
                    "prev": task.state,
                    "state": "ready",
                    "attempt": next_attempt,
                    "by": ctx.caller.name,
                    "reason": other_reason,
                    "resume_note": note,
                    "resume_sha": resume_sha,
                    "wip_ref": new_wip,
                });
                if let Some(l) = release_lease(other_reason) {
                    update.as_object_mut().unwrap().insert("lease".into(), l);
                }
                if let Some(cp) = &effective_cp {
                    update
                        .as_object_mut()
                        .unwrap()
                        .insert("checkpoint".into(), json!(cp));
                }
                msgs.push(Message {
                    id: 0,
                    ts,
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::TaskUpdate,
                    topic: format!("task/{task_id}"),
                    body: update.to_string(),
                    refs: vec!["state:ready".into(), format!("reason:{other_reason}")],
                    to: Vec::new(),
                });
                out.insert("action".into(), json!("requeued"));
                out.insert("state".into(), json!("ready"));
                out.insert("attempt".into(), json!(next_attempt));
            } else {
                let why_msg = format!(
                    "{other_reason} on attempt {} of {}",
                    task.attempt, max_attempts
                );
                let new_wip = if !wip_ref.is_empty() {
                    wip_ref.clone()
                } else {
                    task.wip_ref.clone()
                };
                let mut update = json!({
                    "v": 1,
                    "task": task_id,
                    "prev": task.state,
                    "state": "stuck",
                    "attempt": task.attempt,
                    "by": ctx.caller.name,
                    "reason": why_msg,
                    "wip_ref": new_wip,
                });
                if let Some(l) = release_lease(other_reason) {
                    update.as_object_mut().unwrap().insert("lease".into(), l);
                }
                if let Some(cp) = &effective_cp {
                    update
                        .as_object_mut()
                        .unwrap()
                        .insert("checkpoint".into(), json!(cp));
                }
                msgs.push(Message {
                    id: 0,
                    ts: ts.clone(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::TaskUpdate,
                    topic: format!("task/{task_id}"),
                    body: update.to_string(),
                    refs: vec!["state:stuck".into()],
                    to: Vec::new(),
                });
                msgs.push(build_alert_msg(
                    "TASK_FAILING",
                    &task_id,
                    "error",
                    &format!(
                        "task {task_id} failed after {max_attempts} attempts ({other_reason})"
                    ),
                    json!({
                        "task_id": task_id,
                        "attempt": task.attempt,
                        "max_attempts": max_attempts,
                        "reason": other_reason,
                    }),
                    now_sec,
                    &ts,
                ));
                out.insert("action".into(), json!("stuck"));
                out.insert("state".into(), json!("stuck"));
            }
        }
    }

    let ids = ctx.emit_all(msgs)?;
    out.insert("msg_ids".into(), json!(ids));
    Ok(out)
}

fn build_alert_msg(
    code: &str,
    key: &str,
    severity: &str,
    text: &str,
    data: Value,
    now_sec: u64,
    ts: &str,
) -> Message {
    Message {
        id: 0,
        ts: ts.to_owned(),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Alert,
        topic: format!("alert/{code}/{key}"),
        body: json!({
            "v": 1,
            "code": code,
            "key": key,
            "severity": severity,
            "state": "open",
            "opened_at": now_sec,
            "last_at": now_sec,
            "count": 1,
            "text": text,
            "data": data,
        })
        .to_string(),
        refs: vec!["state:open".into()],
        to: vec!["coordinator".into()],
    }
}
