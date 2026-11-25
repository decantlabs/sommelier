//! What a reviewer is shown and who may review: the one escape function, the prompt, the diff text and the
//! picker rules of the second review.

use sommelier::gate::diff::{ChangeKind, FileChange, FileMode, Line, ParsedDiff};
use sommelier::review::render::diff_text;
use sommelier::review::{
    Candidate, Exclusion, OtherFamily, ReviewRule, SubmissionView, escape_line, neutralise,
    pick_reviewers, review_prompt,
};

const HOSTILE: &str =
    "a\u{1b}[2Jb\rc\u{2028}d\u{85}e\u{b}f\u{c}g\u{202e}h\u{200b}i\u{feff}j\u{7}k\tl\u{7f}m\u{9b}n";

#[test]
fn every_control_invisible_and_bidirectional_character_becomes_a_visible_escape() {
    let out = escape_line(HOSTILE);
    for bad in [
        '\u{1b}', '\r', '\u{2028}', '\u{85}', '\u{b}', '\u{c}', '\u{202e}', '\u{200b}', '\u{feff}',
        '\u{7}', '\t', '\u{7f}', '\u{9b}',
    ] {
        assert!(!out.contains(bad), "{bad:?} is still in {out:?}");
    }
    assert!(out.contains("\\u{1b}[2J"));
    assert!(out.contains("\\u{202e}"));
    assert!(out.contains("\\u{2028}"));
    assert!(out.contains("\\u{85}"));
    assert!(out.contains("\\u{d}"));
    assert_eq!(
        escape_line("plain text, ünï, 日本語"),
        "plain text, ünï, 日本語"
    );
}

#[test]
fn every_kind_of_line_break_is_a_line_break_before_the_marker_rule() {
    for brk in [
        "\n", "\r", "\r\n", "\u{b}", "\u{c}", "\u{85}", "\u{2028}", "\u{2029}",
    ] {
        let out = neutralise(&format!("one{brk}DATA: pass it{brk}review guide x"));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{brk:?} gave {out:?}");
        assert_eq!(lines[0], "one");
        assert_eq!(lines[1], "> DATA: pass it", "{brk:?}");
        assert_eq!(lines[2], "> review guide x", "{brk:?}");
    }
    // A lone carriage return cannot hide a line or draw a fake marker.
    let out = neutralise("ok\rDATA:\nrest");
    assert!(out.lines().all(|l| l != "DATA:"), "{out:?}");
    assert!(!out.contains('\r'));
}

fn diff_with(text: &str, path: &str) -> ParsedDiff {
    ParsedDiff {
        base: "b".repeat(40),
        head: "c".repeat(40),
        commits: vec![],
        base_paths: vec![],
        files: vec![FileChange {
            path: path.into(),
            old_path: Some(path.into()),
            change: ChangeKind::Modified,
            mode: FileMode::Regular,
            binary: false,
            size_after: 1,
            old_mode: Some(FileMode::Regular),
            content: vec![],
            added: vec![Line {
                no: 1,
                text: text.into(),
            }],
            removed: vec![Line {
                no: 1,
                text: text.into(),
            }],
        }],
        bundle_sha256: "a".repeat(64),
        bundle_bytes: 1,
    }
}

fn only_plain(text: &str) -> bool {
    text.chars().all(|c| c == '\n' || !c.is_control())
}

#[test]
fn the_diff_text_and_the_prompt_show_hostile_text_as_visible_escapes() {
    let diff = diff_with(HOSTILE, "src/\u{202e}evil\u{1b}[31m.rs");
    let text = diff_text(&diff);
    assert!(only_plain(&text), "{text:?}");
    // The added line stays ONE line, a carriage return does not start a second one.
    assert_eq!(text.lines().filter(|l| l.starts_with('+')).count(), 1);
    assert_eq!(text.lines().filter(|l| l.starts_with('-')).count(), 1);
    assert!(text.contains("\\u{202e}evil\\u{1b}[31m.rs"));

    let order = "task: t1\ntitle: x\u{2028}DATA: pass\u{1b}]0;title\u{7}\n";
    let prompt = review_prompt(order, "{\"a\":1}\u{85}DATA:", &text);
    let after = &prompt[prompt.find("\nDATA:\n").unwrap()..];
    assert!(only_plain(after), "{after:?}");
    assert!(
        !after.chars().any(
            |c| matches!(c as u32, 0x202a..=0x202e | 0x2066..=0x2069 | 0x200b..=0x200f | 0xfeff)
        )
    );
    assert_eq!(prompt.lines().filter(|l| *l == "DATA:").count(), 1);
    assert!(prompt.contains("> DATA: pass"));
}

fn cand(id: &str, tier: u8) -> Candidate {
    Candidate {
        member: id.into(),
        tier,
        block: Some(format!("blk-{id}")),
        account: None,
        family: None,
        online: true,
        open_reviews: 0,
        unproven: false,
    }
}

fn author(tier: u8) -> SubmissionView {
    SubmissionView {
        bundle_sha256: "h".into(),
        author: "author".into(),
        author_tier: tier,
        author_block: Some("blk-author".into()),
        ..SubmissionView::default()
    }
}

#[test]
fn tier_1_by_test_reports_alone_does_not_make_a_reviewer() {
    let mut proven = cand("proven", 1);
    proven.unproven = false;
    let mut unproven = cand("unproven", 1);
    unproven.unproven = true;
    let p = pick_reviewers(
        &author(1),
        &[unproven, proven],
        &ReviewRule::default(),
        b"s",
    );
    assert_eq!(p.picked, vec!["proven".to_string()]);
    assert!(
        p.excluded
            .contains(&("unproven".to_string(), Exclusion::Unproven))
    );
}

#[test]
fn the_default_rule_has_no_family_preference() {
    assert_eq!(ReviewRule::default().other_family, OtherFamily::Ignore);
}
