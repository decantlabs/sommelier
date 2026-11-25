//! Building a test report from one run.
//!
//! [`build_report`] is a pure function. The caller reads the run folder of `swarm/run-title.sh`, passes
//! the summary lines and the log text in, and signs the report it gets back.
//!
//! Formats it relies on, both from the run script:
//!
//! * A log line starts with the second of the run, for example `12.5 swapchain created`. A line without a
//!   leading number belongs to the second of the line before it.
//! * A summary line is `name: value`. The value is the first word that is a number, so
//!   `frames: 1234 at 12.3 s` gives `frames` as 1234. Columns of `metrics.tsv` such as `fps_median` can be
//!   passed as `fps_median: 58.2` lines. A value of `-` is no value.

use std::collections::BTreeMap;

use regex::{Regex, RegexBuilder};

use super::profile::{Profile, Scenario};
use crate::report::types::{MachineClass, Outcome, Report};

/// The excerpt of the log in a report is cut to this many bytes.
pub const EXCERPT_MAX: usize = 16 * 1024;

/// How much of the raw log tail goes through the scrub function. It is more than the excerpt, because a
/// scrub function can shorten text.
const SCRUB_WINDOW: usize = 4 * EXCERPT_MAX;

/// Error classes, the first line of the log that matches one of them names the first error.
const ERROR_TABLE: &[(&str, &str)] = &[
    (
        "device_removed",
        r"(?i)DEVICE_REMOVED|DEVICE_HUNG|device removed|device is gone|device was removed|Removing Device",
    ),
    ("unhandled_exception", r"UNHANDLED"),
    (
        "out_of_memory",
        r"(?i)out of memory|OUT_OF_MEMORY|E_OUTOFMEMORY|bad_alloc",
    ),
];

/// Everything the report needs from one run.
pub struct RunInput<'a> {
    /// Id of the run order.
    pub run: &'a str,
    pub member: &'a str,
    /// Commit of the build that ran.
    pub build: &'a str,
    /// `main` or `candidate`.
    pub channel: &'a str,
    pub profile: &'a Profile,
    /// Id of the scenario in the profile.
    pub scenario: &'a str,
    pub machine: MachineClass,
    /// Lines of `summary.txt` and any `name: value` metric lines.
    pub summary: &'a [String],
    /// Text of `title.log`.
    pub log: &'a str,
    /// UTC epoch seconds when the run ended.
    pub at: u64,
    /// Seconds the title was alive.
    pub secs_alive: u32,
    /// The run folder says the title ended by itself, before the time was up.
    pub ended_by_itself: bool,
    /// The pause file was set, so the run says nothing about the build.
    pub paused: bool,
    /// The local title check said no.
    pub title_ok: bool,
    /// The harness failed (script error, no log, no start).
    pub harness_error: bool,
}

fn error_table() -> Vec<(&'static str, Regex)> {
    ERROR_TABLE
        .iter()
        .map(|(name, pat)| {
            (
                *name,
                Regex::new(pat).expect("error table pattern compiles"),
            )
        })
        .collect()
}

/// The leading second of a log line, if it has one.
fn line_second(line: &str) -> Option<f64> {
    let first = line.split_whitespace().next()?;
    let v: f64 = first.parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// The first value of a summary line `name: value ...`.
fn summary_value(line: &str) -> Option<(String, f64)> {
    let (k, v) = line.split_once(':')?;
    let name = k.trim().to_lowercase().replace(' ', "_");
    let word = v.split_whitespace().next()?;
    let n: f64 = word.parse().ok()?;
    n.is_finite().then_some((name, n))
}

/// Walks the log once and finds, for every milestone, the second of its first matching line.
fn milestone_seconds(sc: &Scenario, log: &str) -> Vec<Option<f64>> {
    let res: Vec<Regex> = sc
        .milestones
        .iter()
        .map(|m| {
            RegexBuilder::new(&m.pattern)
                .size_limit(1 << 20)
                .build()
                .unwrap_or_else(|_| Regex::new("$^").expect("never matches"))
        })
        .collect();
    let mut seen: Vec<Option<f64>> = vec![None; res.len()];
    let mut now = 0.0;
    for line in log.lines() {
        if let Some(t) = line_second(line) {
            now = t;
        }
        for (i, re) in res.iter().enumerate() {
            if seen[i].is_none() && re.is_match(line) {
                seen[i] = Some(now);
            }
        }
    }
    seen
}

/// The class of the first error line in the log, by position in the log.
fn first_log_error(log: &str) -> Option<&'static str> {
    let table = error_table();
    log.lines().find_map(|line| {
        table
            .iter()
            .find(|(_, re)| re.is_match(line))
            .map(|(n, _)| *n)
    })
}

/// Cuts text to at most `max` bytes from the end, on a line boundary and on a character boundary.
fn tail_on_line(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    // The cut is inside a line unless the byte before it ends a line. Drop the partial line.
    if text.as_bytes()[start - 1] == b'\n' {
        return &text[start..];
    }
    match text[start..].find('\n') {
        Some(nl) => &text[start + nl + 1..],
        None => "",
    }
}

/// The scrubbed tail of the log, at most [`EXCERPT_MAX`] bytes, ending on a line boundary.
fn excerpt(log: &str, scrub: &dyn Fn(&str) -> String) -> String {
    let window = tail_on_line(log, SCRUB_WINDOW);
    let mut scrubbed = String::with_capacity(window.len().min(2 * EXCERPT_MAX));
    for line in window.lines() {
        scrubbed.push_str(&scrub(line));
        scrubbed.push('\n');
    }
    tail_on_line(&scrubbed, EXCERPT_MAX).to_string()
}

/// Builds the report of one run. It is not signed: `sig` is `None`.
///
/// * `Inconclusive` when the run was paused, the title check failed, the harness failed or the scenario
///   is not in the profile.
/// * `Fails` when a milestone was missed or late, or the title ended before the scenario time.
/// * `Works` when every milestone appeared in time.
///
/// The first error is the class of the first matching log line (`device_removed`, `unhandled_exception`,
/// `out_of_memory`). Without such a line it is `exit_early` for a title that ended early and `timeout`
/// for a missed milestone. A run that works has none.
pub fn build_report(input: &RunInput<'_>, scrub: &dyn Fn(&str) -> String) -> Report {
    let scenario = input.profile.scenario(input.scenario);
    let mut metrics = BTreeMap::new();
    let mut outcome = Outcome::Inconclusive;
    let mut first_error = None;

    if let Some(sc) = scenario
        && !input.paused
        && input.title_ok
        && !input.harness_error
    {
        let seen = milestone_seconds(sc, input.log);
        let missed = sc
            .milestones
            .iter()
            .zip(&seen)
            .any(|(m, t)| t.is_none_or(|t| t > f64::from(m.by)));
        let early = input.ended_by_itself && input.secs_alive < sc.seconds;
        if missed || early {
            outcome = Outcome::Fails;
            first_error = Some(
                first_log_error(input.log)
                    .unwrap_or(if early { "exit_early" } else { "timeout" })
                    .to_string(),
            );
        } else {
            outcome = Outcome::Works;
        }
        let wanted: Vec<&String> = sc.metrics.iter().collect();
        for line in input.summary {
            if let Some((name, v)) = summary_value(line)
                && wanted.iter().any(|w| **w == name)
            {
                metrics.entry(name).or_insert(v);
            }
        }
    }

    Report {
        v: 1,
        run: input.run.to_string(),
        member: input.member.to_string(),
        build: input.build.to_string(),
        channel: input.channel.to_string(),
        profile: input.profile.id.clone(),
        scenario: input.scenario.to_string(),
        machine: input.machine.clone(),
        outcome,
        first_error,
        metrics,
        secs_alive: input.secs_alive,
        log_excerpt: excerpt(input.log, scrub),
        at: input.at,
        sig: None,
    }
}
