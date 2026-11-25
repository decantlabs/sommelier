//! Branch sweep (section 4.3), run sweep, and clean-room licence marker scan.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::land::schema::{BranchRow, LandingRow, RunRow};
use crate::msg::{Kind, Message, Role};
use crate::task::{LeaseRow, TaskRow};
use crate::time::rfc3339;

/// Configuration for `branch_sweep` (section 4.3, 5.3 `[sweep]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepConfig {
    pub orphan_hours: u64,
    pub keep_days: u64,
    pub our_namespaces: Vec<String>,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            orphan_hours: 24,
            keep_days: 7,
            our_namespaces: vec!["codex/".into(), "gemini/".into(), "wip/".into()],
        }
    }
}

/// Remote branch deletion planned by `branch_sweep`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteRemoteBranch {
    pub remote: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub head_sha: String,
}

/// Output of `branch_sweep` (section 4.3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSweepResult {
    pub updated_branches: Vec<BranchRow>,
    pub orphan_alerts: Vec<(String, String)>,
    pub triage_tasks: Vec<TaskRow>,
    pub delete_branches: Vec<DeleteRemoteBranch>,
}

/// Observed run folder on disk for `run_sweep`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFolderInfo {
    pub run_id: String,
    pub folder: String,
    pub machine: String,
    pub sha: String,
    pub label: String,
}

/// Output of `run_sweep` (section 4.3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSweepResult {
    pub unregistered_runs: Vec<RunRow>,
    pub alerts: Vec<(String, String)>,
}

/// Strips `refs/heads/` or `<remote>/` prefix when checking branch namespace.
pub fn normalize_branch_short_name<'a>(remote: &str, ref_name: &'a str) -> &'a str {
    let trimmed = ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name);
    if !remote.is_empty()
        && let Some(rest) = trimmed.strip_prefix(remote)
        && let Some(after_slash) = rest.strip_prefix('/')
    {
        return after_slash;
    }
    trimmed
}

/// Returns true when the branch belongs to our namespaces (`codex/*`, `gemini/*`, `wip/*`)
/// and is not on a guest remote (`upstream`).
pub fn is_our_namespace(remote: &str, ref_name: &str, namespaces: &[String]) -> bool {
    if remote.eq_ignore_ascii_case("upstream") {
        return false;
    }
    let short = normalize_branch_short_name(remote, ref_name);
    if short.eq_ignore_ascii_case("main") || short.eq_ignore_ascii_case("master") {
        return false;
    }
    namespaces.iter().any(|ns| short.starts_with(ns.as_str()))
}

fn sanitize_triage_id(remote: &str, ref_name: &str) -> String {
    let raw = if remote.is_empty() {
        ref_name.to_owned()
    } else {
        format!("{remote}-{ref_name}")
    };
    let clean: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("triage-{}", clean.trim_matches('-'))
}

fn branch_matches_task(b: &BranchRow, t: &TaskRow) -> bool {
    if !b.task_id.is_empty() && b.task_id == t.id {
        return true;
    }
    if t.branch.is_empty() {
        return false;
    }
    let b_short = normalize_branch_short_name(&b.remote, &b.ref_name);
    let t_short = normalize_branch_short_name(&b.remote, &t.branch);
    b.ref_name == t.branch || b_short == t_short
}

/// Deterministic branch sweep (section 4.3).
#[allow(clippy::too_many_arguments)]
pub fn branch_sweep(
    branches: &[BranchRow],
    landings: &[LandingRow],
    tasks: &[TaskRow],
    leases: &[LeaseRow],
    open_orphan_alert_keys: &BTreeSet<String>,
    existing_triage_task_ids: &BTreeSet<String>,
    now_sec: u64,
    cfg: &SweepConfig,
) -> BranchSweepResult {
    let mut out = BranchSweepResult::default();
    let live_task_ids: BTreeSet<&str> = leases
        .iter()
        .filter(|l| matches!(l.state.as_str(), "active" | "suspect"))
        .map(|l| l.task_id.as_str())
        .collect();

    let mut landing_by_ref_sha: BTreeMap<(&str, &str), &LandingRow> = BTreeMap::new();
    for l in landings {
        landing_by_ref_sha.insert((l.ref_name.as_str(), l.sha.as_str()), l);
    }

    let orphan_cutoff = cfg.orphan_hours.saturating_mul(3600);
    let keep_cutoff = cfg.keep_days.saturating_mul(86_400);

    for b in branches {
        let mut next = b.clone();
        let key = if b.remote.is_empty() {
            b.ref_name.clone()
        } else {
            format!("{}/{}", b.remote, b.ref_name)
        };

        let in_landing_not_failed = landing_by_ref_sha
            .get(&(b.ref_name.as_str(), b.head_sha.as_str()))
            .or_else(|| landing_by_ref_sha.get(&(key.as_str(), b.head_sha.as_str())))
            .is_some_and(|l| l.state != "failed");

        let has_live_lease_task = tasks
            .iter()
            .any(|t| live_task_ids.contains(t.id.as_str()) && branch_matches_task(b, t));
        let has_delivered_task = tasks
            .iter()
            .any(|t| t.state == "delivered" && branch_matches_task(b, t));

        let next_state = if b.ahead == 0 || b.state == "landed" {
            "landed"
        } else if in_landing_not_failed {
            "landing"
        } else if has_live_lease_task {
            "active"
        } else if has_delivered_task {
            "delivered"
        } else if b.last_commit_at > 0 && now_sec.saturating_sub(b.last_commit_at) > orphan_cutoff {
            "orphan"
        } else if b.state.is_empty() {
            "active"
        } else {
            b.state.as_str()
        };

        next.state = next_state.to_owned();
        if next.state != b.state {
            out.updated_branches.push(next.clone());
        }

        if next.state == "orphan" {
            if !open_orphan_alert_keys.contains(&key) {
                out.orphan_alerts.push((
                    key.clone(),
                    format!("orphan branch {key} (sha {})", b.head_sha),
                ));
            }
            let triage_id = sanitize_triage_id(&b.remote, &b.ref_name);
            let already_has_triage = existing_triage_task_ids.contains(&triage_id)
                || tasks
                    .iter()
                    .any(|t| t.kind == "triage" && branch_matches_task(b, t));
            if !already_has_triage {
                out.triage_tasks.push(TaskRow {
                    id: triage_id,
                    title: format!("Triage orphan branch {key}"),
                    kind: "triage".into(),
                    priority: "filler".into(),
                    state: "ready".into(),
                    scope: "read-only".into(),
                    branch: b.ref_name.clone(),
                    base_sha: b.head_sha.clone(),
                    done_when: "decide land, continue or drop".into(),
                    created_by: "hub".into(),
                    created_at: now_sec,
                    ready_at: now_sec,
                    ..TaskRow::default()
                });
            }
        }

        if next.state == "landed" && is_our_namespace(&b.remote, &b.ref_name, &cfg.our_namespaces) {
            let ref_ts = if b.last_commit_at > 0 {
                b.last_commit_at
            } else {
                b.seen_at
            };
            if ref_ts > 0 && now_sec.saturating_sub(ref_ts) > keep_cutoff {
                out.delete_branches.push(DeleteRemoteBranch {
                    remote: b.remote.clone(),
                    ref_name: b.ref_name.clone(),
                    head_sha: b.head_sha.clone(),
                });
            }
        }
    }

    out
}

/// Deterministic run sweep (section 4.3): flags run folders that have no `RUN` row.
pub fn run_sweep(
    folders: &[RunFolderInfo],
    existing_runs: &[RunRow],
    open_unregistered_alert_keys: &BTreeSet<String>,
) -> RunSweepResult {
    let mut out = RunSweepResult::default();
    let known_ids: BTreeSet<&str> = existing_runs.iter().map(|r| r.run_id.as_str()).collect();
    let known_folders: BTreeSet<&str> = existing_runs
        .iter()
        .map(|r| r.folder.as_str())
        .filter(|f| !f.is_empty())
        .collect();

    for f in folders {
        if known_ids.contains(f.run_id.as_str())
            || (!f.folder.is_empty() && known_folders.contains(f.folder.as_str()))
        {
            continue;
        }
        out.unregistered_runs.push(RunRow {
            run_id: f.run_id.clone(),
            machine: f.machine.clone(),
            sha: f.sha.clone(),
            label: f.label.clone(),
            kind: "check".into(),
            folder: f.folder.clone(),
            registered: false,
            ..RunRow::default()
        });
        if !open_unregistered_alert_keys.contains(&f.run_id) {
            out.alerts.push((
                f.run_id.clone(),
                format!("unregistered run folder {} ({})", f.run_id, f.folder),
            ));
        }
    }
    out
}

/// Builds an `ALERT` message for `ORPHAN_BRANCH` or `UNREGISTERED_RUN`.
pub fn build_land_alert_message(
    code: &str,
    key: &str,
    severity: &str,
    text: &str,
    data: serde_json::Value,
    now_ms: u64,
) -> Message {
    Message {
        id: 0,
        ts: rfc3339(now_ms),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Alert,
        topic: format!("alert/{code}/{key}"),
        body: json!({
            "v": 1,
            "code": code,
            "key": key,
            "severity": severity,
            "to": "coordinator",
            "text": text,
            "data": data,
        })
        .to_string(),
        refs: vec![format!("alert:{code}"), format!("key:{key}")],
        to: vec!["coordinator".into()],
    }
}

/// Builds a `TASK_UPDATE` message creating a `triage` task from `branch_sweep`.
pub fn build_triage_task_message(task: &TaskRow, now_ms: u64) -> Message {
    Message {
        id: 0,
        ts: rfc3339(now_ms),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::TaskUpdate,
        topic: format!("task/{}", task.id),
        body: json!({
            "v": 1,
            "task": task.id,
            "prev": "",
            "state": task.state,
            "attempt": task.attempt,
            "by": "hub",
            "reason": "orphan_branch_sweep",
            "task_row": task,
        })
        .to_string(),
        refs: vec![format!("state:{}", task.state)],
        to: Vec::new(),
    }
}

/// Scans added lines of a unified git diff for GPL/LGPL/AGPL licence markers.
pub fn scan_licence_markers(diff_text: &str) -> Vec<String> {
    let markers = [
        "GPL-2.0",
        "GPL-3.0",
        "LGPL-2.0",
        "LGPL-2.1",
        "LGPL-3.0",
        "AGPL-3.0",
        "GNU General Public License",
        "GNU Lesser General Public License",
        "GNU Affero General Public License",
    ];
    let mut hits = Vec::new();
    for (idx, line) in diff_text.lines().enumerate() {
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let added = &line[1..];
        let is_spdx = added.contains("SPDX-License-Identifier:")
            && (added.contains("GPL") || added.contains("LGPL") || added.contains("AGPL"));
        if is_spdx || markers.iter().any(|m| added.contains(m)) {
            hits.push(format!("line {}: {}", idx + 1, added.trim()));
        }
    }
    hits
}
