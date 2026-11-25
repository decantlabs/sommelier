//! The diff of a submission as plain text for the reviewer. Pure.

use crate::gate::diff::{ChangeKind, FileChange, FileMode, ParsedDiff};

/// Most characters of the text. A longer diff is cut and says so, the reviewer then escalates.
pub const MAX_DIFF_CHARS: usize = 400_000;

fn kind_word(c: ChangeKind) -> &'static str {
    match c {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
    }
}

fn mode_word(m: FileMode) -> &'static str {
    match m {
        FileMode::Regular => "regular",
        FileMode::Executable => "executable",
        FileMode::Symlink => "symlink",
        FileMode::Submodule => "submodule",
    }
}

fn file_text(f: &FileChange, out: &mut String) {
    out.push_str(&format!("file {} ({}", f.path, kind_word(f.change)));
    if let Some(old) = &f.old_path {
        out.push_str(&format!(", from {old}"));
    }
    if f.mode != FileMode::Regular {
        out.push_str(&format!(", {}", mode_word(f.mode)));
    }
    if f.binary {
        out.push_str(", binary");
    }
    out.push_str(&format!(", {} bytes)\n", f.size_after));
    for l in &f.removed {
        out.push_str(&format!("-{}: {}\n", l.no, l.text));
    }
    for l in &f.added {
        out.push_str(&format!("+{}: {}\n", l.no, l.text));
    }
}

/// Renders path, kind of change, removed lines and added lines with their numbers.
pub fn diff_text(diff: &ParsedDiff) -> String {
    let mut out = format!(
        "base {}\nhead {}\n{} files\n",
        diff.base,
        diff.head,
        diff.files.len()
    );
    for f in &diff.files {
        file_text(f, &mut out);
        if out.chars().count() > MAX_DIFF_CHARS {
            let cut: String = out.chars().take(MAX_DIFF_CHARS).collect();
            out = cut;
            out.push_str("\n[the diff is cut here, it is longer than the hub shows: escalate]\n");
            break;
        }
    }
    out
}
