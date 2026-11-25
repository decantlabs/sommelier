//! Tests of the tester side: title profiles, the title check, the machine class, the build check and
//! the report. No network, no live hub, no account.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sommelier::report::types::{MachineClass, Outcome, Report};
use sommelier::tester::machine::{
    normalize_gpu, parse_gpu, parse_gpu_lspci, parse_gpu_nvidia_smi, parse_gpu_windows,
};
use sommelier::tester::run::EXCERPT_MAX;
use sommelier::tester::{
    CmdOutput, CommandProbe, CommandRunner, GpuOutput, GpuSource, Probe, Profile, RunError,
    RunInput, build_report, check_title, machine_class, machine_class_with, verify_build,
};

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tester")
        .join(rel)
}

fn read_fixture(rel: &str) -> String {
    std::fs::read_to_string(fixture(rel)).unwrap_or_else(|e| panic!("fixture {rel}: {e}"))
}

fn example() -> Profile {
    Profile::load(&fixture("profile.toml")).expect("example profile loads")
}

// ---------------------------------------------------------------- profile

#[test]
fn example_profile_loads() {
    let p = example();
    assert_eq!(p.id, "example_title");
    assert_eq!(p.scenarios.len(), 2);
    let s = p.scenario("boot_menu").unwrap();
    assert_eq!(s.seconds, 60);
    assert_eq!(s.inputs.len(), 2);
    assert_eq!(s.milestones.len(), 3);
    assert_eq!(s.metrics, ["fps_median", "frames", "first_present_s"]);
}

/// Text of the example profile with one exact piece replaced.
fn broken(from: &str, to: &str) -> String {
    let text = read_fixture("profile.toml");
    assert!(text.contains(from), "fixture lost the text `{from}`");
    text.replacen(from, to, 1)
}

#[test]
fn profile_rules_name_the_field() {
    let cases: Vec<(String, &str)> = vec![
        (broken("id = \"example_title\"", "id = \"Bad Id\""), "id:"),
        (broken("name = \"Example Title\"", "name = \" \""), "name:"),
        (
            broken("file = \"example_title.exe\"", "file = \"dir/x.exe\""),
            "executable.file",
        ),
        (broken("size = 123456789", "size = 0"), "executable.size"),
        (
            broken(
                "sha256 = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"",
                "sha256 = \"ABC\"",
            ),
            "executable.sha256",
        ),
        (
            broken(
                "pattern = \"swapchain created\"",
                "pattern = \"swap(chain\"",
            ),
            "scenarios[0].milestones[0].pattern",
        ),
        (
            broken("pattern = \"swapchain created\"", "pattern = \"\""),
            "scenarios[0].milestones[0].pattern",
        ),
        (
            broken("by = 20", "by = 61"),
            "scenarios[0].milestones[0].by",
        ),
        (
            broken("id = \"first_frame\"", "id = \"window_up\""),
            "scenarios[0].milestones[1].id",
        ),
        (
            broken("seconds = 60", "seconds = 0"),
            "scenarios[0].seconds",
        ),
        (broken("at = 35", "at = 99"), "scenarios[0].inputs[0].at"),
        (
            broken("key = \"enter\"", "key = \"Enter Key\""),
            "scenarios[0].inputs[0].key",
        ),
        (
            broken(
                "metrics = [\"fps_median\", \"frames\", \"first_present_s\"]",
                "metrics = [\"FPS\"]",
            ),
            "scenarios[0].metrics[0]",
        ),
        (
            broken("id = \"drive_loop\"", "id = \"boot_menu\""),
            "scenarios[1].id",
        ),
    ];
    for (text, field) in cases {
        let err = Profile::parse(&text).expect_err(field);
        assert!(err.contains(field), "`{err}` should name `{field}`");
    }
}

#[test]
fn profile_needs_a_milestone_and_a_scenario() {
    let no_ms = "id = \"t\"\nname = \"T\"\n[executable]\nfile = \"t.exe\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\nsize = 1\n[[scenarios]]\nid = \"s\"\nseconds = 10\nmilestones = []\n";
    let err = Profile::parse(no_ms).unwrap_err();
    assert!(err.contains("scenarios[0].milestones"), "{err}");
    let no_sc = "id = \"t\"\nname = \"T\"\nscenarios = []\n[executable]\nfile = \"t.exe\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\nsize = 1\n";
    let err = Profile::parse(no_sc).unwrap_err();
    assert!(err.contains("scenarios"), "{err}");
}

#[test]
fn profile_refuses_unknown_fields() {
    for (from, to) in [
        (
            "name = \"Example Title\"",
            "name = \"Example Title\"\nextra = 1",
        ),
        ("size = 123456789", "size = 123456789\nowner = \"x\""),
        ("seconds = 60", "seconds = 60\nspeed = 2"),
        ("by = 20", "by = 20\nfuzzy = true"),
    ] {
        assert!(Profile::parse(&broken(from, to)).is_err(), "{to}");
    }
}

// ---------------------------------------------------------------- title check

fn profile_for(bytes: &[u8], size: u64) -> Profile {
    let mut p = example();
    p.executable.sha256 = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    p.executable.size = size;
    p
}

#[test]
fn check_title_yes_and_no() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("example_title.exe");
    // More than one read buffer, so the stream loop runs more than once.
    let bytes: Vec<u8> = (0..200_000u32).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(&exe, &bytes).unwrap();
    let p = profile_for(&bytes, bytes.len() as u64);
    assert_eq!(check_title(&p, &exe), Ok(true));

    // One changed byte, same size.
    let mut changed = bytes.clone();
    changed[100_000] ^= 1;
    let other = dir.path().join("changed.exe");
    std::fs::write(&other, &changed).unwrap();
    assert_eq!(check_title(&p, &other), Ok(false));

    // Wrong size in the profile.
    let wrong = profile_for(&bytes, bytes.len() as u64 + 1);
    assert_eq!(check_title(&wrong, &exe), Ok(false));

    // A shorter file.
    let short = dir.path().join("short.exe");
    std::fs::write(&short, &bytes[..bytes.len() - 1]).unwrap();
    assert_eq!(check_title(&p, &short), Ok(false));
}

#[test]
fn check_title_error_leaks_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("secret_folder_name").join("game.exe");
    let err = check_title(&example(), &missing).unwrap_err();
    assert!(!err.contains("secret_folder_name"), "{err}");
    assert!(!err.contains("game.exe"), "{err}");
}

// ---------------------------------------------------------------- machine class

#[test]
fn gpu_parser_windows() {
    let g = parse_gpu_windows(&read_fixture("gpu/windows.txt")).unwrap();
    assert_eq!(g.name, "example gamer card rtx 4080");
    assert_eq!(g.memory_mb, 4095);
    assert_eq!(g.driver, "31.0.15.5123");
    // Only a software adapter: it is still better than nothing.
    let soft = "Name : Microsoft Basic Render Driver\nAdapterRAM : 0\nDriverVersion : 1.0\n";
    assert_eq!(
        parse_gpu_windows(soft).unwrap().name,
        "microsoft basic render driver"
    );
    assert!(parse_gpu_windows("").is_none());
    assert!(parse_gpu_windows("nothing useful here").is_none());
}

#[test]
fn gpu_parser_linux() {
    let g = parse_gpu_nvidia_smi(&read_fixture("gpu/nvidia_smi.txt")).unwrap();
    assert_eq!(g.name, "rtx 3070");
    assert_eq!(g.memory_mb, 8192);
    assert_eq!(g.driver, "555.42.02");
    assert!(parse_gpu_nvidia_smi("garbage").is_none());

    let l = parse_gpu_lspci(&read_fixture("gpu/lspci.txt")).unwrap();
    assert_eq!(l.name, "radeon rx 7900 xt");
    assert_eq!(l.memory_mb, 0);
    assert!(parse_gpu_lspci("00:1f.3 \"Audio device\" \"x\" \"y\"").is_none());
}

#[test]
fn gpu_names_are_normalised() {
    assert_eq!(normalize_gpu("  NVIDIA  GeForce RTX 4080 "), "rtx 4080");
    assert_eq!(
        normalize_gpu("AMD Radeon(TM) RX 6800"),
        "amd radeon rx 6800"
    );
}

struct FakeProbe(Result<GpuOutput, String>);

impl Probe for FakeProbe {
    fn gpu_output(&self) -> Result<GpuOutput, String> {
        self.0.clone()
    }
}

#[test]
fn machine_class_uses_probe_and_override() {
    let probe = FakeProbe(Ok(GpuOutput {
        source: GpuSource::NvidiaSmi,
        text: read_fixture("gpu/nvidia_smi.txt"),
    }));
    let m = machine_class(&probe);
    assert_eq!(m.gpu, "rtx 3070");
    assert_eq!(m.gpu_memory_mb, 8192);
    assert!(m.memory_mb > 0, "memory comes from sysinfo");
    assert!(m.os == "windows" || m.os == "linux");
    assert_eq!(m.gpu, m.gpu.to_lowercase());

    let m = machine_class_with(&probe, Some("My Custom GPU 9000"));
    assert_eq!(m.gpu, "my custom gpu 9000");
    assert_eq!(m.gpu_memory_mb, 8192);

    let broken = FakeProbe(Err("no tool".into()));
    let m = machine_class(&broken);
    assert_eq!(m.gpu, "unknown");
    let m = machine_class_with(&broken, Some("RTX 2060"));
    assert_eq!(m.gpu, "rtx 2060");
    assert_eq!(m.gpu_memory_mb, 0);
    // A blank override is no override.
    assert_eq!(machine_class_with(&broken, Some("  ")).gpu, "unknown");
    assert!(
        parse_gpu(&GpuOutput {
            source: GpuSource::LspciMm,
            text: String::new()
        })
        .is_none()
    );
}

/// Fake runner that answers by program name and records the calls.
struct Fake {
    answers: Vec<(&'static str, Result<CmdOutput, RunError>)>,
    calls: RefCell<Vec<(String, Vec<String>)>>,
}

impl Fake {
    fn new(answers: Vec<(&'static str, Result<CmdOutput, RunError>)>) -> Self {
        Fake {
            answers,
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl CommandRunner for Fake {
    fn run(&self, program: &str, args: &[String]) -> Result<CmdOutput, RunError> {
        self.calls
            .borrow_mut()
            .push((program.to_string(), args.to_vec()));
        self.answers
            .iter()
            .find(|(p, _)| *p == program)
            .map(|(_, r)| r.clone())
            .unwrap_or(Err(RunError::NotFound))
    }
}

fn out(code: i32, stdout: &str, stderr: &str) -> Result<CmdOutput, RunError> {
    Ok(CmdOutput {
        code: Some(code),
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

#[test]
fn command_probe_picks_the_command_of_the_system() {
    let fake = Fake::new(vec![
        ("powershell", out(0, &read_fixture("gpu/windows.txt"), "")),
        (
            "nvidia-smi",
            out(0, &read_fixture("gpu/nvidia_smi.txt"), ""),
        ),
        ("lspci", out(0, &read_fixture("gpu/lspci.txt"), "")),
    ]);
    let got = CommandProbe { runner: &fake }.gpu_output().unwrap();
    if cfg!(windows) {
        assert_eq!(got.source, GpuSource::WindowsCim);
    } else {
        assert_eq!(got.source, GpuSource::NvidiaSmi);
    }
    // No nvidia-smi on Linux: lspci answers.
    if !cfg!(windows) {
        let fake = Fake::new(vec![("lspci", out(0, &read_fixture("gpu/lspci.txt"), ""))]);
        let got = CommandProbe { runner: &fake }.gpu_output().unwrap();
        assert_eq!(got.source, GpuSource::LspciMm);
    }
    let none = Fake::new(vec![]);
    assert!(CommandProbe { runner: &none }.gpu_output().is_err());
}

// ---------------------------------------------------------------- verify_build

const OK_TEXT: &str = "Loaded digest sha256:ab for file\nLoaded 1 attestation from GitHub API\n\nThe following policy criteria will be enforced:\n\n✓ Verification succeeded!\n";

#[test]
fn verify_build_passes_on_clean_success() {
    let fake = Fake::new(vec![("gh", out(0, OK_TEXT, ""))]);
    assert_eq!(
        verify_build(Path::new("build.zip"), "example-org/example-repo", &fake),
        Ok(())
    );
    let calls = fake.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "gh");
    assert_eq!(
        calls[0].1,
        [
            "attestation",
            "verify",
            "build.zip",
            "--repo",
            "example-org/example-repo"
        ]
    );
    // gh prints to the error stream in some versions.
    let fake = Fake::new(vec![("gh", out(0, "", OK_TEXT))]);
    assert!(verify_build(Path::new("build.zip"), "o/r", &fake).is_ok());
}

#[test]
fn verify_build_fails_closed() {
    let a = Path::new("build.zip");
    let missing = Fake::new(vec![("gh", Err(RunError::NotFound))]);
    let e1 = verify_build(a, "o/r", &missing).unwrap_err();
    assert!(
        e1.contains("not installed") && e1.contains("install"),
        "{e1}"
    );

    let failing = Fake::new(vec![("gh", out(1, "", "no attestations found"))]);
    let e2 = verify_build(a, "o/r", &failing).unwrap_err();
    assert!(
        e2.contains("does not verify") && e2.contains("exit code 1"),
        "{e2}"
    );

    let odd = Fake::new(vec![("gh", out(0, "all good, trust me\n", ""))]);
    let e3 = verify_build(a, "o/r", &odd).unwrap_err();
    assert!(e3.contains("did not report"), "{e3}");

    let hung = Fake::new(vec![("gh", Err(RunError::Failed("timed out".into())))]);
    let e4 = verify_build(a, "o/r", &hung).unwrap_err();
    assert!(e4.contains("could not run"), "{e4}");

    let signal = Fake::new(vec![(
        "gh",
        Ok(CmdOutput {
            code: None,
            stdout: OK_TEXT.into(),
            stderr: String::new(),
        }),
    )]);
    assert!(verify_build(a, "o/r", &signal).is_err());

    let msgs = [e1, e2, e3, e4];
    for (i, m) in msgs.iter().enumerate() {
        for other in &msgs[i + 1..] {
            assert_ne!(m, other);
        }
    }
}

#[test]
fn verify_build_refuses_bad_input_before_running() {
    let fake = Fake::new(vec![("gh", out(0, OK_TEXT, ""))]);
    for repo in ["", "noslash", "a/b/c", "--flag/x", "a/b c", "a/-b"] {
        assert!(
            verify_build(Path::new("b.zip"), repo, &fake).is_err(),
            "{repo}"
        );
    }
    assert!(verify_build(Path::new("--help"), "o/r", &fake).is_err());
    assert!(fake.calls.borrow().is_empty());
}

// ---------------------------------------------------------------- build_report

fn machine() -> MachineClass {
    MachineClass {
        os: "windows".into(),
        os_build: "26100".into(),
        gpu: "rtx 4080".into(),
        gpu_memory_mb: 16384,
        driver: "555.42".into(),
        memory_mb: 32768,
    }
}

#[derive(Clone, Copy, Default)]
struct Flags {
    ended_by_itself: bool,
    paused: bool,
    title_bad: bool,
    harness: bool,
}

struct Case {
    name: &'static str,
    secs_alive: u32,
    flags: Flags,
    outcome: Outcome,
    first_error: Option<&'static str>,
    metrics: &'static [(&'static str, f64)],
}

const ALL_METRICS: &[(&str, f64)] = &[
    ("first_present_s", 12.0),
    ("fps_median", 58.2),
    ("frames", 1800.0),
];

fn cases() -> Vec<Case> {
    let none = Flags::default();
    let early = Flags {
        ended_by_itself: true,
        ..none
    };
    vec![
        Case {
            name: "ok",
            secs_alive: 60,
            flags: none,
            outcome: Outcome::Works,
            first_error: None,
            metrics: ALL_METRICS,
        },
        Case {
            name: "late",
            secs_alive: 60,
            flags: none,
            outcome: Outcome::Fails,
            first_error: Some("timeout"),
            metrics: ALL_METRICS,
        },
        Case {
            name: "missing",
            secs_alive: 60,
            flags: none,
            outcome: Outcome::Fails,
            first_error: Some("timeout"),
            metrics: ALL_METRICS,
        },
        Case {
            name: "early_exit",
            secs_alive: 25,
            flags: early,
            outcome: Outcome::Fails,
            first_error: Some("exit_early"),
            metrics: &[("first_present_s", 12.0), ("frames", 10.0)],
        },
        Case {
            name: "early_exit_late",
            secs_alive: 50,
            flags: early,
            outcome: Outcome::Fails,
            first_error: Some("exit_early"),
            metrics: ALL_METRICS,
        },
        Case {
            name: "device_removed",
            secs_alive: 22,
            flags: early,
            outcome: Outcome::Fails,
            first_error: Some("device_removed"),
            metrics: ALL_METRICS,
        },
        Case {
            name: "unhandled",
            secs_alive: 19,
            flags: early,
            outcome: Outcome::Fails,
            first_error: Some("unhandled_exception"),
            metrics: ALL_METRICS,
        },
        Case {
            name: "out_of_memory",
            secs_alive: 16,
            flags: early,
            outcome: Outcome::Fails,
            first_error: Some("out_of_memory"),
            metrics: ALL_METRICS,
        },
        Case {
            name: "paused",
            secs_alive: 60,
            flags: Flags {
                paused: true,
                ..none
            },
            outcome: Outcome::Inconclusive,
            first_error: None,
            metrics: &[],
        },
        Case {
            name: "title_bad",
            secs_alive: 60,
            flags: Flags {
                title_bad: true,
                ..none
            },
            outcome: Outcome::Inconclusive,
            first_error: None,
            metrics: &[],
        },
        Case {
            name: "harness",
            secs_alive: 60,
            flags: Flags {
                harness: true,
                ..none
            },
            outcome: Outcome::Inconclusive,
            first_error: None,
            metrics: &[],
        },
        Case {
            name: "missing_metrics",
            secs_alive: 60,
            flags: none,
            outcome: Outcome::Works,
            first_error: None,
            metrics: &[("frames", 1800.0)],
        },
        Case {
            name: "noise_after_goal",
            secs_alive: 60,
            flags: none,
            outcome: Outcome::Works,
            first_error: None,
            metrics: ALL_METRICS,
        },
    ]
}

fn run_case(profile: &Profile, c: &Case, scrub: &dyn Fn(&str) -> String) -> Report {
    let log = read_fixture(&format!("runs/{}.log", c.name));
    let summary: Vec<String> = read_fixture(&format!("runs/{}.summary", c.name))
        .lines()
        .map(str::to_string)
        .collect();
    build_report(
        &RunInput {
            run: "run-1",
            member: "member-1",
            build: "abc1234",
            channel: "main",
            profile,
            scenario: "boot_menu",
            machine: machine(),
            summary: &summary,
            log: &log,
            at: 1_800_000_000,
            secs_alive: c.secs_alive,
            ended_by_itself: c.flags.ended_by_itself,
            paused: c.flags.paused,
            title_ok: !c.flags.title_bad,
            harness_error: c.flags.harness,
        },
        scrub,
    )
}

#[test]
fn report_table() {
    let p = example();
    let cs = cases();
    assert!(cs.len() >= 12);
    for c in &cs {
        let r = run_case(&p, c, &|s| s.to_string());
        assert_eq!(r.outcome, c.outcome, "{}", c.name);
        assert_eq!(r.first_error.as_deref(), c.first_error, "{}", c.name);
        let want: std::collections::BTreeMap<String, f64> =
            c.metrics.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        assert_eq!(r.metrics, want, "{}", c.name);
        assert_eq!(r.v, 1);
        assert_eq!(r.profile, "example_title");
        assert_eq!(r.scenario, "boot_menu");
        assert_eq!(r.secs_alive, c.secs_alive, "{}", c.name);
        assert_eq!(r.at, 1_800_000_000);
        assert!(r.sig.is_none(), "the caller signs");
        assert!(!r.log_excerpt.is_empty(), "{}", c.name);
    }
}

#[test]
fn report_for_an_unknown_scenario_is_inconclusive() {
    let p = example();
    let log = read_fixture("runs/ok.log");
    let r = build_report(
        &RunInput {
            run: "r",
            member: "m",
            build: "b",
            channel: "main",
            profile: &p,
            scenario: "no_such_scenario",
            machine: machine(),
            summary: &[],
            log: &log,
            at: 1,
            secs_alive: 60,
            ended_by_itself: false,
            paused: false,
            title_ok: true,
            harness_error: false,
        },
        &|s| s.to_string(),
    );
    assert_eq!(r.outcome, Outcome::Inconclusive);
}

#[test]
fn report_holds_no_user_folder() {
    let p = example();
    for c in cases() {
        let r = run_case(&p, &c, &|s| s.to_string());
        let json = serde_json::to_string(&r).unwrap();
        for bad in ["Users", "\\home\\", "/home/", "C:\\", "D:\\"] {
            assert!(!json.contains(bad), "{}: {bad}", c.name);
        }
    }
}

fn big_log(lines: usize) -> String {
    let mut s = String::new();
    for i in 0..lines {
        s.push_str(&format!(
            "{}.0 line number {i} with some text to fill space\n",
            i % 59
        ));
    }
    s
}

fn report_with_log(log: &str, scrub: &dyn Fn(&str) -> String) -> Report {
    let p = example();
    build_report(
        &RunInput {
            run: "r",
            member: "m",
            build: "b",
            channel: "main",
            profile: &p,
            scenario: "boot_menu",
            machine: machine(),
            summary: &[],
            log,
            at: 1,
            secs_alive: 60,
            ended_by_itself: false,
            paused: false,
            title_ok: true,
            harness_error: false,
        },
        scrub,
    )
}

#[test]
fn excerpt_is_cut_on_a_line_and_scrubbed() {
    let log = big_log(20_000);
    assert!(log.len() > 10 * EXCERPT_MAX);
    let scrub = |l: &str| format!("S|{}", l.replace("number", "NUM"));
    let r = report_with_log(&log, &scrub);
    let ex = &r.log_excerpt;
    assert!(
        ex.len() <= EXCERPT_MAX && ex.len() > EXCERPT_MAX - 200,
        "{}",
        ex.len()
    );
    assert!(ex.ends_with('\n'));
    assert!(
        ex.lines().all(|l| l.starts_with("S|")),
        "every line is scrubbed, none is cut"
    );
    assert!(ex.contains("NUM 19999 "), "the tail of the log is kept");
    assert!(!ex.contains("number"));
}

#[test]
fn excerpt_with_a_scrub_that_grows_text_stays_in_the_limit() {
    let log = big_log(5_000);
    let r = report_with_log(&log, &|l| format!("{l} {l}"));
    assert!(r.log_excerpt.len() <= EXCERPT_MAX);
    assert!(r.log_excerpt.ends_with('\n'));
}

#[test]
fn excerpt_handles_multibyte_text_and_short_logs() {
    let mut log = String::new();
    for i in 0..9_000 {
        log.push_str(&format!("{i}.0 \u{e4}\u{20ac}\u{1f600} line\n"));
    }
    let r = report_with_log(&log, &|s| s.to_string());
    assert!(r.log_excerpt.len() <= EXCERPT_MAX);
    assert!(r.log_excerpt.ends_with('\n'));
    assert!(r.log_excerpt.lines().all(|l| l.ends_with(" line")));

    let short = "1.0 hello\n2.0 world\n";
    assert_eq!(
        report_with_log(short, &|s| s.to_string()).log_excerpt,
        short
    );
    assert_eq!(report_with_log("", &|s| s.to_string()).log_excerpt, "");
    // A log that is one huge line has no line boundary to cut on.
    let one = "x".repeat(3 * EXCERPT_MAX);
    assert_eq!(report_with_log(&one, &|s| s.to_string()).log_excerpt, "");
}

#[test]
fn scrub_function_sees_no_more_than_the_log() {
    // The scrub function is the only place a path can be removed. The report keeps what it returns.
    let log = "1.0 opened a file in the user area\n";
    let r = report_with_log(log, &|l| l.replace("user area", "<path>"));
    assert_eq!(r.log_excerpt, "1.0 opened a file in the <path>\n");
}
