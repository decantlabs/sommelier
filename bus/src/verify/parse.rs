//! Parsers for the GitHub answers and the reading of a run into a [`Conclusion`].
//!
//! Only the fields the verifier needs are read. Unknown fields are ignored. Everything here is pure.

use serde::Deserialize;

/// One workflow run, as far as the verifier cares.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Run {
    /// The run id.
    pub id: u64,
    /// The name of the workflow. The run name is in `display_title`.
    #[serde(default)]
    pub name: Option<String>,
    /// The display title. GitHub fills it from `run-name`, so [`find_run`] reads it first.
    #[serde(default)]
    pub display_title: Option<String>,
    /// `queued`, `in_progress`, `completed` and a few more.
    pub status: String,
    /// Set when `status` is `completed`.
    #[serde(default)]
    pub conclusion: Option<String>,
    /// When the run was created, RFC 3339.
    #[serde(default)]
    pub created_at: Option<String>,
    /// When the run started, RFC 3339.
    #[serde(default)]
    pub run_started_at: Option<String>,
    /// The last update, RFC 3339.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// One step of a job.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct JobStep {
    /// The step name from the workflow file.
    pub name: String,
    /// The position in the job, from 1.
    #[serde(default)]
    pub number: u32,
    /// `queued`, `in_progress` or `completed`.
    #[serde(default)]
    pub status: String,
    /// Set when the step is completed.
    #[serde(default)]
    pub conclusion: Option<String>,
}

/// One job of a run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Job {
    /// The job name.
    pub name: String,
    /// `queued`, `in_progress` or `completed`.
    #[serde(default)]
    pub status: String,
    /// Set when the job is completed.
    #[serde(default)]
    pub conclusion: Option<String>,
    /// When the job started, RFC 3339.
    #[serde(default)]
    pub started_at: Option<String>,
    /// When the job completed, RFC 3339.
    #[serde(default)]
    pub completed_at: Option<String>,
    /// The steps in order.
    #[serde(default)]
    pub steps: Vec<JobStep>,
}

#[derive(Deserialize)]
struct RunList {
    workflow_runs: Vec<Run>,
}

#[derive(Deserialize)]
struct JobList {
    jobs: Vec<Job>,
}

/// Parses the answer of the list of runs of a workflow.
pub fn parse_runs(json: &str) -> Result<Vec<Run>, String> {
    serde_json::from_str::<RunList>(json)
        .map(|l| l.workflow_runs)
        .map_err(|e| format!("invalid workflow runs answer: {e}"))
}

/// Parses the answer for one run.
pub fn parse_run(json: &str) -> Result<Run, String> {
    serde_json::from_str(json).map_err(|e| format!("invalid workflow run answer: {e}"))
}

/// Parses the answer for the jobs of a run.
pub fn parse_jobs(json: &str) -> Result<Vec<Job>, String> {
    serde_json::from_str::<JobList>(json)
        .map(|l| l.jobs)
        .map_err(|e| format!("invalid jobs answer: {e}"))
}

/// The title of a run. GitHub puts the `run-name` of the template into `display_title` and the name of
/// the workflow into `name`, so `display_title` is read first.
fn run_title(run: &Run) -> &str {
    run.display_title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .or(run.name.as_deref())
        .unwrap_or("")
}

/// Finds the run that the hub waits for by its whole name, see [`crate::verify::request::run_name`].
///
/// The name carries the submission, the attempt of the order, the dispatch count and the start of the
/// round. Only a run with exactly this name matches, so the run of an earlier round or of an earlier
/// attempt never does. When several runs match, the newest id wins.
pub fn find_run<'a>(runs: &'a [Run], expected_name: &str) -> Option<&'a Run> {
    let want: Vec<&str> = expected_name.split_whitespace().collect();
    if want.is_empty() {
        return None;
    }
    runs.iter()
        .filter(|r| run_title(r).split_whitespace().eq(want.iter().copied()))
        .max_by_key(|r| r.id)
}

/// What the hub makes of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conclusion {
    /// Waiting for a runner.
    Queued,
    /// A runner works on it.
    Running,
    /// Every job succeeded.
    Passed,
    /// A job failed. `step` is the name of the first failing step when the jobs show one.
    Failed {
        /// The first failing step, trimmed and without control characters.
        step: Option<String>,
    },
    /// Somebody or something cancelled the run.
    Cancelled,
    /// A time limit ended the run.
    TimedOut,
}

impl Conclusion {
    /// The word used in the FACT body.
    pub fn as_str(&self) -> &'static str {
        match self {
            Conclusion::Queued => "queued",
            Conclusion::Running => "running",
            Conclusion::Passed => "passed",
            Conclusion::Failed { .. } => "failed",
            Conclusion::Cancelled => "cancelled",
            Conclusion::TimedOut => "timed_out",
        }
    }

    /// True when the run has ended.
    pub fn is_final(&self) -> bool {
        !matches!(self, Conclusion::Queued | Conclusion::Running)
    }
}

fn clean_step_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// The first step (by job order, then step number) that failed or timed out.
fn failing_step(jobs: &[Job]) -> Option<String> {
    for job in jobs {
        let mut steps: Vec<&JobStep> = job.steps.iter().collect();
        steps.sort_by_key(|s| s.number);
        if let Some(s) = steps
            .iter()
            .find(|s| matches!(s.conclusion.as_deref(), Some("failure" | "timed_out")))
        {
            return Some(clean_step_name(&s.name));
        }
    }
    None
}

/// Reads a run and its jobs into a [`Conclusion`].
///
/// Fail closed: every completed conclusion other than `success`, `cancelled` and `timed_out` counts as
/// failed. A run that failed while a job or step reports `timed_out` is read as timed out.
pub fn conclusion(run: &Run, jobs: &[Job]) -> Conclusion {
    match run.status.as_str() {
        "completed" => {}
        "in_progress" => return Conclusion::Running,
        _ => return Conclusion::Queued,
    }
    match run.conclusion.as_deref() {
        Some("success") => Conclusion::Passed,
        Some("cancelled") => Conclusion::Cancelled,
        Some("timed_out") => Conclusion::TimedOut,
        _ => {
            let timed_out = jobs.iter().any(|j| {
                j.conclusion.as_deref() == Some("timed_out")
                    || j.steps
                        .iter()
                        .any(|s| s.conclusion.as_deref() == Some("timed_out"))
            });
            if timed_out {
                Conclusion::TimedOut
            } else {
                Conclusion::Failed {
                    step: failing_step(jobs),
                }
            }
        }
    }
}

/// Parses `2026-11-25T10:00:00Z` (GitHub) or the millisecond form of [`crate::time::rfc3339`].
fn parse_github_ms(ts: &str) -> Option<u64> {
    if ts.len() == 20 && ts.ends_with('Z') {
        let with_ms = format!("{}.000Z", ts.get(..19)?);
        crate::time::parse_ms(&with_ms)
    } else {
        crate::time::parse_ms(ts)
    }
}

/// The billed minutes of a run: every job's duration rounded up to a whole minute, summed.
/// A job without both timestamps counts as zero.
pub fn job_minutes(jobs: &[Job]) -> u32 {
    jobs.iter()
        .filter_map(|j| {
            let a = parse_github_ms(j.started_at.as_deref()?)?;
            let b = parse_github_ms(j.completed_at.as_deref()?)?;
            let ms = b.saturating_sub(a);
            Some(u32::try_from(ms.div_ceil(60_000)).unwrap_or(u32::MAX))
        })
        .fold(0u32, |acc, m| acc.saturating_add(m))
}
