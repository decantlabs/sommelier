//! Tests of the static gates (docs/factory.md section 8.2). All inputs are built in code.

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
        message: "change".into(),
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

fn file(path: &str, added: &[&str]) -> FileChange {
    FileChange {
        path: path.into(),
        old_path: None,
        change: ChangeKind::Modified,
        mode: FileMode::Regular,
        binary: false,
        size_after: 100,
        added: lines(added),
        removed: Vec::new(),
    }
}

fn clean_diff() -> ParsedDiff {
    ParsedDiff {
        base: BASE.into(),
        head: HEAD.into(),
        commits: vec![commit(HEAD, BASE)],
        files: vec![file(
            "src/util.rs",
            &["pub fn add(a: i32, b: i32) -> i32 { a + b }"],
        )],
        bundle_sha256: "0".repeat(64),
        bundle_bytes: 2000,
    }
}

fn order() -> OrderView {
    OrderView {
        base: BASE.into(),
        scope: vec!["src/**".into(), "docs/**".into(), "Cargo.toml".into()],
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

type Mutate = fn(&mut ParsedDiff, &mut OrderView);

/// (name, mutation, gate that must block or None when the patch must pass)
fn table() -> Vec<(&'static str, Mutate, Option<&'static str>)> {
    vec![
        ("clean patch passes", |_, _| {}, None),
        ("base differs", |d, _| d.base = "c".repeat(40), Some("base")),
        ("base equal near miss", |_, _| {}, None),
        (
            "two parents",
            |d, _| d.commits[0].parents.push("d".repeat(40)),
            Some("history"),
        ),
        (
            "chain broken",
            |d, _| {
                d.commits
                    .insert(0, commit("e".repeat(40).as_str(), "f".repeat(40).as_str()));
            },
            Some("history"),
        ),
        (
            "two commit chain passes",
            |d, _| {
                let mid = "c".repeat(40);
                d.commits = vec![commit(&mid, BASE), commit(HEAD, &mid)];
            },
            None,
        ),
        ("no commits", |d, _| d.commits.clear(), Some("history")),
        (
            "head not last commit",
            |d, _| d.head = "9".repeat(40),
            Some("history"),
        ),
        (
            "control char in author",
            |d, _| d.commits[0].author_name = "Evil\u{1b}[2J".into(),
            Some("authors"),
        ),
        (
            "email without at",
            |d, _| d.commits[0].author_email = "nobody".into(),
            Some("authors"),
        ),
        (
            "very long name",
            |d, _| d.commits[0].author_name = "n".repeat(5000),
            Some("authors"),
        ),
        (
            "unicode name passes",
            |d, _| d.commits[0].author_name = "Zoe Muller-Strasse".into(),
            None,
        ),
        (
            "dot dot path",
            |d, o| {
                d.files[0].path = "src/../x.rs".into();
                o.scope = vec!["**".into()];
            },
            Some("paths"),
        ),
        (
            "absolute path",
            |d, o| {
                d.files[0].path = "/etc/x".into();
                o.scope = vec!["**".into()];
            },
            Some("paths"),
        ),
        (
            "backslash path",
            |d, o| {
                d.files[0].path = "src\\x.rs".into();
                o.scope = vec!["**".into()];
            },
            Some("paths"),
        ),
        (
            "reserved windows name",
            |d, _| d.files[0].path = "src/aux.rs".into(),
            Some("paths"),
        ),
        (
            "trailing dot",
            |d, _| d.files[0].path = "src/x.".into(),
            Some("paths"),
        ),
        (
            "case collision",
            |d, _| d.files.push(file("src/Util.rs", &["// x"])),
            Some("paths"),
        ),
        (
            "dotted name passes",
            |d, _| d.files[0].path = "src/auxiliary.v2.rs".into(),
            None,
        ),
        (
            "outside scope",
            |d, _| d.files[0].path = "tools/x.rs".into(),
            Some("scope"),
        ),
        (
            "rename out of scope",
            |d, _| {
                d.files[0].change = ChangeKind::Renamed;
                d.files[0].old_path = Some("tools/old.rs".into());
            },
            Some("scope"),
        ),
        (
            "rename inside scope passes",
            |d, _| {
                d.files[0].change = ChangeKind::Renamed;
                d.files[0].old_path = Some("src/old.rs".into());
            },
            None,
        ),
        (
            "hold path on normal order",
            |d, o| {
                d.files[0].path = "swarm/x.sh".into();
                o.scope = vec!["swarm/**".into()];
            },
            Some("hold_paths"),
        ),
        (
            "hold path on guarded order passes",
            |d, o| {
                d.files[0].path = "docs/CLAUDE.md".into();
                o.scope = vec!["docs/**".into()];
                o.risk = Risk::Guarded;
            },
            None,
        ),
        (
            "hold path in other case",
            |d, o| {
                d.files[0].path = "Swarm/run.PS1".into();
                o.scope = vec!["**".into()];
            },
            Some("hold_paths"),
        ),
        (
            "binary file",
            |d, _| {
                d.files[0].binary = true;
                d.files[0].added.clear();
            },
            Some("entry_kinds"),
        ),
        (
            "symlink",
            |d, _| d.files[0].mode = FileMode::Symlink,
            Some("entry_kinds"),
        ),
        (
            "submodule",
            |d, _| d.files[0].mode = FileMode::Submodule,
            Some("entry_kinds"),
        ),
        (
            "gitattributes",
            |d, o| {
                d.files[0].path = "src/.gitattributes".into();
                o.scope = vec!["**".into()];
            },
            Some("entry_kinds"),
        ),
        (
            "executable text passes",
            |d, _| d.files[0].mode = FileMode::Executable,
            None,
        ),
        (
            "bundle too big for tier 0",
            |d, o| {
                o.tier = 0;
                d.bundle_bytes = 300 * 1024;
            },
            Some("size"),
        ),
        (
            "same bundle fits tier 1",
            |d, _| d.bundle_bytes = 300 * 1024,
            None,
        ),
        (
            "too many files",
            |d, o| {
                o.tier = 0;
                for i in 0..11 {
                    d.files.push(file(&format!("src/f{i}.rs"), &["// x"]));
                }
            },
            Some("size"),
        ),
        (
            "largest file too big",
            |d, _| d.files[0].size_after = 200 * 1024,
            Some("size"),
        ),
        (
            "too many changed lines",
            |d, _| d.files[0].added = lines(&vec!["// x"; 1001]),
            Some("size"),
        ),
        (
            "exactly at the line cap passes",
            |d, _| d.files[0].added = lines(&vec!["// x"; 1000]),
            None,
        ),
        (
            "manifest on normal order",
            |d, _| d.files[0].path = "Cargo.toml".into(),
            Some("manifests"),
        ),
        (
            "manifest on guarded order raises only",
            |d, o| {
                d.files[0].path = "Cargo.toml".into();
                o.risk = Risk::Guarded;
            },
            None,
        ),
        (
            "licence marker",
            |d, _| {
                let m = ["// SPDX-License-Identifier: ", "GP", "L-3.0-only"].concat();
                d.files[0].added = lines(&[&m]);
            },
            Some("licence"),
        ),
        (
            "ported from",
            |d, _| d.files[0].added = lines(&["// Ported from the other project"]),
            Some("licence"),
        ),
        (
            "foreign spdx",
            |d, _| d.files[0].added = lines(&["// SPDX-License-Identifier: Zlib"]),
            Some("licence"),
        ),
        (
            "project spdx passes",
            |d, _| d.files[0].added = lines(&["// SPDX-License-Identifier: MIT"]),
            None,
        ),
        (
            "transported near miss passes",
            |d, _| d.files[0].added = lines(&["// transported from A to B"]),
            None,
        ),
        (
            "private key header",
            |d, _| {
                let m = ["-----BEGIN ", "RSA PRIVATE", " KEY-----"].concat();
                d.files[0].added = lines(&[&m]);
            },
            Some("secrets"),
        ),
        (
            "token shape",
            |d, _| {
                let t = format!("let t = \"{}{}\";", "ghp_", "A".repeat(30));
                d.files[0].added = lines(&[&t]);
            },
            Some("secrets"),
        ),
        (
            "user folder path",
            |d, _| {
                let p = format!("let p = \"{}:\\{}\\{}\\x\";", "C", "Users", "someone");
                d.files[0].added = lines(&[&p]);
            },
            Some("secrets"),
        ),
        (
            "real looking email",
            |d, _| {
                let e = format!("// mail {}@{}.{}", "person", "mail", "org");
                d.files[0].added = lines(&[&e]);
            },
            Some("secrets"),
        ),
        (
            "example email passes",
            |d, _| d.files[0].added = lines(&["// mail someone@example.com"]),
            None,
        ),
        (
            "capability alone does not block",
            |d, _| d.files[0].added = lines(&["let c = std::process::Command::new(\"x\");"]),
            None,
        ),
        (
            "test removal alone does not block",
            |d, _| {
                d.files[0].removed = lines(&["#[test]", "assert_eq!(1, 1);"]);
            },
            None,
        ),
    ]
}

#[test]
fn table_of_patches() {
    let cases = table();
    assert!(cases.len() >= 30);
    for (name, mutate, expect) in cases {
        let (mut d, mut o) = (clean_diff(), order());
        mutate(&mut d, &mut o);
        let r = run(&d, &o);
        match expect {
            None => assert!(r.passed, "{name}: should pass, got {:?}", r.findings),
            Some(gate) => {
                assert!(!r.passed, "{name}: should block");
                assert!(
                    blocks(&r, gate),
                    "{name}: gate {gate} missing in {:?}",
                    r.findings
                );
            }
        }
    }
}

#[test]
fn clean_patch_has_no_findings() {
    let r = run(&clean_diff(), &order());
    assert!(r.passed);
    assert!(r.findings.is_empty());
    assert_eq!(r.risk_after, Risk::Normal);
    assert!(r.capability_delta.is_empty() && r.test_delta.is_empty());
}

#[test]
fn capability_hits_raise_one_step() {
    let cases: &[(&str, &str)] = &[
        (
            "let c = std::process::Command::new(\"x\");",
            "process_start",
        ),
        ("let s = TcpStream::connect(a);", "network"),
        ("std::fs::write(p, b)?;", "file_write"),
        ("unsafe { f() }", "unsafe"),
        ("extern \"C\" {", "extern_block"),
        ("#[link(name = \"z\")]", "link_attribute"),
        ("#[proc_macro_derive(Foo)]", "proc_macro"),
        ("let b = include_bytes!(\"a.bin\");", "embedded_file"),
        ("let l = libloading::Library::new(p);", "dynamic_library"),
        ("let v = std::env::var(\"X\");", "environment"),
    ];
    for (line, kind) in cases {
        let mut d = clean_diff();
        d.files[0].added = lines(&[line]);
        let mut o = order();
        o.risk = Risk::Low;
        let r = run(&d, &o);
        assert!(r.passed, "{kind}: raises, never blocks");
        assert_eq!(r.risk_after, Risk::Normal, "{kind}");
        assert!(
            r.capability_delta
                .iter()
                .any(|h| h.kind == *kind && h.line == 1),
            "{kind}"
        );
        o.risk = Risk::Normal;
        assert_eq!(run(&d, &o).risk_after, Risk::Guarded, "{kind}");
    }
}

#[test]
fn capability_only_on_added_lines() {
    let mut d = clean_diff();
    d.files[0].added = lines(&["// nothing"]);
    d.files[0].removed = lines(&["unsafe { f() }", "let v = std::env::var(\"X\");"]);
    let r = run(&d, &order());
    assert!(r.capability_delta.is_empty());
    assert_eq!(r.risk_after, Risk::Normal);
}

#[test]
fn build_script_is_a_capability() {
    let mut d = clean_diff();
    d.files[0].path = "src/build.rs".into();
    let mut o = order();
    o.risk = Risk::Guarded;
    let r = run(&d, &o);
    assert!(r.capability_delta.iter().any(|h| h.kind == "build_script"));
}

#[test]
fn manifest_forces_guarded_risk() {
    let mut d = clean_diff();
    d.files[0].path = "Cargo.toml".into();
    let r = run(&d, &order());
    assert_eq!(r.risk_after, Risk::Guarded);
    let mut o = order();
    o.risk = Risk::Guarded;
    let r = run(&d, &o);
    assert!(r.passed);
    assert_eq!(r.risk_after, Risk::Guarded);
    assert!(
        r.findings
            .iter()
            .any(|f| f.gate == "manifests" && f.severity == Severity::Raise)
    );
}

#[test]
fn test_delta_is_listed_as_notes() {
    let mut d = clean_diff();
    d.files[0].removed = lines(&["#[test]", "    assert_eq!(a, b);", "let x = 1;"]);
    d.files[0].added = lines(&["#[ignore]"]);
    let mut gone = file("tests/old.rs", &[]);
    gone.change = ChangeKind::Deleted;
    gone.size_after = 0;
    d.files.push(gone);
    let mut o = order();
    o.scope.push("tests/**".into());
    let r = run(&d, &o);
    assert!(r.passed);
    let kinds: Vec<&str> = r.test_delta.iter().map(|h| h.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "removed_test_attribute",
            "removed_assertion",
            "added_ignore_attribute",
            "deleted_test_file"
        ]
    );
    assert_eq!(kinds.len(), 4);
    assert!(
        r.findings
            .iter()
            .filter(|f| f.gate == "tests")
            .all(|f| f.severity == Severity::Note)
    );
}

#[test]
fn secret_finding_does_not_repeat_the_secret() {
    let secret = format!("{}{}", "ghp_", "Zq9".repeat(12));
    let mail = format!("{}@{}.{}", "private.person", "mail", "org");
    let mut d = clean_diff();
    d.files[0].added = lines(&[&format!("let t = \"{secret}\"; // {mail}")]);
    let r = run(&d, &order());
    assert!(!r.passed);
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains(&secret));
    assert!(!json.contains(&mail));
    assert!(!json.contains("private.person"));
    let found: Vec<_> = r.findings.iter().filter(|f| f.gate == "secrets").collect();
    assert_eq!(found.len(), 2);
    assert!(found.iter().all(|f| f.line == Some(1)));
}

#[test]
fn same_input_gives_equal_json() {
    let mut d = clean_diff();
    d.files[0].added = lines(&["unsafe { f() }", "let v = std::env::var(\"X\");"]);
    d.files[0].removed = lines(&["#[test]"]);
    d.files.push(file("tools/x.rs", &["// x"]));
    let a = serde_json::to_string(&run(&d, &order())).unwrap();
    let b = serde_json::to_string(&run(&d, &order())).unwrap();
    assert_eq!(a, b);
}

#[test]
fn hostile_strings_do_not_panic() {
    let mut d = clean_diff();
    let long = "x".repeat(300_000);
    let nasty = [
        "../../..",
        "",
        "/",
        "//",
        "\0",
        "a\u{7}b",
        "C:",
        "CON",
        "con.txt",
        "a/./b",
        "a//b",
        "\u{202e}rtl",
        "src/\u{1b}[31m",
        " ",
        ".",
        "..",
        "a/..",
        "\\\\?\\C:\\x",
        &long,
    ];
    for (i, p) in nasty.iter().enumerate() {
        let mut f = file(
            p,
            &[&long, "\u{0}\u{1b}[0m", "unsafe unsafe unsafe", "@@@@@@"],
        );
        f.old_path = Some((*p).into());
        f.removed = lines(&[&long, "#[test]"]);
        if i % 2 == 0 {
            f.change = ChangeKind::Renamed;
        }
        d.files.push(f);
    }
    d.commits[0].author_name = long.clone();
    d.commits[0].author_email = "\u{0}@\u{0}@".into();
    d.commits.push(CommitInfo {
        sha: String::new(),
        parents: Vec::new(),
        author_name: String::new(),
        author_email: String::new(),
        message: long.clone(),
    });
    for risk in [Risk::Low, Risk::Normal, Risk::Guarded] {
        for tier in [0u8, 1, 2, 3, 200] {
            let o = OrderView {
                base: long.clone(),
                scope: vec![long.clone(), "**".into(), "[".into(), String::new()],
                risk,
                tier,
            };
            let r = run(&d, &o);
            assert!(!r.passed);
            // Findings stay short and free of control characters.
            for f in &r.findings {
                assert!(f.text.chars().count() < 400);
                assert!(
                    f.path
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .all(|c| !c.is_control())
                );
            }
        }
    }
}

#[test]
fn findings_per_gate_are_capped() {
    let mut d = clean_diff();
    d.files[0].added = lines(&vec!["unsafe { f() }"; 500]);
    let r = run(&d, &order());
    let n = r.findings.iter().filter(|f| f.gate == "capability").count();
    assert_eq!(n, 101);
    assert!(r.capability_delta.len() <= 100);
    assert_eq!(r.risk_after, Risk::Guarded);
}

#[test]
fn broken_rule_pattern_blocks() {
    let mut rules = GateRules::default();
    rules.secrets[0].pattern = "(".into();
    let r = check(&clean_diff(), &order(), &rules);
    assert!(!r.passed);
    assert!(blocks(&r, "rules"));
}

#[test]
fn single_gates_are_callable() {
    let d = clean_diff();
    let o = order();
    let rules = GateRules::default();
    assert!(gates::base(&d, &o).is_empty());
    assert!(gates::history(&d).is_empty());
    assert!(gates::authors(&d, &rules).is_empty());
    assert!(gates::paths(&d).is_empty());
    assert!(gates::scope(&d, &o).is_empty());
    assert!(gates::hold_paths(&d, &o, &rules).is_empty());
    assert!(gates::entry_kinds(&d).is_empty());
    assert!(gates::size(&d, &o, &rules).is_empty());
    assert!(gates::manifests(&d, &o, &rules).is_empty());
    assert!(gates::licence(&d, &rules).is_empty());
    assert!(gates::secrets(&d, &rules).is_empty());
}

#[test]
fn report_round_trips_through_json() {
    let r = run(&clean_diff(), &order());
    let back: GateReport = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    assert_eq!(r, back);
}
