//! Exit output and checkpoint capture (sections 4.2, 5.4, 7.3, and step 7).
//!
//! Captures the worker's last message (`last.txt`), error output (`err.txt`),
//! event stream (`events.jsonl`), and the latest checkpoint (`.sommelier/checkpoint.md`).
//! Never captures raw vendor logs (finding F6: vendor logs never leave the machine).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Maximum characters kept in a checkpoint or resume note (section 3 and 7.3).
pub const MAX_NOTE_CHARS: usize = 1200;
/// Maximum characters kept for the tail of the last message.
pub const MAX_TAIL_CHARS: usize = 600;

/// Captured files and summaries at worker exit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitCapture {
    /// Full content of `$SOMMELIER_OUT/last.txt` (or `<out>/<name>.last.txt`).
    pub last_message: String,
    /// First non-empty line of `last_message` (trimmed).
    pub first_line: String,
    /// Bounded tail of `last_message` (at most `MAX_TAIL_CHARS` characters).
    pub last_tail: String,
    /// Content of `$SOMMELIER_OUT/err.txt` (or `<out>/<name>.err.txt`), used for quota detection.
    pub err_text: String,
    /// Tail or rate-limit lines of `$SOMMELIER_OUT/events.jsonl` if present.
    pub events_text: String,
    /// Content of `.sommelier/checkpoint.md` in the worktree (at most `MAX_NOTE_CHARS` characters).
    pub checkpoint: Option<String>,
    /// Branch name of the worker.
    pub branch: String,
    /// Head commit SHA if found in the result or worktree.
    pub head_sha: Option<String>,
    /// Optional rescue ref (`wip/<task>-<attempt>`).
    pub wip_ref: Option<String>,
    /// Whether a handover marker or checkpoint handover was recorded before exit.
    pub handover_recorded: bool,
}

impl ExitCapture {
    /// Captures output files from `out_dir` (and optional `worktree` and `worker_name`).
    ///
    /// Supports both the hook layout of section 5.4 (`$SOMMELIER_OUT/last.txt`, `err.txt`, `events.jsonl`)
    /// and the launcher layout (`$OUT/<name>.last.txt`, `<name>.err.txt`, `<name>.events.jsonl`).
    pub fn collect(
        out_dir: &Path,
        worker_name: Option<&str>,
        worktree: Option<&Path>,
        branch: &str,
    ) -> Self {
        let last_path = resolve_file(out_dir, worker_name, "last.txt");
        let err_path = resolve_file(out_dir, worker_name, "err.txt");
        let events_path = resolve_file(out_dir, worker_name, "events.jsonl");
        let handover_path = resolve_file(out_dir, worker_name, "handover");

        let last_message = last_path
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default()
            .trim_start_matches('\u{feff}')
            .to_owned();
        let err_text = err_path
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default()
            .trim_start_matches('\u{feff}')
            .to_owned();
        let events_text = events_path
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default()
            .trim_start_matches('\u{feff}')
            .to_owned();

        let first_line = last_message
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_owned();
        let last_tail = tail_chars(&last_message, MAX_TAIL_CHARS);

        let checkpoint = worktree
            .map(|wt| wt.join(".sommelier").join("checkpoint.md"))
            .or_else(|| Some(out_dir.join("checkpoint.md")))
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|s| truncate_chars(s.trim_start_matches('\u{feff}').trim(), MAX_NOTE_CHARS))
            .filter(|s| !s.is_empty());

        let handover_in_wt = worktree
            .map(|wt| wt.join(".sommelier").join("handover").is_file())
            .unwrap_or(false);
        let handover_recorded = handover_path.is_some() || handover_in_wt;

        let head_sha = extract_sha_from_first_line(&first_line);

        Self {
            last_message,
            first_line,
            last_tail,
            err_text,
            events_text,
            checkpoint,
            branch: branch.to_owned(),
            head_sha,
            wip_ref: None,
            handover_recorded,
        }
    }

    /// Builds the combined resume note for `Requeue(t)` (section 4.2):
    /// newest checkpoint + tail of the last message + branch + head sha + wip ref,
    /// bounded to `MAX_NOTE_CHARS`.
    pub fn resume_note(&self) -> String {
        let mut parts = Vec::new();
        if !self.branch.is_empty() {
            let mut header = format!("branch: {}", self.branch);
            if let Some(sha) = &self.head_sha {
                header.push_str(&format!(" sha: {sha}"));
            }
            if let Some(wip) = &self.wip_ref {
                header.push_str(&format!(" wip: {wip}"));
            }
            parts.push(header);
        }
        if let Some(cp) = &self.checkpoint {
            parts.push(format!("checkpoint: {}", single_line(cp)));
        }
        if !self.last_tail.is_empty() {
            parts.push(format!("last: {}", single_line(&self.last_tail)));
        }
        truncate_chars(&parts.join(" | "), MAX_NOTE_CHARS)
    }

    /// Short one-line summary for the `STATUS` row on exit.
    pub fn status_headline(&self, exit_code: Option<i32>, reason: &str) -> String {
        let code_str = exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into());
        let summary = if !self.first_line.is_empty() {
            single_line(&truncate_chars(&self.first_line, 140))
        } else if !self.last_tail.is_empty() {
            single_line(&truncate_chars(&self.last_tail, 140))
        } else {
            reason.to_owned()
        };
        format!("finished ({reason}, exit {code_str}): {summary}")
    }
}

fn resolve_file(out_dir: &Path, worker_name: Option<&str>, suffix: &str) -> Option<PathBuf> {
    if let Some(name) = worker_name {
        let named = out_dir.join(format!("{name}.{suffix}"));
        if named.is_file() {
            return Some(named);
        }
    }
    let direct = out_dir.join(suffix);
    if direct.is_file() {
        return Some(direct);
    }
    None
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

pub fn tail_chars(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    let count = trimmed.chars().count();
    if count <= max {
        return trimmed.to_owned();
    }
    trimmed.chars().skip(count - max).collect()
}

fn single_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn extract_sha_from_first_line(first_line: &str) -> Option<String> {
    for word in first_line.split_whitespace() {
        let clean = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if (7..=64).contains(&clean.len()) && clean.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(clean.to_ascii_lowercase());
        }
    }
    None
}
