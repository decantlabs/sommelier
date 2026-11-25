//! `sommelier serve --factory FILE`: the local settings file, the policy veto and the one `[switch]` table.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use sommelier::plan::PlanPolicy;
use sommelier::policy::{Mode, Snapshot};

const EXAMPLE: &str = include_str!("../../swarm/factory.example.toml");

fn serve(dir: &std::path::Path, factory: &std::path::Path, extra: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sommelier"));
    c.args([
        "serve",
        "--listen",
        "127.0.0.1:0",
        "--no-ui",
        "--data",
        dir.to_str().unwrap(),
        "--factory",
        factory.to_str().unwrap(),
    ])
    .args(extra)
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    c
}

/// Starts the hub, reads stderr until the factory line or the exit, then stops it.
fn first_lines(mut cmd: Command) -> (bool, Vec<String>) {
    let mut child = cmd.spawn().unwrap();
    let mut lines = Vec::new();
    let mut ok = true;
    let err = child.stderr.take().unwrap();
    for line in BufReader::new(err).lines() {
        let line = line.unwrap();
        let stop = line.starts_with("factory:");
        lines.push(line);
        if stop {
            break;
        }
    }
    if let Some(status) = child.try_wait().unwrap() {
        ok = status.success();
    } else {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    (ok, lines)
}

#[test]
fn the_example_file_parses_and_names_every_part() {
    let file = sommelier::factory::parse(EXAMPLE).unwrap();
    assert!(!file.intake.enabled && !file.verify.enabled && !file.door.keep_days.is_none());
}

#[test]
fn a_factory_file_switches_parts_on_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("factory.toml");
    std::fs::write(
        &file,
        "[join]\nenabled = true\n[intake]\nenabled = true\n[review]\nenabled = true\n",
    )
    .unwrap();
    let (_, lines) = first_lines(serve(&dir.path().join("data"), &file, &["--quiet"]));
    let line = lines.last().unwrap();
    assert_eq!(
        line,
        "factory: join on, intake on, verify off, review on, stamp off, test off, door off"
    );
}

#[test]
fn an_unknown_key_is_refused_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("factory.toml");
    std::fs::write(&file, "[intake]\nenabled = true\nreference_repos = \"x\"\n").unwrap();
    let out = serve(&dir.path().join("data"), &file, &[])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("reference_repos"), "{err}");
    assert!(err.contains("--factory"), "{err}");
}

#[test]
fn a_policy_switch_off_vetoes_a_part_the_file_enables() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let policy = dir.path().join("policy.toml");
    let text = format!(
        "{}\n[switch]\nintake = \"off\"\n",
        sommelier::policy::DEFAULT_POLICY
    );
    std::fs::write(&policy, text).unwrap();
    let file = dir.path().join("factory.toml");
    std::fs::write(
        &file,
        "[intake]\nenabled = true\n[review]\nenabled = true\n",
    )
    .unwrap();
    let (_, lines) = first_lines(serve(
        &data,
        &file,
        &["--quiet", "--policy", policy.to_str().unwrap()],
    ));
    let line = lines.last().unwrap();
    assert!(line.contains("intake off (policy switch)"), "{line}");
    assert!(line.contains("review on"), "{line}");
    std::thread::sleep(Duration::from_millis(10));
}

#[test]
fn one_policy_file_holds_both_kinds_of_switches() {
    let text = format!(
        "{}\n[switch]\nintake = \"enforce\"\ntier0_coding = \"shadow\"\nscheduler = \"shadow\"\nadmission = \"off\"\nprobe_codex = \"off\"\n",
        sommelier::policy::DEFAULT_POLICY
    );
    let snap = Snapshot::parse(&text).unwrap();
    let rules = snap.policy.factory();
    assert_eq!(rules.switch.intake, Mode::Enforce);
    assert_eq!(rules.switch.tier0_coding, Mode::Shadow);
    assert_eq!(rules.switch.scheduler, Some(Mode::Shadow));
    assert_eq!(rules.switch.admission, Some(Mode::Off));
    assert!(snap.policy.has_switch_table());
    // The scheduler's loader reads the same text.
    let plan = PlanPolicy::parse_toml(&text).unwrap();
    assert_eq!(plan.switch.probes.get("codex"), Some(&false));
    assert_eq!(plan.switch.admission, sommelier::plan::PlanMode::Off);
    // The file survives the round trip the hub does for the policy cache.
    let back = Snapshot::verify(snap.response()).unwrap();
    assert_eq!(back.policy.factory(), rules);
    // A key that belongs to neither loader is refused.
    let bad = format!(
        "{}\n[switch]\nsomething = \"off\"\n",
        sommelier::policy::DEFAULT_POLICY
    );
    assert!(
        Snapshot::parse(&bad)
            .unwrap_err()
            .to_string()
            .contains("switch.something")
    );
    let bad = format!(
        "{}\n[switch]\nprobe_codex = \"maybe\"\n",
        sommelier::policy::DEFAULT_POLICY
    );
    assert!(Snapshot::parse(&bad).is_err());
}
