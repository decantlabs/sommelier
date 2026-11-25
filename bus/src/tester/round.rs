//! One round of `sommelier test`: ask the hub for a run order, check the build and the title, run the
//! scenario, build the report, sign it and send it.
//!
//! Every effect outside this process sits behind [`Effects`]: the machine probe, the build check, the
//! run script, the clock, the pause file and sleeping. Tests fake them. The hub is reached through
//! the ordinary client, so a test can run a real hub on a free port.
//!
//! Order of a round, and why:
//!
//! 1. No pause file, else the round does not start.
//! 2. `test.next`. Nothing to test ends the round.
//! 3. The build check, then the title check. A failure sends an inconclusive report for the order
//!    that was taken, so the order does not wait for this tester, and runs nothing.
//! 4. The scenario through the run script, the report, its signature, `report.send`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::cmd::{CommandRunner, SystemRunner};
use super::machine::{CommandProbe, machine_class_with};
use super::profile::{Profile, check_title};
use super::run::{RunInput, build_report};
use super::verify::verify_build;
use crate::client::{Opts, request_ok};
use crate::key::crypto::{MemberKey, sign_request};
use crate::report::scrub;
use crate::report::types::{MachineClass, Outcome, Report};

/// The excerpt that goes to the hub is cut to this many bytes. A message body of the hub is at most
/// 4096 bytes, so a report must stay far below the 16 KiB the report module could take.
pub const SEND_EXCERPT_MAX: usize = 2048;

/// A whole report as text must stay below this, before the signature is added.
const REPORT_BYTES_MAX: usize = 3800;

/// What the run script is asked to do.
#[derive(Clone, Debug)]
pub struct ScenarioRun<'a> {
    pub build: &'a Path,
    pub title: &'a Path,
    pub seconds: u32,
    /// A short name for the run folder.
    pub label: &'a str,
}

/// What a run left behind, as the report builder needs it.
#[derive(Clone, Debug, Default)]
pub struct ScenarioOutput {
    /// Lines `name: value` (the summary of the run script and the metric columns).
    pub summary: Vec<String>,
    /// Text of the title log.
    pub log: String,
    pub secs_alive: u32,
    pub ended_by_itself: bool,
}

/// Everything a round does outside this process.
pub trait Effects {
    /// UTC epoch seconds.
    fn now(&self) -> u64;
    /// True while the pause file exists.
    fn paused(&self) -> bool;
    /// The machine class, with the GPU the tester stated if any.
    fn machine(&self, gpu: Option<&str>) -> MachineClass;
    /// The provenance check of the build. An error means: do not run it.
    fn verify_build(&self, build: &Path, repo: &str) -> Result<(), String>;
    /// Runs the scenario once and says what it left.
    fn run_scenario(&self, run: &ScenarioRun<'_>) -> Result<ScenarioOutput, String>;
    /// Waits between rounds of a loop.
    fn sleep(&self, secs: u64);
}

/// What a round needs to know.
pub struct RoundConfig {
    pub opts: Opts,
    /// The key that signs reports. None for a token member.
    pub key: Option<MemberKey>,
    pub profile: Profile,
    /// The tester's own copy of the title.
    pub title: PathBuf,
    /// The build to run.
    pub build: PathBuf,
    /// `owner/name` of the repository that attests the build.
    pub repo: String,
    pub gpu: Option<String>,
    /// Seconds to wait between rounds of a loop that found work.
    pub interval_secs: u64,
}

/// How a round ended.
#[derive(Clone, Debug, PartialEq)]
pub enum RoundOutcome {
    /// The pause file exists. Nothing was asked.
    Paused,
    /// The hub has no order for this machine now.
    NothingToTest,
    /// A report was sent.
    Reported {
        run: String,
        outcome: Outcome,
        first_error: Option<String>,
        /// Why the run was not made, when it was not.
        note: Option<String>,
    },
}

impl RoundOutcome {
    /// The one line the command prints.
    pub fn line(&self) -> String {
        match self {
            RoundOutcome::Paused => "paused: the pause file exists, nothing was asked".into(),
            RoundOutcome::NothingToTest => "nothing to test now".into(),
            RoundOutcome::Reported {
                run,
                outcome,
                first_error,
                note,
            } => {
                let name = match outcome {
                    Outcome::Works => "works",
                    Outcome::Fails => "fails",
                    Outcome::Inconclusive => "inconclusive",
                };
                let mut s = format!("run order {run}: {name}");
                if let Some(e) = first_error {
                    s.push_str(&format!(", first error {e}"));
                }
                if let Some(n) = note {
                    s.push_str(&format!(" ({n})"));
                }
                s.push_str(", report sent");
                s
            }
        }
    }
}

async fn call(opts: &Opts, op: &str, mut body: Value) -> Result<Value, String> {
    if let Some(o) = body.as_object_mut() {
        o.insert("op".into(), json!(op));
    }
    request_ok(opts, body).await
}

/// `text` cut to its last `max` bytes on a line boundary.
fn tail_lines(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    match text[start..].find('\n') {
        Some(nl) if text.as_bytes()[start - 1] != b'\n' => text[start + nl + 1..].to_owned(),
        _ => text[start..].to_owned(),
    }
}

/// Cuts the excerpt until the whole report fits a message of the hub.
fn fit(report: &mut Report) {
    report.log_excerpt = tail_lines(&report.log_excerpt, SEND_EXCERPT_MAX);
    loop {
        let len = serde_json::to_string(report).map(|s| s.len()).unwrap_or(0);
        if len <= REPORT_BYTES_MAX || report.log_excerpt.is_empty() {
            return;
        }
        let keep = report.log_excerpt.len() * 3 / 4;
        report.log_excerpt = tail_lines(&report.log_excerpt, keep);
        if keep < 16 {
            report.log_excerpt.clear();
        }
    }
}

fn text_of(v: &Value, field: &str) -> Result<String, String> {
    v.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("the hub's order has no `{field}`"))
}

/// One round. An error is a refusal or a failure to reach the hub, in one line.
pub async fn run_round(cfg: &RoundConfig, fx: &dyn Effects) -> Result<RoundOutcome, String> {
    if fx.paused() {
        return Ok(RoundOutcome::Paused);
    }
    let who = call(&cfg.opts, "whoami", json!({})).await?;
    let member = text_of(&who, "name")?;
    let machine = fx.machine(cfg.gpu.as_deref());
    let next = call(
        &cfg.opts,
        "test.next",
        json!({"machine": machine, "profiles": [cfg.profile.id]}),
    )
    .await?;
    let Some(order) = next.get("order").filter(|o| o.is_object()) else {
        return Ok(RoundOutcome::NothingToTest);
    };
    let run = text_of(order, "id")?;
    let build_commit = text_of(order, "build")?;
    let channel = text_of(order, "channel")?;
    let scenario = text_of(order, "scenario")?;
    if text_of(order, "profile")? != cfg.profile.id {
        return Err("the hub handed out an order for another profile: update sommelier".into());
    }

    let sc = cfg.profile.scenario(&scenario);
    let mut note: Option<String> = None;
    let mut harness_error = sc.is_none();
    if sc.is_none() {
        note = Some(format!("the profile has no scenario `{scenario}`"));
    }
    let mut title_ok = true;
    if !harness_error && let Err(e) = fx.verify_build(&cfg.build, &cfg.repo) {
        harness_error = true;
        note = Some(e);
    }
    if !harness_error {
        match check_title(&cfg.profile, &cfg.title) {
            Ok(true) => {}
            Ok(false) => {
                title_ok = false;
                note = Some("your copy of the title does not match the profile".into());
            }
            Err(e) => {
                title_ok = false;
                note = Some(e);
            }
        }
    }
    let mut out = ScenarioOutput::default();
    if !harness_error
        && title_ok
        && let Some(sc) = sc
    {
        let label = format!("test-{run}");
        match fx.run_scenario(&ScenarioRun {
            build: &cfg.build,
            title: &cfg.title,
            seconds: sc.seconds,
            label: &label,
        }) {
            Ok(o) => out = o,
            Err(e) => {
                harness_error = true;
                note = Some(e);
            }
        }
    }

    let input = RunInput {
        run: &run,
        member: &member,
        build: &build_commit,
        channel: &channel,
        profile: &cfg.profile,
        scenario: &scenario,
        machine,
        summary: &out.summary,
        log: &out.log,
        at: fx.now(),
        secs_alive: out.secs_alive,
        ended_by_itself: out.ended_by_itself,
        paused: fx.paused(),
        title_ok,
        harness_error,
    };
    let mut report = build_report(&input, &scrub);
    fit(&mut report);
    let mut value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
    if let Some(key) = &cfg.key {
        let sig = sign_request(key, &value);
        value["sig"] = json!(sig);
    }
    call(&cfg.opts, "report.send", json!({"report": value})).await?;
    Ok(RoundOutcome::Reported {
        run,
        outcome: report.outcome,
        first_error: report.first_error,
        note,
    })
}

/// Rounds until the pause file appears. `seen` is told about every round. Returns how many rounds
/// ran. A refusal of the hub ends the loop with its error. Nothing to test waits ten times the
/// interval.
pub async fn run_loop(
    cfg: &RoundConfig,
    fx: &dyn Effects,
    seen: &mut dyn FnMut(&RoundOutcome),
) -> Result<usize, String> {
    let mut rounds = 0;
    loop {
        let o = run_round(cfg, fx).await?;
        seen(&o);
        if o == RoundOutcome::Paused {
            return Ok(rounds);
        }
        rounds += 1;
        let wait = if o == RoundOutcome::NothingToTest {
            cfg.interval_secs.saturating_mul(10)
        } else {
            cfg.interval_secs
        };
        fx.sleep(wait);
    }
}

/// The real effects: the system probe, `gh`, the run script, the clock and the pause file.
pub struct SystemEffects {
    pub run_script: PathBuf,
    pub pause_file: PathBuf,
}

/// The run folder named in the output of the run script.
fn run_folder(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("run folder:"))
        .map(|p| PathBuf::from(p.trim()))
}

/// Metric columns of the run in `$SOMMELIER_RUNS/metrics.tsv` as `name: value` lines.
fn metric_lines(folder: &Path) -> Vec<String> {
    let Some(runs) = std::env::var_os("SOMMELIER_RUNS") else {
        return Vec::new();
    };
    let Ok(tsv) = std::fs::read_to_string(PathBuf::from(runs).join("metrics.tsv")) else {
        return Vec::new();
    };
    let id = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut lines = tsv.lines();
    let Some(head) = lines.next() else {
        return Vec::new();
    };
    let cols: Vec<&str> = head.split('\t').collect();
    let Some(id_col) = cols.iter().position(|c| *c == "run_id") else {
        return Vec::new();
    };
    lines
        .filter_map(|l| {
            let cells: Vec<&str> = l.split('\t').collect();
            (cells.get(id_col) == Some(&id.as_str())).then_some(cells)
        })
        .next_back()
        .map(|cells| {
            cols.iter()
                .zip(cells)
                .filter(|(_, v)| *v != "-")
                .map(|(c, v)| format!("{c}: {v}"))
                .collect()
        })
        .unwrap_or_default()
}

impl Effects for SystemEffects {
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn paused(&self) -> bool {
        self.pause_file.exists()
    }

    fn machine(&self, gpu: Option<&str>) -> MachineClass {
        let runner = SystemRunner::default();
        machine_class_with(&CommandProbe { runner: &runner }, gpu)
    }

    fn verify_build(&self, build: &Path, repo: &str) -> Result<(), String> {
        verify_build(build, repo, &SystemRunner::default())
    }

    fn run_scenario(&self, run: &ScenarioRun<'_>) -> Result<ScenarioOutput, String> {
        let worktree = run
            .build
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let args: Vec<String> = vec![
            self.run_script.display().to_string(),
            worktree.display().to_string(),
            run.seconds.to_string(),
            run.label.to_owned(),
            format!("SOMMELIER_TITLE_EXE={}", run.title.display()),
            format!("SOMMELIER_BUILD_FILE={}", run.build.display()),
        ];
        // The script may wait in a queue behind other runs, so the time is generous.
        let runner = SystemRunner {
            timeout: Duration::from_secs(u64::from(run.seconds) + 3600),
        };
        let out = runner
            .run("bash", &args)
            .map_err(|e| format!("the run script could not run: {e:?}"))?;
        if out.code != Some(0) {
            return Err(format!(
                "the run script failed (exit code {}): check the script and the title settings",
                out.code.map(|c| c.to_string()).unwrap_or("none".into())
            ));
        }
        let folder = run_folder(&out.stdout).ok_or(
            "the run script did not name its run folder: it must print `run folder: <path>`",
        )?;
        let log = std::fs::read(folder.join("title.log"))
            .map(|b| {
                let start = b.len().saturating_sub(256 * 1024);
                String::from_utf8_lossy(&b[start..]).into_owned()
            })
            .map_err(|_| "the run folder has no title.log".to_string())?;
        let run_txt = std::fs::read_to_string(folder.join("run.txt")).unwrap_or_default();
        let ended = run_txt
            .lines()
            .find_map(|l| l.strip_prefix("ended by itself after "))
            .and_then(|r| r.split_whitespace().next())
            .and_then(|n| n.parse::<u32>().ok());
        let mut summary: Vec<String> = out
            .stdout
            .lines()
            .filter(|l| l.contains(':'))
            .map(str::to_owned)
            .collect();
        summary.extend(metric_lines(&folder));
        Ok(ScenarioOutput {
            summary,
            log,
            secs_alive: ended.unwrap_or(run.seconds),
            ended_by_itself: ended.is_some(),
        })
    }

    fn sleep(&self, secs: u64) {
        std::thread::sleep(Duration::from_secs(secs));
    }
}
