//! Dirty worktree rescue refs (section 7.3, step 12a).
//!
//! When a worker dies (or during worktree sweep when a worktree is dirty with no live child),
//! the supervisor captures uncommitted work into a snapshot ref without touching the branch:
//! 1. `git stash create`
//! 2. `git update-ref refs/heads/wip/<task>-<attempt> <sha>`
//! 3. `git push <fork> refs/heads/wip/<task>-<attempt>:refs/heads/wip/<task>-<attempt>`

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Outcome of inspecting a worktree and creating a rescue ref if dirty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescueOutcome {
    pub dirty: bool,
    pub wip_ref: Option<String>,
    pub sha: Option<String>,
    pub pushed: bool,
}

/// Returns true if the git worktree at `worktree` has uncommitted tracked or untracked changes.
pub fn is_worktree_dirty(worktree: &Path) -> Result<bool, String> {
    if !worktree.is_dir() {
        return Ok(false);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("git status failed: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "git status exited {}: {}",
            out.status,
            stderr.trim()
        ));
    }
    let lines = String::from_utf8_lossy(&out.stdout);
    let has_dirty = lines
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // Ignore the `.sommelier` scratch directory written by the supervisor/worker
        .any(|l| {
            let path = l.get(2..).map(str::trim).unwrap_or(l);
            !path.starts_with(".sommelier") && path != ".sommelier"
        });
    Ok(has_dirty)
}

/// Returns true if the git worktree at `worktree` has untracked files outside `.sommelier`.
fn has_untracked_files(worktree: &Path) -> Result<bool, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("git status failed: {e}"))?;
    if !out.status.success() {
        return Ok(false);
    }
    let lines = String::from_utf8_lossy(&out.stdout);
    Ok(lines.lines().filter(|l| l.starts_with("?? ")).any(|l| {
        let path = l.get(3..).map(str::trim).unwrap_or(l);
        !path.starts_with(".sommelier") && path != ".sommelier"
    }))
}

/// Returns the current `HEAD` commit SHA in `worktree`, if available.
pub fn worktree_head_sha(worktree: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!sha.is_empty() && sha != "HEAD").then_some(sha)
}

/// Returns the current checked-out branch name in `worktree`, if any.
pub fn worktree_branch(worktree: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!b.is_empty() && b != "HEAD").then_some(b)
}

/// Creates a rescue ref `wip/<task>-<attempt>` in `worktree` when dirty (section 7.3)
/// and pushes `refs/heads/wip/<task>-<attempt>` to `fork_remote` when configured.
///
/// Never modifies the checked-out branch or working tree files:
/// - Runs `git stash create` first (captures tracked modifications without touching the index).
/// - If untracked files exist and `git stash create` produced nothing, stages into a temporary
///   `GIT_INDEX_FILE` to write a tree and commit object without mutating the real index or branch.
/// - Updates `refs/heads/wip/<task>-<attempt>` to the resulting commit SHA.
/// - Pushes that ref to `fork_remote` if non-empty.
pub fn make_rescue_ref(
    worktree: &Path,
    task_id: &str,
    attempt: u32,
    fork_remote: Option<&str>,
) -> Result<RescueOutcome, String> {
    if !is_worktree_dirty(worktree)? {
        return Ok(RescueOutcome {
            dirty: false,
            wip_ref: None,
            sha: None,
            pushed: false,
        });
    }

    let safe_task: String = task_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let short_ref = format!("wip/{safe_task}-{attempt}");
    let full_ref = format!("refs/heads/{short_ref}");

    // 1. If untracked files exist, `git stash create` ignores them even when tracked edits make the
    //    stash nonempty; use the temporary index snapshot in that case so both tracked and untracked
    //    files are captured without mutating the real index or branch.
    let mut sha = if has_untracked_files(worktree)? {
        String::new()
    } else {
        let stash_out = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["stash", "create", &format!("sommelier rescue {short_ref}")])
            .output()
            .map_err(|e| format!("git stash create failed: {e}"))?;
        String::from_utf8_lossy(&stash_out.stdout).trim().to_owned()
    };

    // If `git stash create` was skipped or returned empty (e.g. untracked files or missing git user config),
    // create a snapshot commit via a temporary index file so the real index and branch are untouched.
    if sha.is_empty() {
        sha = create_snapshot_commit_temp_index(worktree, &short_ref)?;
    }

    if sha.is_empty() {
        return Err("failed to produce rescue commit SHA".into());
    }

    // 2. `git update-ref refs/heads/wip/<task>-<attempt> <sha>`
    let upd = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["update-ref", &full_ref, &sha])
        .output()
        .map_err(|e| format!("git update-ref failed: {e}"))?;
    if !upd.status.success() {
        let err = String::from_utf8_lossy(&upd.stderr);
        return Err(format!("git update-ref {full_ref} failed: {}", err.trim()));
    }

    // 3. Push `refs/heads/wip/<task>-<attempt>` to the fork remote if configured and present.
    let mut pushed = false;
    if let Some(remote) = fork_remote.map(str::trim).filter(|r| !r.is_empty()) {
        let refspec = format!("{full_ref}:{full_ref}");
        if let Ok(push_out) = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["push", "-f", remote, &refspec])
            .output()
            && push_out.status.success()
        {
            pushed = true;
        }
    }

    Ok(RescueOutcome {
        dirty: true,
        wip_ref: Some(short_ref),
        sha: Some(sha),
        pushed,
    })
}

fn create_snapshot_commit_temp_index(worktree: &Path, short_ref: &str) -> Result<String, String> {
    let temp_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let temp_index = temp_dir.path().join("rescue.index");

    // Seed temporary index from HEAD if HEAD exists.
    let head = worktree_head_sha(worktree);
    if head.is_some() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .env("GIT_INDEX_FILE", &temp_index)
            .args(["read-tree", "HEAD"])
            .output();
    }

    let add_out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .env("GIT_INDEX_FILE", &temp_index)
        .args(["add", "-A", "--", ".", ":!.sommelier"])
        .output()
        .map_err(|e| format!("git add to temp index failed: {e}"))?;
    if !add_out.status.success() {
        // Fallback without pathspec exclusion if older git rejects `:!.sommelier`
        let _ = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .env("GIT_INDEX_FILE", &temp_index)
            .args(["add", "-A"])
            .output();
    }

    let tree_out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .env("GIT_INDEX_FILE", &temp_index)
        .arg("write-tree")
        .output()
        .map_err(|e| format!("git write-tree failed: {e}"))?;
    if !tree_out.status.success() {
        return Err(String::from_utf8_lossy(&tree_out.stderr).trim().to_owned());
    }
    let tree_sha = String::from_utf8_lossy(&tree_out.stdout).trim().to_owned();

    let mut commit_cmd = Command::new("git");
    commit_cmd
        .arg("-C")
        .arg(worktree)
        .env("GIT_AUTHOR_NAME", "sommelier")
        .env("GIT_AUTHOR_EMAIL", "sommelier@local")
        .env("GIT_COMMITTER_NAME", "sommelier")
        .env("GIT_COMMITTER_EMAIL", "sommelier@local")
        .arg("commit-tree")
        .arg(&tree_sha);
    if let Some(parent) = &head {
        commit_cmd.args(["-p", parent]);
    }
    commit_cmd.args(["-m", &format!("sommelier rescue {short_ref}")]);

    let commit_out = commit_cmd
        .output()
        .map_err(|e| format!("git commit-tree failed: {e}"))?;
    if !commit_out.status.success() {
        return Err(String::from_utf8_lossy(&commit_out.stderr)
            .trim()
            .to_owned());
    }
    Ok(String::from_utf8_lossy(&commit_out.stdout)
        .trim()
        .to_owned())
}
