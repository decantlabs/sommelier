//! Task state machine, score calculation, scope overlap, and result validation (sections 3, 4.1, 4.2, 8.6).

use std::collections::BTreeSet;
use std::fs;

use rusqlite::Connection;

use super::schema::{TaskRow, query_tasks};

fn prefix_overlaps(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@' | '%' | '=' | '#')
}

fn names(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c| !name_char(c))
        .map(|s| s.trim_end_matches('.'))
        .filter(|s| !s.is_empty())
}

fn branches(text: &str) -> impl Iterator<Item = &str> {
    names(text).filter_map(|name| {
        let name = ["refs/heads/", "refs/remotes/origin/", "origin/"]
            .into_iter()
            .find_map(|prefix| name.strip_prefix(prefix))
            .unwrap_or(name);
        let (namespace, rest) = name.split_once('/')?;
        (matches!(namespace, "codex" | "gemini") && !rest.is_empty()).then_some(name)
    })
}

fn foreign_branch(line: &str, branch: &str) -> Result<(), String> {
    if let Some(other) = branches(line).find(|name| *name != branch) {
        return Err(format!(
            "Found branch {other}, expected {branch}. Line: {line}"
        ));
    }
    Ok(())
}

fn is_sha(word: &str) -> bool {
    (7..=64).contains(&word.len()) && word.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn check_remote(text: &str, branch: &str, remote: &str) -> Result<(), String> {
    let mut commits = names(text).filter(|word| is_sha(word));
    let commit = commits.next().ok_or("Result does not name a commit")?;
    if commits.any(|other| !other.eq_ignore_ascii_case(commit)) {
        return Err("Result names more than one commit".into());
    }
    let expected_ref = format!("refs/heads/{branch}");
    let remote_sha = remote
        .trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find_map(|(sha, name)| (name.trim() == expected_ref).then_some(sha))
        .ok_or_else(|| format!("Remote output has no entry for {expected_ref}"))?;
    if matches!(remote_sha.len(), 40 | 64)
        && is_sha(remote_sha)
        && remote_sha
            .get(..commit.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(commit))
    {
        Ok(())
    } else {
        Err(format!(
            "Result commit {commit} does not match remote commit {remote_sha} for {branch}"
        ))
    }
}

/// Validates a worker result text (mirrors `dispatch::check_result`).
pub fn check_result(
    text: &str,
    branch: &str,
    task: &str,
    remote_path: Option<&str>,
) -> Result<(), String> {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or("");
    if !matches!(names(first).next(), Some("READY" | "STUCK"))
        || !(first.starts_with("READY") || first.starts_with("STUCK"))
    {
        return Err("First nonempty line must start with READY or STUCK".into());
    }
    let end = text.char_indices().nth(400).map_or(text.len(), |(i, _)| i);
    if !names(text).any(|name| {
        (name == branch || name == task)
            && name.as_bytes().as_ptr_range().end <= text.as_bytes()[..end].as_ptr_range().end
    }) {
        return Err(format!("First 400 characters must name {branch} or {task}"));
    }
    if let Some(pushed) = branches(first).next()
        && pushed != branch
    {
        return Err(format!(
            "Found result branch {pushed}, expected {branch}. Line: {first}"
        ));
    }
    for line in std::iter::once(first).chain(lines) {
        if names(line).any(|word| matches!(word.to_ascii_lowercase().as_str(), "push" | "pushed"))
            || line.to_ascii_lowercase().starts_with("branch:")
        {
            foreign_branch(line, branch)?;
        }
    }
    if let Some(path) = remote_path.filter(|p| !p.trim().is_empty()) {
        let remote = fs::read_to_string(path).map_err(|e| format!("Cannot read {path}. {e}"))?;
        check_remote(text, branch, &remote)?;
    }
    Ok(())
}

/// Maximum length of a checkpoint note in characters (section 3).
pub const MAX_CHECKPOINT_NOTE_CHARS: usize = 1200;

/// Valid task lifecycle states (section 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TaskState {
    Draft,
    Ready,
    Offered,
    Leased,
    Running,
    Delivered,
    Landing,
    Closed,
    Stuck,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Draft => "draft",
            TaskState::Ready => "ready",
            TaskState::Offered => "offered",
            TaskState::Leased => "leased",
            TaskState::Running => "running",
            TaskState::Delivered => "delivered",
            TaskState::Landing => "landing",
            TaskState::Closed => "closed",
            TaskState::Stuck => "stuck",
            TaskState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "draft" => Some(TaskState::Draft),
            "ready" => Some(TaskState::Ready),
            "offered" => Some(TaskState::Offered),
            "leased" => Some(TaskState::Leased),
            "running" => Some(TaskState::Running),
            "delivered" => Some(TaskState::Delivered),
            "landing" => Some(TaskState::Landing),
            "closed" => Some(TaskState::Closed),
            "stuck" => Some(TaskState::Stuck),
            "cancelled" => Some(TaskState::Cancelled),
            _ => None,
        }
    }

    /// Returns true if transitioning from `self` to `next` is allowed by the task state machine.
    pub fn can_transition(self, next: TaskState) -> bool {
        if self == next {
            return true;
        }
        matches!(
            (self, next),
            (TaskState::Draft, TaskState::Ready | TaskState::Cancelled)
                | (
                    TaskState::Ready,
                    TaskState::Offered
                        | TaskState::Leased
                        | TaskState::Running
                        | TaskState::Stuck
                        | TaskState::Cancelled
                )
                | (
                    TaskState::Offered,
                    TaskState::Ready
                        | TaskState::Leased
                        | TaskState::Running
                        | TaskState::Stuck
                        | TaskState::Cancelled
                )
                | (
                    TaskState::Leased,
                    TaskState::Running
                        | TaskState::Ready
                        | TaskState::Delivered
                        | TaskState::Stuck
                        | TaskState::Cancelled
                )
                | (
                    TaskState::Running,
                    TaskState::Ready
                        | TaskState::Delivered
                        | TaskState::Stuck
                        | TaskState::Cancelled
                )
                | (
                    TaskState::Delivered,
                    TaskState::Landing | TaskState::Closed | TaskState::Ready | TaskState::Stuck
                )
                | (
                    TaskState::Landing,
                    TaskState::Closed | TaskState::Ready | TaskState::Stuck
                )
                | (TaskState::Stuck, TaskState::Ready | TaskState::Cancelled)
        )
    }
}

/// Priority level score used by `score()` (section 4.1 and `swarm.policy.toml`).
pub fn priority_level(priority: &str) -> i64 {
    match priority.trim().to_ascii_lowercase().as_str() {
        "blocker" => 100,
        "high" => 60,
        "normal" => 30,
        "filler" => 5,
        _ => 30,
    }
}

/// Deterministic task score for `next` ordering before step 11 scheduler (section 4.1).
///
/// `score(t, now) = level(priority) + min(40, 2 * floor(minutes_ready / 10)) + (25 if attempt > 1 else 0)`
pub fn task_score(task: &TaskRow, now_sec: u64) -> i64 {
    let base = priority_level(&task.priority);
    let ready_at = if task.ready_at > 0 {
        task.ready_at
    } else {
        task.created_at
    };
    let minutes_ready = now_sec.saturating_sub(ready_at) / 60;
    let aging = ((minutes_ready / 10) * 2).min(40) as i64;
    let resume_boost = if task.attempt > 1 { 25 } else { 0 };
    base + aging + resume_boost
}

/// Splits a comma-separated glob list into trimmed non-empty patterns.
pub fn split_csv(list: &str) -> Vec<&str> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Returns true if any glob in `a` prefix-overlaps any glob in `b` (section 4.1).
pub fn scopes_overlap(a: &str, b: &str) -> bool {
    let ga = split_csv(a);
    let gb = split_csv(b);
    for pa in &ga {
        let pa_norm = pa.trim_end_matches('*');
        for pb in &gb {
            let pb_norm = pb.trim_end_matches('*');
            if prefix_overlaps(pa_norm, pb_norm) {
                return true;
            }
        }
    }
    false
}

/// Checks whether a task has all required fields to enter `ready` (section 3, 4.1).
pub fn has_required_fields(task: &TaskRow) -> bool {
    !task.id.trim().is_empty()
        && !task.title.trim().is_empty()
        && !task.kind.trim().is_empty()
        && !task.scope.trim().is_empty()
        && !task.done_when.trim().is_empty()
}

/// Returns true if all dependency task IDs in `task.deps` exist and are `closed`.
pub fn deps_closed(all_tasks: &[TaskRow], task: &TaskRow) -> bool {
    if task.deps.is_empty() {
        return true;
    }
    let closed: BTreeSet<&str> = all_tasks
        .iter()
        .filter(|t| t.state == TaskState::Closed.as_str())
        .map(|t| t.id.as_str())
        .collect();
    task.deps.iter().all(|d| closed.contains(d.as_str()))
}

/// Returns true if the task is eligible for `next` (ready, required fields present, deps closed).
pub fn is_admissible_ready(
    all_tasks: &[TaskRow],
    task: &TaskRow,
    class_filter: Option<&str>,
) -> bool {
    if task.state != TaskState::Ready.as_str() {
        return false;
    }
    if !has_required_fields(task) || !deps_closed(all_tasks, task) {
        return false;
    }
    if let Some(cls) = class_filter.map(str::trim).filter(|s| !s.is_empty()) {
        let accepts = split_csv(&task.accepts);
        if !accepts.is_empty() && !accepts.iter().any(|a| a.eq_ignore_ascii_case(cls)) {
            return false;
        }
    }
    // Check scope overlap with any currently leased/running task.
    !all_tasks.iter().any(|other| {
        other.id != task.id
            && matches!(other.state.as_str(), "leased" | "running")
            && scopes_overlap(&task.scope, &other.scope)
    })
}

/// Selects the next ready task in `(score desc, id asc)` order (section 4.1, step 8).
pub fn select_next_ready(
    conn: &Connection,
    now_sec: u64,
    class_filter: Option<&str>,
) -> Result<Option<(TaskRow, i64, usize)>, String> {
    let all = query_tasks(conn)?;
    let mut candidates: Vec<(i64, TaskRow)> = all
        .iter()
        .filter(|t| is_admissible_ready(&all, t, class_filter))
        .map(|t| (task_score(t, now_sec), t.clone()))
        .collect();
    candidates.sort_by(|(sa, ta), (sb, tb)| sb.cmp(sa).then_with(|| ta.id.cmp(&tb.id)));
    let total = candidates.len();
    Ok(candidates
        .into_iter()
        .next()
        .map(|(score, task)| (task, score, total)))
}

/// Validates that `result_text` contains a prior art verdict line when required (section 8.6, step 8).
pub fn has_prior_art_verdict(result_text: &str) -> bool {
    for raw_line in result_text.trim_start_matches('\u{feff}').lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        for prefix in [
            "prior art:",
            "prior_art:",
            "prior-art:",
            "prior art verdict:",
            "verdict:",
        ] {
            if let Some(rest) = lower.strip_prefix(prefix)
                && !rest.trim().is_empty()
            {
                return true;
            }
        }
    }
    false
}

/// Validates a `done` submission using `check_result` and the prior art verdict line check (step 8).
pub fn validate_done_result(
    result_text: &str,
    branch: &str,
    task_id: &str,
    remote_sha_path: Option<&str>,
    require_prior_art: bool,
) -> Result<(), String> {
    check_result(result_text, branch, task_id, remote_sha_path)?;
    if require_prior_art && !has_prior_art_verdict(result_text) {
        return Err(
            "result missing prior art verdict line (expected `PRIOR ART: <verdict>`)".into(),
        );
    }
    Ok(())
}

/// Truncates a checkpoint note to at most `MAX_CHECKPOINT_NOTE_CHARS` characters.
pub fn normalize_checkpoint_note(note: &str) -> Result<String, String> {
    let trimmed = note.trim();
    if trimmed.chars().count() > MAX_CHECKPOINT_NOTE_CHARS {
        return Err(format!(
            "checkpoint note exceeds {} characters",
            MAX_CHECKPOINT_NOTE_CHARS
        ));
    }
    Ok(trimmed.to_owned())
}
