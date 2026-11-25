//! One test per finding of the security review of the bundle and gate code. Each test fails on the
//! code from before the fix. The tests that need a real bundle are in `bundle.rs`.

use sommelier::bundle::clip_text;
use sommelier::bundle::parse::parse_raw;
use sommelier::gate::diff::{ChangeKind, CommitInfo, FileChange, FileMode, Line, ParsedDiff};
use sommelier::gate::{GateReport, GateRules, OrderView, Risk, Severity, check, gates};

const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn commit(sha: &str, parent: &str) -> CommitInfo {
    CommitInfo {
        sha: sha.into(),
        parents: vec![parent.into()],
        author_name: "Test Author".into(),
        author_email: "author@example.com".into(),
        committer_name: "Test Author".into(),
        committer_email: "author@example.com".into(),
        message: "change".into(),
        paths: vec!["src/util.rs".into()],
    }
}

fn lines(texts: &[&str]) -> Vec<Line> {
    texts
        .iter()
        .enumerate()
        .map(|(i, t)| Line {
            no: i as u32 + 1,
            text: (*t).into(),
        })
        .collect()
}

fn file(path: &str, change: ChangeKind, added: &[&str]) -> FileChange {
    FileChange {
        path: path.into(),
        old_path: None,
        change,
        mode: FileMode::Regular,
        binary: false,
        size_after: 100,
        added: lines(added),
        removed: Vec::new(),
        old_mode: None,
        content: Vec::new(),
    }
}

fn clean() -> ParsedDiff {
    ParsedDiff {
        base: BASE.into(),
        head: HEAD.into(),
        commits: vec![commit(HEAD, BASE)],
        files: vec![file(
            "src/util.rs",
            ChangeKind::Modified,
            &["pub fn add(a: i32, b: i32) -> i32 { a + b }"],
        )],
        bundle_sha256: "0".repeat(64),
        bundle_bytes: 2000,
        base_paths: vec!["src/util.rs".into()],
    }
}

fn order() -> OrderView {
    OrderView {
        base: BASE.into(),
        scope: vec!["src/**".into()],
        risk: Risk::Normal,
        tier: 1,
    }
}

fn run(d: &ParsedDiff, o: &OrderView) -> GateReport {
    check(d, o, &GateRules::default())
}

fn blocks(r: &GateReport, gate: &str) -> bool {
    r.findings
        .iter()
        .any(|f| f.gate == gate && f.severity == Severity::Block)
}

#[test]
fn the_clean_fixture_passes() {
    let r = run(&clean(), &order());
    assert!(r.passed, "{:?}", r.findings);
    assert!(r.findings.is_empty());
}

// F1: commits between base and head.

#[test]
fn f1_a_middle_commit_cannot_touch_what_the_end_diff_may_not() {
    // Commit 1 adds a script and a hold path, commit 2 removes them. The end diff is clean.
    let cases: [(&str, &str); 3] = [
        ("tools/x.rs", "scope"),
        ("src/run.sh", "hold_paths"),
        ("src/.gitattributes", "entry_kinds"),
    ];
    for (path, gate) in cases {
        let mut d = clean();
        let mid = "c".repeat(40);
        let mut first = commit(&mid, BASE);
        first.paths = vec![path.into()];
        let mut second = commit(HEAD, &mid);
        second.paths = vec![path.into(), "src/util.rs".into()];
        d.commits = vec![first, second];
        let r = run(&d, &order());
        assert!(!r.passed, "{path}");
        assert!(blocks(&r, gate), "{path} should block gate {gate}");
        assert!(
            r.findings.iter().any(|f| f.path.as_deref() == Some(path)),
            "{path}"
        );
    }
}

#[test]
fn f1_a_middle_commit_with_a_bad_path_is_blocked() {
    let mut d = clean();
    d.commits[0].paths.push("src/CARGO~1.TOM".into());
    d.commits[0].paths.push("src/../x".into());
    let r = run(&d, &order());
    assert!(blocks(&r, "paths"));
    assert!(r.findings.iter().filter(|f| f.gate == "paths").count() >= 2);
}

#[test]
fn f1_paths_of_the_end_diff_are_not_reported_twice() {
    let mut d = clean();
    d.commits[0].paths = vec!["tools/x.rs".into()];
    d.files[0].path = "tools/x.rs".into();
    let r = run(&d, &order());
    let n = r.findings.iter().filter(|f| f.gate == "scope").count();
    assert_eq!(n, 1, "{:?}", r.findings);
}

#[test]
fn f1_committer_and_message_are_gated_like_author_fields() {
    type Mutate = fn(&mut CommitInfo);
    let bad: [(&str, Mutate); 5] = [
        ("committer name", |c| {
            c.committer_name = "Ev\u{1b}[2J".into()
        }),
        ("committer email", |c| c.committer_email = "nobody".into()),
        ("committer name hidden", |c| {
            c.committer_name = "Zo\u{200C}e".into()
        }),
        ("message control", |c| c.message = "fix\u{7}".into()),
        ("message too long", |c| c.message = "m".repeat(20_000)),
    ];
    for (name, mutate) in bad {
        let mut d = clean();
        mutate(&mut d.commits[0]);
        let r = run(&d, &order());
        assert!(blocks(&r, "authors"), "{name}: {:?}", r.findings);
    }
    // Line breaks and tabs are fine in a message.
    let mut d = clean();
    d.commits[0].message = "title\n\n\tbody line\r\nmore".into();
    assert!(run(&d, &order()).passed);
}

#[test]
fn f1_secrets_in_commit_text_are_found_and_not_repeated() {
    let token = format!("{}{}", "ghp_", "Zq9".repeat(12));
    for place in 0..3 {
        let mut d = clean();
        match place {
            0 => d.commits[0].message = format!("add it\n\ntoken {token}"),
            1 => d.commits[0].committer_name = format!("n {token}"),
            _ => d.commits[0].author_name = format!("n {token}"),
        }
        let r = run(&d, &order());
        assert!(blocks(&r, "secrets"), "place {place}: {:?}", r.findings);
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains(&token));
    }
    // An address in the message is expected, for example a trailer.
    let mut d = clean();
    d.commits[0].message = "x\n\nCo-Authored-By: Name <someone@mail.org>".into();
    assert!(!blocks(&run(&d, &order()), "secrets"));
}

// F3: hold list.

#[test]
fn f3_files_that_execute_are_hold_paths() {
    let paths = [
        "crates/a/.cargo/config.toml",
        ".cargo/config.toml",
        "rust-toolchain",
        "sub/rust-toolchain.toml",
        "sub/.claude/settings.json",
        "docs/GEMINI.md",
        "x/.gemini/settings.json",
        ".codex/config.toml",
        "a/.mcp.json",
        "a/.vscode/tasks.json",
        "tools/x.bat",
        "tools/x.cmd",
        "tools/x.py",
    ];
    for p in paths {
        let mut d = clean();
        d.files[0].path = p.into();
        d.commits[0].paths = vec![p.into()];
        let mut o = order();
        o.scope = vec!["**".into()];
        let r = run(&d, &o);
        assert!(blocks(&r, "hold_paths"), "{p}: {:?}", r.findings);
        o.risk = Risk::Guarded;
        assert!(!blocks(&run(&d, &o), "hold_paths"), "{p} on guarded");
    }
}

#[test]
fn f3_a_file_that_becomes_executable_raises_the_risk() {
    let mut d = clean();
    d.files[0].mode = FileMode::Executable;
    d.files[0].old_mode = Some(FileMode::Regular);
    let mut o = order();
    o.risk = Risk::Low;
    let r = run(&d, &o);
    assert!(r.passed);
    assert_eq!(r.risk_after, Risk::Normal);
    assert!(
        r.capability_delta
            .iter()
            .any(|h| h.kind == "executable_bit")
    );
    // A file that was executable before does not raise.
    d.files[0].old_mode = Some(FileMode::Executable);
    let r = run(&d, &o);
    assert_eq!(r.risk_after, Risk::Low);
    // A new executable file raises.
    d.files[0].old_mode = None;
    d.files[0].change = ChangeKind::Added;
    assert_eq!(run(&d, &o).risk_after, Risk::Normal);
}

// F4: path aliasing.

#[test]
fn f4_a_new_path_uses_the_safe_characters_only() {
    for p in [
        "src/CARGO~1.TOM",
        "src/caf\u{e9}.rs",
        "src/a$b.rs",
        "src/a&b.rs",
        "src/\u{131}.rs",
    ] {
        let mut d = clean();
        d.files[0].path = p.into();
        d.files[0].change = ChangeKind::Added;
        let r = run(&d, &order());
        assert!(blocks(&r, "paths"), "{p}");
    }
    for p in ["src/a b-c_d.v2.rs", "src/A/B.rs"] {
        let mut d = clean();
        d.files[0].path = p.into();
        d.files[0].change = ChangeKind::Added;
        assert!(run(&d, &order()).passed, "{p}");
    }
    // A path that already exists in the base keeps its name when it is only modified.
    let mut d = clean();
    d.files[0].path = "src/caf\u{e9}.rs".into();
    d.commits[0].paths = vec!["src/caf\u{e9}.rs".into()];
    d.base_paths = vec!["src/caf\u{e9}.rs".into()];
    assert!(run(&d, &order()).passed);
    // The rules can allow more.
    let mut d = clean();
    d.files[0].path = "src/caf\u{e9}.rs".into();
    d.files[0].change = ChangeKind::Added;
    d.commits[0].paths = vec!["src/caf\u{e9}.rs".into()];
    let rules = GateRules {
        extra_path_chars: "\u{e9}".into(),
        ..GateRules::default()
    };
    assert!(check(&d, &order(), &rules).passed);
}

#[test]
fn f4_a_new_path_may_not_differ_only_in_case_from_a_base_path() {
    let mut d = clean();
    d.base_paths = vec!["src/Util.rs".into(), "src/other.rs".into()];
    d.files[0].path = "src/util.rs".into();
    d.files[0].change = ChangeKind::Added;
    d.commits[0].paths = vec!["src/util.rs".into()];
    let r = run(&d, &order());
    assert!(blocks(&r, "paths"), "{:?}", r.findings);

    // A folder collides too: `Docs` and `docs` are one folder on Windows and macOS.
    let mut d = clean();
    d.base_paths = vec!["src/Sub/a.rs".into()];
    d.files[0].path = "src/sub/b.rs".into();
    d.files[0].change = ChangeKind::Added;
    d.commits[0].paths = vec!["src/sub/b.rs".into()];
    assert!(blocks(&run(&d, &order()), "paths"));

    // Moving a base path to another case is a rename and fine.
    let mut d = clean();
    d.base_paths = vec!["src/Util.rs".into()];
    d.files[0].path = "src/util.rs".into();
    d.files[0].old_path = Some("src/Util.rs".into());
    d.files[0].change = ChangeKind::Renamed;
    d.commits[0].paths = vec!["src/util.rs".into(), "src/Util.rs".into()];
    assert!(run(&d, &order()).passed);

    // Folding goes further than `to_lowercase`.
    assert_eq!(gates::fold_path("\u{17f}"), gates::fold_path("s"));
    assert_eq!(gates::fold_path("\u{131}"), gates::fold_path("i"));
}

// F5: backslash.

#[test]
fn f5_a_backslash_stays_in_the_path_and_the_path_gate_blocks_it() {
    let raw = b":000000 100644 0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 A\0src\\evil.rs\0";
    let parsed = parse_raw(raw).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].path, "src\\evil.rs");
    let mut d = clean();
    d.files[0].path = parsed[0].path.clone();
    d.files[0].change = ChangeKind::Added;
    d.commits[0].paths = vec![parsed[0].path.clone()];
    // The scope `src/**` does not match a root file with a backslash in its name.
    let r = run(&d, &order());
    assert!(blocks(&r, "paths"));
    assert!(blocks(&r, "scope"));
}

// F6: capability patterns.

#[test]
fn f6_more_capabilities_are_listed() {
    let cases: &[(&str, &str)] = &[
        (
            "let f = File::options().append(true).open(p);",
            "file_write",
        ),
        ("std::fs::hard_link(a, b)?;", "file_link"),
        ("std::os::unix::fs::symlink(a, b)?;", "file_link"),
        ("std::fs::set_permissions(p, perms)?;", "file_permissions"),
        ("use std::os::unix::fs::PermissionsExt;", "file_permissions"),
        ("let f = File::open(p)?;", "file_read"),
        ("let s = fs::read_to_string(p)?;", "file_read"),
        ("let b = std::fs::read(p)?;", "file_read"),
        ("core::arch::global_asm!(\"nop\");", "inline_asm"),
        ("unsafe { asm!(\"nop\") }", "inline_asm"),
        ("build = \"tools/gen.rs\"", "build_script"),
    ];
    for (line, kind) in cases {
        let mut d = clean();
        d.files[0].added = lines(&[line]);
        let mut o = order();
        o.risk = Risk::Low;
        let r = run(&d, &o);
        assert!(r.passed, "{kind}");
        assert!(
            r.capability_delta.iter().any(|h| h.kind == *kind),
            "{kind} missing for {line}"
        );
        assert_eq!(r.risk_after, Risk::Normal, "{kind}");
    }
    // Near misses stay quiet.
    for line in [
        "let build = false;",
        "build = false",
        "// reads the file",
        "fn asm_like() {}",
    ] {
        let mut d = clean();
        d.files[0].added = lines(&[line]);
        let r = run(&d, &order());
        assert!(
            r.capability_delta.is_empty(),
            "{line}: {:?}",
            r.capability_delta
        );
    }
}

#[test]
fn f6_a_pure_rename_counts_the_whole_new_content() {
    let mut d = clean();
    let mut f = file("src/moved.rs", ChangeKind::Renamed, &[]);
    f.old_path = Some("src/util.rs".into());
    f.content = lines(&["fn a() {}", "unsafe { b() }"]);
    d.files = vec![f];
    let r = run(&d, &order());
    assert!(
        r.capability_delta
            .iter()
            .any(|h| h.kind == "unsafe" && h.line == 2),
        "{:?}",
        r.capability_delta
    );
    assert_eq!(r.risk_after, Risk::Guarded);
}

#[test]
fn f6_many_hits_raise_one_step_in_total() {
    let mut d = clean();
    d.files[0].added = lines(&["unsafe { f() }"; 50]);
    let mut o = order();
    o.risk = Risk::Low;
    let r = run(&d, &o);
    assert_eq!(r.risk_after, Risk::Normal);
}

// F7: padding.

#[test]
fn f7_padding_cannot_hide_a_dangerous_hit() {
    let mut text: Vec<&str> = vec!["unsafe { f() }"; 300];
    text.push("let c = std::process::Command::new(\"x\");");
    let mut d = clean();
    d.files[0].added = lines(&text);
    let r = run(&d, &order());
    assert!(r.capability_delta.len() <= 110);
    assert!(r.capability_delta.iter().any(|h| h.kind == "process_start"));
    assert!(
        r.findings
            .iter()
            .any(|f| f.gate == "capability" && f.text.contains("process_start"))
    );
    assert_eq!(r.capability_counts.get("unsafe"), Some(&300));
    assert_eq!(r.capability_counts.get("process_start"), Some(&1));
    let json = serde_json::to_string(&r).unwrap();
    let back: GateReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back, r);
}

#[test]
fn f7_the_test_delta_keeps_every_kind_too() {
    let mut d = clean();
    d.files[0].removed = lines(&vec!["assert_eq!(a, b);"; 300]);
    d.files[0].removed.push(Line {
        no: 999,
        text: "#[test]".into(),
    });
    let r = run(&d, &order());
    assert!(
        r.test_delta
            .iter()
            .any(|h| h.kind == "removed_test_attribute")
    );
    assert_eq!(r.test_counts.get("removed_assertion"), Some(&300));
}

// F8: hidden characters and git text.

#[test]
fn f8_invisible_characters_are_refused_like_control_characters() {
    for c in [
        '\u{200C}', '\u{200B}', '\u{202E}', '\u{FEFF}', '\u{2066}', '\u{AD}',
    ] {
        let mut d = clean();
        d.commits[0].author_name = format!("Zo{c}e");
        assert!(blocks(&run(&d, &order()), "authors"), "{c:?} in a name");
        let mut d = clean();
        let p = format!("src/a{c}b.rs");
        assert!(gates::path_problem(&p).is_some(), "{c:?} in a path");
        d.files[0].path = p;
        assert!(blocks(&run(&d, &order()), "paths"));
    }
    // Findings never show them.
    let mut d = clean();
    d.files[0].path = "src/a\u{202E}b.rs".into();
    let r = run(&d, &order());
    for f in &r.findings {
        assert!(
            f.path
                .as_deref()
                .unwrap_or("")
                .chars()
                .all(|c| !gates::is_invisible(c))
        );
    }
}

#[test]
fn f8_git_text_in_an_error_is_short_clean_and_without_folders() {
    let hostile = format!(
        "fatal: bad object in /work/box/secret-folder/bundle.x\u{1b}[2J\u{202E}{}\nsecond line",
        "x".repeat(5000)
    );
    let scrub = vec![("/work/box/secret-folder".to_string(), "<sandbox>")];
    let t = clip_text(&hostile, &scrub);
    assert!(t.chars().count() <= 200);
    assert!(!t.contains("secret-folder"));
    assert!(t.contains("<sandbox>"));
    assert!(!t.contains("second line"));
    assert!(
        t.chars()
            .all(|c| !c.is_control() && !gates::is_invisible(c))
    );
}

// Second review, finding 2: more files that tools read are on the hold list.

#[test]
fn r2_2_more_instruction_and_tool_settings_files_are_held() {
    for path in [
        "AGENTS.override.md",
        "CLAUDE.local.md",
        ".cursor/rules/a.mdc",
        ".cursorrules",
        ".windsurfrules",
        ".clinerules",
        ".devcontainer/devcontainer.json",
        ".envrc",
        "rustfmt.toml",
        ".rustfmt.toml",
        "clippy.toml",
        ".clippy.toml",
        "deny.toml",
        "src/deny.toml",
    ] {
        let mut d = clean();
        d.files = vec![file(path, ChangeKind::Added, &["x"])];
        d.commits[0].paths = vec![path.into()];
        let mut o = order();
        o.scope = vec!["**".into()];
        let r = run(&d, &o);
        assert!(blocks(&r, "hold_paths"), "{path} should be held");
    }
}

// Second review, finding 4: deep and long paths are refused before any other gate runs.

#[test]
fn r2_4_a_deep_or_long_path_stops_the_run_early() {
    let deep = format!("src/{}f.rs", "d/".repeat(40));
    let long = format!("src/{}.rs", "a".repeat(1100));
    for path in [deep, long] {
        let mut d = clean();
        d.files = vec![file(&path, ChangeKind::Added, &["x"])];
        let r = run(&d, &order());
        assert!(!r.passed);
        assert!(blocks(&r, "paths"));
        assert!(
            r.findings.iter().all(|f| f.gate == "paths"),
            "{:?}",
            r.findings
        );
        assert!(
            r.findings
                .iter()
                .all(|f| f.path.as_ref().is_none_or(|p| p.len() < 400))
        );
    }
    // A middle commit with a deep path is refused too.
    let mut d = clean();
    d.commits[0]
        .paths
        .push(format!("src/{}f.rs", "d/".repeat(40)));
    assert!(blocks(&run(&d, &order()), "paths"));
}

#[test]
fn r2_4_many_files_under_a_deep_folder_cost_little() {
    let mut d = clean();
    let folder = "d/".repeat(2000);
    d.files = (0..500)
        .map(|i| file(&format!("src/{folder}f{i}.rs"), ChangeKind::Added, &["x"]))
        .collect();
    let t = std::time::Instant::now();
    let r = run(&d, &order());
    assert!(!r.passed);
    assert!(t.elapsed().as_secs() < 2, "took {:?}", t.elapsed());
}

#[test]
fn r2_4_a_path_at_the_limit_is_fine() {
    let ok = format!("src/{}f.rs", "d/".repeat(30));
    let mut d = clean();
    d.files = vec![file(&ok, ChangeKind::Added, &["x"])];
    let r = run(&d, &order());
    assert!(!blocks(&r, "paths"), "{:?}", r.findings);
}
