//! The diff of a submission as plain text for the reviewer. Pure.

use super::prompt::escape_line;
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
    out.push_str(&format!(
        "file {} ({}",
        escape_line(&f.path),
        kind_word(f.change)
    ));
    if let Some(old) = &f.old_path {
        out.push_str(&format!(", from {}", escape_line(old)));
    }
    if f.mode != FileMode::Regular {
        out.push_str(&format!(", {}", mode_word(f.mode)));
    }
    if f.binary {
        out.push_str(", binary");
    }
    out.push_str(&format!(", {} bytes)\n", f.size_after));
    for l in &f.removed {
        out.push_str(&format!("-{}: {}\n", l.no, escape_line(&l.text)));
    }
    for l in &f.added {
        out.push_str(&format!("+{}: {}\n", l.no, escape_line(&l.text)));
    }
}

/// Renders path, kind of change, removed lines and added lines with their numbers.
pub fn diff_text(diff: &ParsedDiff) -> String {
    let mut out = format!(
        "base {}\nhead {}\n{} files\n",
        escape_line(&diff.base),
        escape_line(&diff.head),
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

/// The order text of a task for the reviewer: title, scope and done condition. When the order names a
/// feature the text starts with the feature id, its title and its `how` from the feature map. Every
/// piece passes through [`escape_line`] like the rest of the prompt. An order without a feature reads
/// exactly as it always did.
pub fn order_text(t: &crate::task::TaskRow, map: Option<&crate::feature::FeatureMap>) -> String {
    let mut out = String::new();
    if let Some(id) = t.feature.as_deref().filter(|f| !f.is_empty()) {
        out.push_str(&format!("feature: {}", escape_line(id)));
        if let Some(f) = map.and_then(|m| m.get(id)) {
            out.push_str(&format!(" ({})", escape_line(&f.title)));
            out.push('\n');
            if let Some(how) = &f.how {
                out.push_str(&format!("how to reach it: {}\n", escape_line(how)));
            }
        } else {
            out.push('\n');
        }
    }
    out.push_str(&format!(
        "task: {}\ntitle: {}\nscope: {}\ndone when: {}\n",
        escape_line(&t.id),
        escape_line(&t.title),
        escape_line(&t.scope),
        escape_line(&t.done_when)
    ));
    out
}

#[cfg(test)]
mod order_text_tests {
    use super::*;
    use crate::feature::FeatureMap;
    use crate::task::TaskRow;

    fn map() -> FeatureMap {
        FeatureMap::parse(
            "version = 1\n[[area]]\nid = \"g\"\ntitle = \"G\"\n[[feature]]\nid = \"g.one\"\ntitle = \"Frames reach the window\"\narea = \"g\"\npaths = [\"a/**\"]\nhow = \"Start it.\"\n",
        )
        .unwrap()
    }

    #[test]
    fn an_order_without_a_feature_reads_as_before() {
        let t = TaskRow {
            id: "t1".into(),
            title: "T".into(),
            scope: "a/**".into(),
            done_when: "done".into(),
            ..TaskRow::default()
        };
        assert_eq!(
            order_text(&t, None),
            "task: t1\ntitle: T\nscope: a/**\ndone when: done\n"
        );
    }

    #[test]
    fn the_feature_comes_first_and_is_escaped() {
        let t = TaskRow {
            id: "t1".into(),
            title: "T".into(),
            feature: Some("g.one".into()),
            ..TaskRow::default()
        };
        let text = order_text(&t, Some(&map()));
        assert!(
            text.starts_with(
                "feature: g.one (Frames reach the window)\nhow to reach it: Start it.\ntask: t1\n"
            ),
            "{text}"
        );
        // An id that is not in the map (the map changed) is still shown, escaped, and nothing more.
        let odd = TaskRow {
            feature: Some("g.\u{202e}x".into()),
            ..t
        };
        let text = order_text(&odd, Some(&map()));
        assert!(text.starts_with("feature: g.\\u{202e}x\ntask:"), "{text}");
    }
}
