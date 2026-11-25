//! Local worktree sweep (section 4.3, step 12a).
//!
//! Section 4.3:
//! ```text
//! worktree_sweep()                               # runs in the supervisor, local machine only
//!   for each worktree: task closed or branch landed, and clean -> remove the worktree
//!                      dirty and no live child -> make the rescue ref (section 7.3), then Alert(DIRTY_WORKTREE)
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::children::ChildrenStore;
use crate::agent::outbox::Outbox;
use crate::agent::rescue::{is_worktree_dirty, make_rescue_ref, worktree_branch};
use crate::client::Opts;

/// Action taken for a single worktree during `worktree_sweep`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepAction {
    /// Worktree belonged to a closed task or landed branch and was clean -> removed.
    Removed {
        worktree: PathBuf,
        task_id: Option<String>,
        branch: Option<String>,
    },
    /// Worktree had uncommitted changes and no live child -> rescue ref created and `DIRTY_WORKTREE` alert emitted.
    RescuedDirty {
        worktree: PathBuf,
        task_id: String,
        attempt: u32,
        wip_ref: String,
        sha: String,
    },
    /// Worktree is still active or clean for an open task -> kept.
    Kept { worktree: PathBuf },
}

/// Summary of a `worktree_sweep` pass.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepReport {
    pub actions: Vec<SweepAction>,
    pub alerts: Vec<Value>,
}

/// Input context for `worktree_sweep`.
#[derive(Clone, Debug, Default)]
pub struct SweepContext {
    /// Base repo clone where `git worktree remove` is run if available.
    pub repo_clone: Option<PathBuf>,
    /// Directory containing task worktrees (e.g. `<data_dir>/worktrees` or `$SOMMELIER_REPOS`).
    pub worktrees_dir: PathBuf,
    /// Optional git remote for pushing rescue refs (`origin`).
    pub fork_remote: Option<String>,
    /// Set of task IDs whose state is `closed` (or `delivered` / `cancelled`).
    pub closed_tasks: BTreeSet<String>,
    /// Set of branch names that have landed on `main`.
    pub landed_branches: BTreeSet<String>,
    /// Known task attempt numbers (`task_id -> attempt`).
    pub task_attempts: BTreeMap<String, u32>,
    /// Worktrees currently owned by a live child process.
    pub active_worktrees: BTreeSet<PathBuf>,
    /// Task IDs currently owned by a live child process.
    pub active_tasks: BTreeSet<String>,
}

impl SweepContext {
    /// Populates `closed_tasks`, `landed_branches`, and `task_attempts` from a `/api/state` JSON value,
    /// and `active_worktrees` / `active_tasks` from `ChildrenStore`.
    pub fn from_state_and_children(
        worktrees_dir: impl Into<PathBuf>,
        repo_clone: Option<PathBuf>,
        fork_remote: Option<String>,
        api_state: &Value,
        children_store: &ChildrenStore,
    ) -> Self {
        let mut ctx = Self {
            repo_clone,
            worktrees_dir: worktrees_dir.into(),
            fork_remote,
            ..Default::default()
        };

        if let Some(tasks) = api_state["tasks"].as_array() {
            for t in tasks {
                let id = t["id"].as_str().unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                let attempt = t["attempt"].as_u64().unwrap_or(1) as u32;
                ctx.task_attempts.insert(id.to_owned(), attempt);
                let st = t["state"].as_str().unwrap_or("");
                if st == "closed" {
                    ctx.closed_tasks.insert(id.to_owned());
                    if let Some(b) = t["branch"].as_str().filter(|s| !s.is_empty()) {
                        ctx.landed_branches.insert(b.to_owned());
                    }
                }
            }
        }

        if let Some(branches) = api_state["branches"].as_array() {
            for b in branches {
                if b["state"].as_str() == Some("landed")
                    && let Some(r) = b["ref"].as_str().filter(|s| !s.is_empty())
                {
                    ctx.landed_branches.insert(r.to_owned());
                }
            }
        }

        if let Ok(statuses) = children_store.list_statuses() {
            for st in statuses {
                if st.pid_alive {
                    if let Some(wt) = &st.record.worktree {
                        ctx.active_worktrees.insert(normalize_path(wt));
                    }
                    if let Some(tid) = &st.record.task_id {
                        ctx.active_tasks.insert(tid.clone());
                    }
                }
            }
        }

        ctx
    }
}

/// Runs the local supervisor worktree sweep of section 4.3:
/// - For each worktree under `ctx.worktrees_dir`:
///   - If a live child owns it, skip (`Kept`).
///   - If dirty and no live child: creates the rescue ref `wip/<task>-<attempt>` (section 7.3)
///     and builds an `Alert(DIRTY_WORKTREE)`.
///   - If clean and its task is closed or its branch is landed: removes the worktree.
pub fn worktree_sweep(ctx: &SweepContext, now_sec: u64) -> Result<SweepReport, String> {
    let mut report = SweepReport::default();
    if !ctx.worktrees_dir.is_dir() {
        return Ok(report);
    }

    let entries = fs::read_dir(&ctx.worktrees_dir)
        .map_err(|e| format!("read worktrees dir {}: {e}", ctx.worktrees_dir.display()))?;
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join(".git").exists())
        .collect();
    dirs.sort();

    for wt in dirs {
        let norm_wt = normalize_path(&wt);
        let folder_name = wt
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("worktree")
            .to_owned();
        let task_id = infer_task_id(&wt, &folder_name);
        let branch = worktree_branch(&wt);

        let has_live_child = ctx.active_worktrees.contains(&norm_wt)
            || task_id
                .as_ref()
                .is_some_and(|tid| ctx.active_tasks.contains(tid));

        if has_live_child {
            report.actions.push(SweepAction::Kept { worktree: wt });
            continue;
        }

        let dirty = is_worktree_dirty(&wt).unwrap_or(false);
        if dirty {
            let tid = task_id.clone().unwrap_or_else(|| folder_name.clone());
            let attempt = ctx.task_attempts.get(&tid).copied().unwrap_or(1);
            let rescue = make_rescue_ref(&wt, &tid, attempt, ctx.fork_remote.as_deref())?;
            if let (Some(wip_ref), Some(sha)) = (rescue.wip_ref, rescue.sha) {
                let alert_req = build_dirty_worktree_alert(&tid, &wt, &wip_ref, &sha, now_sec);
                report.alerts.push(alert_req);
                report.actions.push(SweepAction::RescuedDirty {
                    worktree: wt,
                    task_id: tid,
                    attempt,
                    wip_ref,
                    sha,
                });
                continue;
            }
        }

        let is_closed = task_id
            .as_ref()
            .is_some_and(|tid| ctx.closed_tasks.contains(tid));
        let is_landed = branch
            .as_ref()
            .is_some_and(|b| ctx.landed_branches.contains(b))
            || is_branch_ancestor_of_main(&wt);

        if !dirty && (is_closed || is_landed) {
            remove_worktree(ctx.repo_clone.as_deref(), &wt)?;
            report.actions.push(SweepAction::Removed {
                worktree: wt,
                task_id,
                branch,
            });
        } else {
            report.actions.push(SweepAction::Kept { worktree: wt });
        }
    }

    Ok(report)
}

/// Runs `worktree_sweep` and sends any resulting `DIRTY_WORKTREE` alerts to the hub (or spools to `outbox`).
pub async fn sweep_and_report(
    ctx: &SweepContext,
    now_sec: u64,
    opts: Option<&Opts>,
    outbox: &Outbox,
) -> Result<SweepReport, String> {
    let report = worktree_sweep(ctx, now_sec)?;
    for alert_req in &report.alerts {
        match opts {
            Some(o) => {
                let _ = outbox.send_or_spool(o, alert_req.clone()).await;
            }
            None => {
                let _ = outbox.enqueue(alert_req.clone());
            }
        }
    }
    Ok(report)
}

fn infer_task_id(wt: &Path, folder_name: &str) -> Option<String> {
    let meta_path = wt.join(".sommelier").join("task_id");
    if let Ok(s) = fs::read_to_string(meta_path) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    for prefix in ["sommelier-agy-", "sommelier-codex-", "sommelier-claude-", "wt-"] {
        if let Some(rest) = folder_name.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return Some(rest.to_owned());
        }
    }
    Some(folder_name.to_owned())
}

fn is_branch_ancestor_of_main(wt: &Path) -> bool {
    for main_ref in ["refs/remotes/upstream/main", "refs/heads/main"] {
        let has_ref = Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["rev-parse", "--verify", main_ref])
            .output()
            .is_ok_and(|o| o.status.success());
        if !has_ref {
            continue;
        }
        // Check if HEAD has at least one commit and is an ancestor of main_ref, while not being a fresh empty branch
        let out = Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["merge-base", "--is-ancestor", "HEAD", main_ref])
            .output();
        if out.is_ok_and(|o| o.status.success()) {
            return false;
        }
    }
    false
}

fn remove_worktree(repo_clone: Option<&Path>, wt: &Path) -> Result<(), String> {
    if let Some(clone) = repo_clone
        && clone.exists()
    {
        let _ = Command::new("git")
            .arg("-C")
            .arg(clone)
            .args(["worktree", "remove", "--force"])
            .arg(wt)
            .output();
    }
    if wt.exists() {
        fs::remove_dir_all(wt)
            .map_err(|e| format!("failed to remove worktree {}: {e}", wt.display()))?;
    }
    Ok(())
}

fn normalize_path(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Builds an `ALERT` request for `DIRTY_WORKTREE` (sections 4.3 and 5.2).
pub fn build_dirty_worktree_alert(
    task_id: &str,
    worktree: &Path,
    wip_ref: &str,
    sha: &str,
    now_sec: u64,
) -> Value {
    let wt_name = worktree
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(task_id);
    let body = json!({
        "v": 1,
        "code": "DIRTY_WORKTREE",
        "key": task_id,
        "severity": "warn",
        "state": "open",
        "opened_at": now_sec,
        "last_at": now_sec,
        "count": 1,
        "text": format!("dirty worktree {wt_name} without live worker; rescued to {wip_ref} ({sha})"),
        "data": {
            "task_id": task_id,
            "worktree": wt_name,
            "wip_ref": wip_ref,
            "sha": sha,
        },
    })
    .to_string();
    json!({
        "op": "send",
        "kind": "ALERT",
        "topic": format!("alert/DIRTY_WORKTREE/{task_id}"),
        "body": body,
        "refs": ["state:open", format!("wip:{wip_ref}")],
        "to": ["coordinator"],
    })
}
