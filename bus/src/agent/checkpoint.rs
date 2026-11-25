//! Checkpoint watcher and forwarder (section 7.3, step 12a).
//!
//! Section 7.3:
//! - Every task of a checkpointable kind writes `.sommelier/checkpoint.md` in its worktree
//!   every `checkpoint_min` (30) and at each milestone.
//! - The supervisor watches the file and forwards each new version (`task.checkpoint` op,
//!   note at most 1,200 characters).
//! - Parses `BRANCH: <branch> COMMIT: <sha> PUSHED: yes|no` when present in the checkpoint text.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::capture::{MAX_NOTE_CHARS, truncate_chars};
use crate::agent::outbox::Outbox;
use crate::agent::rescue::worktree_head_sha;
use crate::client::Opts;

/// Parsed metadata and bounded note from `.sommelier/checkpoint.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedCheckpoint {
    pub note: String,
    pub branch: String,
    pub sha: String,
    pub pushed: bool,
}

/// Parses `.sommelier/checkpoint.md` content, extracting `BRANCH:`, `COMMIT:`, and `PUSHED:` fields
/// if present and bounding the note to `MAX_NOTE_CHARS` (1,200 characters).
pub fn parse_checkpoint_text(
    raw: &str,
    default_branch: &str,
    default_sha: &str,
) -> ParsedCheckpoint {
    let cleaned = raw.trim_start_matches('\u{feff}').trim();
    let note = truncate_chars(cleaned, MAX_NOTE_CHARS);

    let mut branch = default_branch.to_owned();
    let mut sha = default_sha.to_owned();
    let mut pushed = false;

    for line in cleaned.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let mut i = 0;
        while i < tokens.len() {
            let key = tokens[i].to_ascii_uppercase();
            if key == "BRANCH:" && i + 1 < tokens.len() {
                branch = tokens[i + 1].trim_matches(',').to_owned();
                i += 2;
            } else if (key == "COMMIT:" || key == "SHA:") && i + 1 < tokens.len() {
                sha = tokens[i + 1].trim_matches(',').to_owned();
                i += 2;
            } else if key == "PUSHED:" && i + 1 < tokens.len() {
                let val = tokens[i + 1].trim_matches(',').to_ascii_lowercase();
                pushed = matches!(val.as_str(), "yes" | "true" | "1");
                i += 2;
            } else {
                i += 1;
            }
        }
    }

    ParsedCheckpoint {
        note,
        branch,
        sha,
        pushed,
    }
}

/// Stateful watcher for `.sommelier/checkpoint.md` in a task worktree.
#[derive(Clone, Debug)]
pub struct CheckpointWatcher {
    task_id: String,
    branch: String,
    worktree: PathBuf,
    path: PathBuf,
    last_forwarded_note: Option<String>,
}

impl CheckpointWatcher {
    pub fn new(
        task_id: impl Into<String>,
        branch: impl Into<String>,
        worktree: impl Into<PathBuf>,
    ) -> Self {
        let wt = worktree.into();
        let path = wt.join(".sommelier").join("checkpoint.md");
        Self {
            task_id: task_id.into(),
            branch: branch.into(),
            worktree: wt,
            path,
            last_forwarded_note: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn last_forwarded_note(&self) -> Option<&str> {
        self.last_forwarded_note.as_deref()
    }

    /// Checks whether `.sommelier/checkpoint.md` has a new non-empty version since the last check.
    /// Returns `Some(ParsedCheckpoint)` and records the note as seen when changed.
    pub fn poll_change(&mut self) -> Option<ParsedCheckpoint> {
        let raw = fs::read_to_string(&self.path).ok()?;
        let trimmed = raw.trim_start_matches('\u{feff}').trim();
        if trimmed.is_empty() {
            return None;
        }
        let default_sha = worktree_head_sha(&self.worktree).unwrap_or_default();
        let parsed = parse_checkpoint_text(trimmed, &self.branch, &default_sha);
        if self.last_forwarded_note.as_deref() == Some(parsed.note.as_str()) {
            return None;
        }
        self.last_forwarded_note = Some(parsed.note.clone());
        Some(parsed)
    }

    /// Builds the `task.checkpoint` request value for a parsed checkpoint.
    pub fn build_request(&self, cp: &ParsedCheckpoint) -> Value {
        json!({
            "op": "task.checkpoint",
            "task": self.task_id,
            "note": cp.note,
            "branch": cp.branch,
            "sha": cp.sha,
            "pushed": cp.pushed,
            "source": "supervisor",
        })
    }

    /// Polls `.sommelier/checkpoint.md` and forwards any new version to the hub (or spools to `outbox`).
    /// Returns `Ok(Some(ParsedCheckpoint))` if a new checkpoint was forwarded.
    pub async fn poll_and_forward(
        &mut self,
        opts: Option<&Opts>,
        outbox: &Outbox,
    ) -> Result<Option<ParsedCheckpoint>, String> {
        let Some(cp) = self.poll_change() else {
            return Ok(None);
        };
        let req = self.build_request(&cp);
        match opts {
            Some(o) => {
                let _ = outbox.send_or_spool(o, req).await?;
            }
            None => {
                outbox.enqueue(req).map_err(|e| e.to_string())?;
            }
        }
        Ok(Some(cp))
    }
}
