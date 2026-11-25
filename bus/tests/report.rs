//! Tests of the report module: privacy, validation, machine keys, quorum, regressions, projection.

use std::collections::BTreeMap;

use sommelier::msg::{Kind, Message, Role};
use sommelier::report::types::{MachineClass, Outcome, Report};
use sommelier::report::{
    Cell, CellState, QuorumRules, ReportLimits, ReportView, Verdict, cell, classify, find_private,
    machine_key, query_reports, regressions, scrub, validate_report,
};
use sommelier::store::Store;

// ---------- fixtures ----------

fn machine(os: &str, gpu: &str) -> MachineClass {
    MachineClass {
        os: os.into(),
        os_build: "26100".into(),
        gpu: gpu.into(),
        gpu_memory_mb: 16384,
        driver: "552.22".into(),
        memory_mb: 32768,
    }
}

fn report(run: &str, member: &str, outcome: Outcome) -> Report {
    let mut metrics = BTreeMap::new();
    metrics.insert("fps_median".to_string(), 58.5);
    metrics.insert("frames".to_string(), 1200.0);
    Report {
        v: 1,
        run: run.into(),
        member: member.into(),
        build: "abc1234".into(),
        channel: "main".into(),
        profile: "title-a".into(),
        scenario: "boot".into(),
        machine: machine("windows", "NVIDIA GeForce RTX 4080"),
        outcome,
        first_error: None,
        metrics,
        secs_alive: 120,
        log_excerpt: "frame 100 presented\nframe 200 presented\n".into(),
        at: 1_000,
        sig: None,
    }
}

fn view(member: &str, block: Option<&str>, outcome: Outcome, at: u64) -> ReportView {
    ReportView {
        member: member.into(),
        tier: 0,
        block: block.map(str::to_string),
        outcome,
        at,
    }
}

fn rules() -> QuorumRules {
    QuorumRules::default()
}

// ---------- cell ----------

use Outcome::{Fails as F, Inconclusive as I, Works as W};

/// (name, reports as (member, block, outcome, at), expected state, works, fails, need_more)
type Case = (
    &'static str,
    Vec<(&'static str, Option<&'static str>, Outcome, u64)>,
    CellState,
    usize,
    usize,
    usize,
);

fn cell_cases() -> Vec<Case> {
    use CellState::*;
    vec![
        ("empty", vec![], Unknown, 0, 0, 3),
        (
            "one works",
            vec![("a", Some("1"), W, 1)],
            HintWorks,
            1,
            0,
            2,
        ),
        (
            "one fails",
            vec![("a", Some("1"), F, 1)],
            HintFails,
            0,
            1,
            2,
        ),
        (
            "two works two blocks",
            vec![("a", Some("1"), W, 1), ("b", Some("2"), W, 2)],
            HintWorks,
            2,
            0,
            1,
        ),
        (
            "three works three blocks",
            vec![
                ("a", Some("1"), W, 1),
                ("b", Some("2"), W, 2),
                ("c", Some("3"), W, 3),
            ],
            Works,
            3,
            0,
            0,
        ),
        (
            "three fails three blocks",
            vec![
                ("a", Some("1"), F, 1),
                ("b", Some("2"), F, 2),
                ("c", Some("3"), F, 3),
            ],
            Fails,
            0,
            3,
            0,
        ),
        (
            "two liars in one block cannot settle",
            vec![
                ("liar1", Some("x"), F, 1),
                ("liar2", Some("x"), F, 2),
                ("a", Some("1"), F, 3),
            ],
            HintFails,
            0,
            2,
            1,
        ),
        (
            "five keys in one block count once",
            vec![
                ("k1", Some("x"), F, 1),
                ("k2", Some("x"), F, 2),
                ("k3", Some("x"), F, 3),
                ("k4", Some("x"), F, 4),
                ("k5", Some("x"), F, 5),
            ],
            HintFails,
            0,
            1,
            2,
        ),
        (
            "three owners in three blocks settle",
            vec![
                ("a", Some("1"), F, 1),
                ("b", Some("2"), F, 2),
                ("c", Some("3"), F, 3),
                ("liar", Some("3"), W, 0),
            ],
            Fails,
            0,
            3,
            0,
        ),
        (
            "inconclusive is ignored",
            vec![
                ("a", Some("1"), W, 1),
                ("b", Some("2"), W, 2),
                ("c", Some("3"), I, 3),
            ],
            HintWorks,
            2,
            0,
            1,
        ),
        (
            "only inconclusive stays unknown",
            vec![("a", Some("1"), I, 1), ("b", Some("2"), I, 2)],
            Unknown,
            0,
            0,
            3,
        ),
        (
            "newest of a member wins",
            vec![
                ("a", Some("1"), W, 1),
                ("a", Some("1"), F, 5),
                ("b", Some("2"), F, 2),
                ("c", Some("3"), F, 3),
            ],
            Fails,
            0,
            3,
            0,
        ),
        (
            "older report of a member does not count",
            vec![
                ("a", Some("1"), F, 9),
                ("a", Some("1"), W, 1),
                ("b", Some("2"), W, 2),
            ],
            Unknown,
            1,
            1,
            2,
        ),
        (
            "member with a newer inconclusive keeps the older real report",
            vec![
                ("a", Some("1"), W, 1),
                ("a", Some("1"), I, 5),
                ("b", Some("2"), W, 2),
                ("c", Some("3"), W, 3),
            ],
            Works,
            3,
            0,
            0,
        ),
        (
            "missing block is its own block",
            vec![("a", None, W, 1), ("b", None, W, 2), ("c", None, W, 3)],
            Works,
            3,
            0,
            0,
        ),
        (
            "missing block does not merge with a named one",
            vec![("a", None, W, 1), ("b", Some("none"), W, 2)],
            HintWorks,
            2,
            0,
            1,
        ),
        (
            "mixed when both sides reach the quorum",
            vec![
                ("a", Some("1"), W, 1),
                ("b", Some("2"), W, 2),
                ("c", Some("3"), W, 3),
                ("d", Some("4"), F, 4),
                ("e", Some("5"), F, 5),
                ("f", Some("6"), F, 6),
            ],
            Mixed,
            3,
            3,
            0,
        ),
        (
            "works settled while fails is below the quorum",
            vec![
                ("a", Some("1"), W, 1),
                ("b", Some("2"), W, 2),
                ("c", Some("3"), W, 3),
                ("d", Some("4"), F, 4),
            ],
            Works,
            3,
            1,
            0,
        ),
        (
            "tie below quorum is unknown",
            vec![("a", Some("1"), W, 1), ("b", Some("2"), F, 2)],
            Unknown,
            1,
            1,
            2,
        ),
        (
            "two against one is a hint",
            vec![
                ("a", Some("1"), W, 1),
                ("b", Some("2"), W, 2),
                ("c", Some("3"), F, 3),
            ],
            HintWorks,
            2,
            1,
            1,
        ),
        (
            "block with both sides keeps the newest",
            vec![("a", Some("x"), W, 1), ("b", Some("x"), F, 2)],
            HintFails,
            0,
            1,
            2,
        ),
        (
            "same time in one block is decided by a fixed order",
            vec![("a", Some("x"), W, 5), ("b", Some("x"), F, 5)],
            HintFails,
            0,
            1,
            2,
        ),
        (
            "more than the quorum",
            vec![
                ("a", Some("1"), W, 1),
                ("b", Some("2"), W, 2),
                ("c", Some("3"), W, 3),
                ("d", Some("4"), W, 4),
                ("e", Some("5"), W, 5),
            ],
            Works,
            5,
            0,
            0,
        ),
    ]
}

fn views(c: &Case) -> Vec<ReportView> {
    c.1.iter()
        .map(|(m, b, o, at)| view(m, *b, *o, *at))
        .collect()
}

#[test]
fn cell_table() {
    let cases = cell_cases();
    assert!(cases.len() >= 20);
    for c in &cases {
        let got = cell(&views(c), &rules());
        assert_eq!(
            (got.state, got.works, got.fails, got.need_more),
            (c.2, c.3, c.4, c.5),
            "case `{}`",
            c.0
        );
    }
}

#[test]
fn cell_quorum_is_a_rule() {
    let r = QuorumRules { quorum: 1 };
    let got = cell(&[view("a", None, W, 1)], &r);
    assert_eq!(got.state, CellState::Works);
    let r = QuorumRules { quorum: 0 };
    let got = cell(&[], &r);
    assert_eq!((got.state, got.need_more), (CellState::Unknown, 1));
    let r = QuorumRules { quorum: 5 };
    let got = cell(&[view("a", None, W, 1)], &r);
    assert_eq!((got.state, got.need_more), (CellState::HintWorks, 4));
}

/// Fixed seed generator, no random crate.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[test]
fn cell_ignores_the_order_of_reports() {
    let mut rng = Lcg(0x5eed);
    for c in &cell_cases() {
        let base = views(c);
        let want = cell(&base, &rules());
        for _ in 0..100 {
            let mut v = base.clone();
            for i in (1..v.len()).rev() {
                let j = (rng.next() % (i as u64 + 1)) as usize;
                v.swap(i, j);
            }
            assert_eq!(cell(&v, &rules()), want, "case `{}`", c.0);
        }
    }
}

// ---------- classify ----------

fn cell_of(state: CellState) -> Cell {
    Cell {
        state,
        works: 0,
        fails: 0,
        need_more: 0,
    }
}

#[test]
fn classify_table() {
    use CellState::*;
    use Verdict::{Agreed, None as No, Outlier};
    let table = [
        (Works, W, Agreed),
        (Works, F, Outlier),
        (Works, I, No),
        (Fails, F, Agreed),
        (Fails, W, Outlier),
        (Fails, I, No),
        (Unknown, W, No),
        (HintWorks, W, No),
        (HintFails, F, No),
        (Mixed, W, No),
        (Mixed, F, No),
    ];
    for (state, outcome, want) in table {
        let got = classify(&view("a", None, outcome, 1), &cell_of(state));
        assert_eq!(got, want, "{state:?} {outcome:?}");
    }
}

// ---------- regressions ----------

fn history(states: &[(&str, Option<CellState>)]) -> (Vec<String>, BTreeMap<String, Cell>) {
    let commits = states.iter().map(|(c, _)| c.to_string()).collect();
    let cells = states
        .iter()
        .filter_map(|(c, s)| s.map(|s| (c.to_string(), cell_of(s))))
        .collect();
    (commits, cells)
}

#[test]
fn regressions_table() {
    use CellState::*;
    type Row = (
        &'static str,
        Vec<(&'static str, Option<CellState>)>,
        Vec<(&'static str, &'static str)>,
    );
    let table: Vec<Row> = vec![
        ("none", vec![], vec![]),
        (
            "works then fails",
            vec![("c1", Some(Works)), ("c2", Some(Fails))],
            vec![("c1", "c2")],
        ),
        (
            "fails from the start is no regression",
            vec![("c1", Some(Fails)), ("c2", Some(Fails))],
            vec![],
        ),
        (
            "gap of unknown cells",
            vec![
                ("c1", Some(Works)),
                ("c2", Some(Unknown)),
                ("c3", None),
                ("c4", Some(HintFails)),
                ("c5", Some(HintWorks)),
                ("c6", Some(Fails)),
            ],
            vec![("c1", "c6")],
        ),
        (
            "mixed does not break the range",
            vec![
                ("c1", Some(Works)),
                ("c2", Some(Mixed)),
                ("c3", Some(Fails)),
            ],
            vec![("c1", "c3")],
        ),
        (
            "last good is the newest works before the fall",
            vec![
                ("c1", Some(Works)),
                ("c2", Some(Works)),
                ("c3", Some(Unknown)),
                ("c4", Some(Fails)),
            ],
            vec![("c2", "c4")],
        ),
        (
            "repeated fails give one",
            vec![
                ("c1", Some(Works)),
                ("c2", Some(Fails)),
                ("c3", Some(Fails)),
            ],
            vec![("c1", "c2")],
        ),
        (
            "fixed and broken again",
            vec![
                ("c1", Some(Works)),
                ("c2", Some(Fails)),
                ("c3", Some(Works)),
                ("c4", Some(Fails)),
            ],
            vec![("c1", "c2"), ("c3", "c4")],
        ),
        (
            "works only",
            vec![("c1", Some(Works)), ("c2", Some(Works))],
            vec![],
        ),
    ];
    for (name, states, want) in table {
        let (commits, cells) = history(&states);
        let got: Vec<(String, String)> = regressions(&commits, &cells)
            .into_iter()
            .map(|r| (r.last_good, r.first_bad))
            .collect();
        let want: Vec<(String, String)> = want
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        assert_eq!(got, want, "case `{name}`");
    }
}

// ---------- machine_key ----------

#[test]
fn machine_key_joins_spellings() {
    let pairs = [
        (
            ("windows", "NVIDIA GeForce RTX 4080"),
            ("windows", "rtx 4080"),
        ),
        (
            ("windows", "GeForce RTX 4080"),
            ("Windows", "  RTX   4080 "),
        ),
        (
            ("windows", "NVIDIA GeForce RTX 4080"),
            ("windows", "rtx4080"),
        ),
        (
            ("windows", "AMD Radeon RX 7800 XT"),
            ("windows", "Radeon(TM) RX 7800XT"),
        ),
        (
            ("windows", "AMD Radeon(TM) RX 7800 XT Graphics"),
            ("windows", "rx-7800-xt"),
        ),
        (
            ("linux", "Intel(R) Arc(TM) A770 Graphics"),
            ("linux", "intel arc a770"),
        ),
        (
            ("windows", "NVIDIA\u{ae} GeForce\u{2122} GTX 1060"),
            ("windows", "gtx 1060"),
        ),
    ];
    for ((o1, g1), (o2, g2)) in pairs {
        assert_eq!(
            machine_key(&machine(o1, g1)),
            machine_key(&machine(o2, g2)),
            "{g1} vs {g2}"
        );
    }
    assert_eq!(
        machine_key(&machine("windows", "NVIDIA GeForce RTX 4080")),
        "windows/rtx 4080"
    );
}

#[test]
fn machine_key_keeps_cards_apart() {
    let all = [
        ("windows", "RTX 4080"),
        ("windows", "RTX 4080 Super"),
        ("windows", "RTX 4080 Ti"),
        ("windows", "RTX 4090"),
        ("windows", "RTX 3080"),
        ("windows", "GTX 1080"),
        ("windows", "RX 7800 XT"),
        ("windows", "RX 7900 XT"),
        ("linux", "RTX 4080"),
        ("windows", "RTX 4080 Laptop GPU"),
    ];
    let mut keys: Vec<String> = all
        .iter()
        .map(|(o, g)| machine_key(&machine(o, g)))
        .collect();
    let n = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), n);
    assert_eq!(machine_key(&machine("windows", "")), "windows/unknown");
}

// ---------- privacy ----------

#[test]
fn private_finds_and_scrubs() {
    let samples = [
        r"loading C:\Users\someone\AppData\Local\game\cache.bin now",
        r"loading c:/users/someone/Documents/x.log now",
        r"\\?\C:\Users\someone\x",
        "loaded /home/someone/.config/game/save.dat ok",
        "mac path /Users/someone/Library/x done",
        "wsl /mnt/c/Users/someone/x done",
        "mail me at someone@example.com please",
        "key ghp_abcdefghijklmnopqrstuvwxyz0123456789 end",
        "key sk-abcdefghijklmnopqrstuvwxyz012345 end",
        "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijk end",
        "Authorization: Bearer abcdefghij1234567890xyz",
        "token=abcd1234efgh5678",
        "password: hunter2hunter2",
        "blob Abcdefghijklmnopqrstuvwxyz0123456789ABCD end",
        "COMPUTERNAME=DESKTOP-AB12CD3",
        "hostname: myhostbox",
        "user=someone",
        "from DESKTOP-ABCDEFG at start",
        r"share \\fileserver\games\x",
    ];
    for s in samples {
        assert!(!find_private(s).is_empty(), "no hit in: {s}");
        let clean = scrub(s);
        assert_ne!(clean, s, "unchanged: {s}");
        assert!(
            find_private(&clean).is_empty(),
            "still private after scrub: {clean}"
        );
        assert!(!clean.contains("someone"), "name left: {clean}");
    }
}

#[test]
fn scrub_keeps_ordinary_lines() {
    let lines = [
        "frame 100 presented",
        "[INFO] device created in 1.25 s, adapter 0",
        "shader cache hit 4096 entries, miss 12",
        "commit 0123456789abcdef0123456789abcdef01234567 built",
        "at https://example.com/api/users/5 status 200",
        "token expired",
        "d3d12: DXGI_ERROR_DEVICE_REMOVED (0x887A0005)",
        "",
    ];
    for l in lines {
        assert!(find_private(l).is_empty(), "false hit: {l}");
        assert_eq!(scrub(l), l);
    }
    let text = lines.join("\n");
    assert_eq!(scrub(&text), text);
}

#[test]
fn scrub_handles_overlap_and_many_hits() {
    let s = "a C:\\Users\\bob\\x b bob@example.com c /home/bob/y d host=bobs-box e";
    let clean = scrub(s);
    assert!(find_private(&clean).is_empty());
    assert!(!clean.contains("bob"));
    assert!(clean.contains("<path>"));
    assert!(clean.contains("<email>"));
    assert_eq!(scrub(&clean), clean);
}

// ---------- validate_report ----------

fn refused(mut f: impl FnMut(&mut Report), field: &str) {
    let mut r = report("run1", "m1", Outcome::Works);
    f(&mut r);
    let err = validate_report(&r, &ReportLimits::default()).expect_err(field);
    assert!(err.contains(field), "`{err}` does not name {field}");
    assert!(!err.contains('\n'), "more than one line: {err}");
}

#[test]
fn validate_accepts_a_good_report() {
    let r = report("run1", "m1", Outcome::Works);
    assert_eq!(validate_report(&r, &ReportLimits::default()), Ok(()));
    let mut r = report("run1", "m1", Outcome::Fails);
    r.first_error = Some("device_removed".into());
    assert_eq!(validate_report(&r, &ReportLimits::default()), Ok(()));
}

#[test]
fn validate_refuses_each_bad_case() {
    refused(|r| r.v = 2, "report.v");
    refused(|r| r.channel = "nightly".into(), "report.channel");
    refused(|r| r.channel = String::new(), "report.channel");
    refused(|r| r.run = String::new(), "report.run");
    refused(|r| r.run = "a b".into(), "report.run");
    refused(|r| r.member = "x/y".into(), "report.member");
    refused(|r| r.build = "a".repeat(65), "report.build");
    refused(|r| r.profile = "p;q".into(), "report.profile");
    refused(|r| r.scenario = "s\n".into(), "report.scenario");
    refused(|r| r.machine.os = "macos".into(), "report.machine.os");
    refused(|r| r.machine.gpu = "  ".into(), "report.machine.gpu");
    refused(
        |r| r.machine.driver = "see /home/someone/x".into(),
        "report.machine.driver",
    );
    refused(
        |r| r.first_error = Some("Device Removed".into()),
        "report.first_error",
    );
    refused(
        |r| r.first_error = Some(String::new()),
        "report.first_error",
    );
    refused(
        |r| {
            r.metrics.clear();
            for i in 0..65 {
                r.metrics.insert(format!("m{i}"), 1.0);
            }
        },
        "report.metrics",
    );
    refused(
        |r| {
            r.metrics.insert("Bad Name".into(), 1.0);
        },
        "report.metrics name",
    );
    refused(
        |r| {
            r.metrics.insert("nan_value".into(), f64::NAN);
        },
        "nan_value",
    );
    refused(
        |r| {
            r.metrics.insert("inf_value".into(), f64::INFINITY);
        },
        "inf_value",
    );
    refused(
        |r| r.log_excerpt = "x".repeat(16 * 1024 + 1),
        "report.log_excerpt",
    );
    refused(
        |r| r.log_excerpt = r"opened C:\Users\someone\x.log".into(),
        "report.log_excerpt",
    );
    refused(
        |r| r.log_excerpt = "contact someone@example.com".into(),
        "report.log_excerpt",
    );
}

#[test]
fn validate_allows_exactly_the_limits() {
    let mut r = report("run1", "m1", Outcome::Works);
    r.metrics.clear();
    for i in 0..64 {
        r.metrics.insert(format!("m{i}"), i as f64);
    }
    r.log_excerpt = "x".repeat(16 * 1024);
    assert_eq!(validate_report(&r, &ReportLimits::default()), Ok(()));
}

// ---------- projection ----------

fn fact(id: u64, run: &str, body: &str) -> Message {
    Message {
        id,
        ts: format!("2026-11-25T10:{:02}:00.000Z", id % 60),
        from: "member".into(),
        role: Role::Member,
        kind: Kind::Fact,
        topic: format!("report/{run}"),
        body: body.into(),
        refs: vec![],
        to: vec![],
    }
}

fn line(r: &Report) -> String {
    serde_json::to_string(r).unwrap()
}

fn fixture_log() -> Vec<Message> {
    let mut bad = report("run3", "m3", Outcome::Works);
    bad.log_excerpt = r"C:\Users\someone\x.log".into();
    let mut other_gpu = report("run4", "m4", Outcome::Fails);
    other_gpu.machine = machine("linux", "AMD Radeon RX 7800 XT");
    other_gpu.first_error = Some("device_removed".into());
    let mut chat = fact(7, "run1", "not a report");
    chat.kind = Kind::Chat;
    vec![
        fact(1, "run1", &line(&report("run1", "m1", Outcome::Works))),
        fact(2, "run2", &line(&report("run2", "m2", Outcome::Works))),
        fact(3, "run3", &line(&bad)),
        fact(4, "run4", &line(&other_gpu)),
        fact(5, "run5", "{ this is not json"),
        // The topic and the run of the body must agree.
        fact(6, "run9", &line(&report("run1", "m5", Outcome::Works))),
        chat,
        fact(
            8,
            "run8",
            &line(&report("run8", "m8", Outcome::Inconclusive)),
        ),
    ]
}

fn replay(msgs: &[Message]) -> Store {
    let mut store = Store::open_in_memory().unwrap();
    for m in msgs {
        store.apply(m).unwrap();
    }
    store
}

#[test]
fn replay_gives_equal_report_tables_and_skips_bad_lines() {
    let log = fixture_log();
    let a = replay(&log);
    let b = replay(&log);
    let ra = query_reports(a.connection()).unwrap();
    let rb = query_reports(b.connection()).unwrap();
    assert_eq!(ra, rb);
    let runs: Vec<&str> = ra.iter().map(|r| r.run.as_str()).collect();
    assert_eq!(runs, ["run1", "run2", "run4", "run8"]);
    assert!(!runs.contains(&"run3"), "private log left a row");
    let first = &ra[0];
    assert_eq!(first.member, "m1");
    assert_eq!(first.machine_key, "windows/rtx 4080");
    assert_eq!(first.outcome, "works");
    assert_eq!(first.msg_id, 1);
    assert_eq!(first.metrics, r#"{"fps_median":58.5,"frames":1200.0}"#);
    assert_eq!(ra[2].first_error.as_deref(), Some("device_removed"));
    assert_eq!(ra[2].machine_key, "linux/rx 7800 xt");
    assert_eq!(ra[3].outcome, "inconclusive");
}

#[test]
fn matrix_counts_by_member() {
    let mut log = Vec::new();
    for (i, m) in ["m1", "m2", "m3"].iter().enumerate() {
        let run = format!("r{i}");
        log.push(fact(
            i as u64 + 1,
            &run,
            &line(&report(&run, m, Outcome::Works)),
        ));
    }
    let mut other = report("r9", "m9", Outcome::Fails);
    other.build = "def5678".into();
    log.push(fact(4, "r9", &line(&other)));
    let store = replay(&log);
    let state = store.api_state(2_000).unwrap();
    let matrix = state["matrix"].as_array().unwrap();
    assert_eq!(matrix.len(), 2);
    let settled = matrix.iter().find(|r| r["build"] == "abc1234").unwrap();
    assert_eq!(settled["profile"], "title-a");
    assert_eq!(settled["machine_key"], "windows/rtx 4080");
    assert_eq!(settled["state"], "works");
    assert_eq!(settled["works"], 3);
    assert_eq!(settled["counted_by"], "member");
    let hint = matrix.iter().find(|r| r["build"] == "def5678").unwrap();
    assert_eq!(hint["state"], "hint_fails");
    assert_eq!(hint["need_more"], 2);
}
