//! The state machine of one submission in the clean build: queued, dispatched, running, done.
//!
//! [`step`] is pure. The caller owns the clock and the network: it passes `now` in milliseconds and the
//! events it observed, and it carries out the returned [`Action`]s (send a dispatch, poll GitHub, publish
//! a FACT). A poll answers with an event again. So the machine can be tested as a table.

use serde::{Deserialize, Serialize};

use super::parse::Conclusion;

/// The time settings of one submission, copied into its state when it is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timers {
    /// The first wait before a poll, and the base of the back off, in milliseconds.
    pub first_poll_ms: u64,
    /// The longest wait between two polls.
    pub max_poll_ms: u64,
    /// How long a dispatch may go without a matching run before it counts as lost.
    pub run_wait_ms: u64,
    /// The time limit from the first dispatch to the end. The workflow has its own shorter limit.
    pub time_limit_ms: u64,
}

impl Default for Timers {
    fn default() -> Self {
        Self {
            first_poll_ms: 5_000,
            max_poll_ms: 60_000,
            run_wait_ms: 180_000,
            time_limit_ms: 30 * 60_000,
        }
    }
}

/// Where a submission is in the clean build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Waiting for a slot ([`crate::verify::next_to_verify`]).
    Queued,
    /// The workflow was started and no run was seen yet.
    Dispatched,
    /// A run exists and has not ended.
    Running,
    /// The run ended, or the verifier gave up. Final.
    Done,
}

impl Phase {
    /// The word used in the FACT body.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Dispatched => "dispatched",
            Phase::Running => "running",
            Phase::Done => "done",
        }
    }
}

/// The whole state of one submission. Plain data, so the hub can store it and survive a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyState {
    /// The SHA 256 of the bundle.
    pub submission: String,
    /// The order id.
    pub order: String,
    /// The phase.
    pub phase: Phase,
    /// The time settings.
    pub timers: Timers,
    /// When the submission entered the queue, milliseconds.
    pub queued_at: u64,
    /// When the first dispatch went out.
    pub started_at: Option<u64>,
    /// When the latest dispatch went out.
    pub dispatched_at: Option<u64>,
    /// How many dispatches went out, at most 2.
    pub dispatches: u8,
    /// The run id once a run was seen.
    pub run_id: Option<u64>,
    /// How many polls since the last reset. It drives the back off.
    pub polls: u32,
    /// The earliest time of the next poll.
    pub next_poll_at: u64,
    /// The final conclusion word: a [`Conclusion`] word, or `no_run` for a dispatch that never produced a run.
    pub conclusion: Option<String>,
    /// The name of the failing step, when known.
    pub failing_step: Option<String>,
    /// The billed minutes, when known.
    pub minutes: u32,
}

impl VerifyState {
    /// A new submission in the queue.
    pub fn new(submission: &str, order: &str, queued_at: u64, timers: Timers) -> Self {
        Self {
            submission: submission.to_owned(),
            order: order.to_owned(),
            phase: Phase::Queued,
            timers,
            queued_at,
            started_at: None,
            dispatched_at: None,
            dispatches: 0,
            run_id: None,
            polls: 0,
            next_poll_at: 0,
            conclusion: None,
            failing_step: None,
            minutes: 0,
        }
    }

    /// The FACT body that describes this state now.
    pub fn fact(&self) -> FactBody {
        FactBody {
            v: 1,
            submission: self.submission.clone(),
            run_id: self.run_id,
            state: self.phase.as_str().to_owned(),
            conclusion: self.conclusion.clone(),
            failing_step: self.failing_step.clone(),
            minutes: self.minutes,
        }
    }
}

/// The body of a FACT on the topic `verify/<sha256>`.
///
/// `conclusion` and `failing_step` stay `null` until the run ends. `minutes` is 0 until it is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactBody {
    /// The body version, 1.
    pub v: u32,
    /// The SHA 256 of the bundle.
    pub submission: String,
    /// The workflow run id, once known.
    pub run_id: Option<u64>,
    /// `dispatched`, `running` or `done` (also `queued` for a body made before the dispatch).
    pub state: String,
    /// `passed`, `failed`, `cancelled`, `timed_out` or `no_run`, only when `state` is `done`.
    pub conclusion: Option<String>,
    /// The name of the failing step when the conclusion is `failed`.
    pub failing_step: Option<String>,
    /// The billed runner minutes.
    pub minutes: u32,
}

/// The FACT topic of a submission.
pub fn fact_topic(submission: &str) -> String {
    format!("verify/{submission}")
}

/// What the caller observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The scheduler picked this submission ([`crate::verify::next_to_verify`]).
    Start,
    /// Time passed. The caller sends a tick whenever it wakes, for example once a second.
    Tick,
    /// GitHub refused the dispatch, or the call failed.
    DispatchFailed,
    /// A poll listed the runs and none carried the submission.
    RunNotFound,
    /// A poll read the run: its id, what [`crate::verify::conclusion`] made of it, and the billed minutes.
    RunSeen {
        /// The run id.
        run_id: u64,
        /// The reading of the run.
        conclusion: Conclusion,
        /// The billed minutes so far.
        minutes: u32,
    },
    /// A poll call failed. The machine waits longer.
    PollFailed,
}

/// What the caller must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start the workflow. `attempt` is 1, or 2 for the single retry.
    Dispatch {
        /// The SHA 256 of the bundle.
        submission: String,
        /// The order id.
        order: String,
        /// 1 or 2.
        attempt: u8,
    },
    /// Read GitHub. With a run id: get the run and its jobs. Without: list the runs and `find_run`.
    Poll {
        /// The SHA 256 of the bundle.
        submission: String,
        /// The run id when known.
        run_id: Option<u64>,
    },
    /// Publish a FACT.
    Emit {
        /// `verify/<sha256>`.
        topic: String,
        /// The body.
        body: FactBody,
    },
}

/// The wait before poll number `n` (from 1): `first * 2^(n-1)`, capped at `max`.
fn backoff(t: &Timers, n: u32) -> u64 {
    let shift = n.saturating_sub(1).min(32);
    t.first_poll_ms
        .saturating_mul(1u64 << shift)
        .min(t.max_poll_ms)
}

fn emit(s: &VerifyState) -> Action {
    Action::Emit {
        topic: fact_topic(&s.submission),
        body: s.fact(),
    }
}

fn dispatch_action(s: &VerifyState) -> Action {
    Action::Dispatch {
        submission: s.submission.clone(),
        order: s.order.clone(),
        attempt: s.dispatches,
    }
}

/// Sends a dispatch, first or second, and arms the poll timer.
fn send_dispatch(mut s: VerifyState, now: u64) -> (VerifyState, Vec<Action>) {
    s.dispatches += 1;
    s.dispatched_at = Some(now);
    s.started_at.get_or_insert(now);
    s.polls = 0;
    s.next_poll_at = now.saturating_add(s.timers.first_poll_ms);
    s.phase = Phase::Dispatched;
    let mut actions = vec![dispatch_action(&s)];
    if s.dispatches == 1 {
        actions.push(emit(&s));
    }
    (s, actions)
}

/// The dispatch produced no run: one retry, then give up with `no_run`.
fn retry_or_give_up(s: VerifyState, now: u64) -> (VerifyState, Vec<Action>) {
    if s.dispatches < 2 {
        send_dispatch(s, now)
    } else {
        finish(s, now, "no_run", None, None)
    }
}

/// Ends the submission and emits the final FACT. `minutes` of `None` means the time since the first dispatch.
fn finish(
    mut s: VerifyState,
    now: u64,
    word: &str,
    failing_step: Option<String>,
    minutes: Option<u32>,
) -> (VerifyState, Vec<Action>) {
    s.phase = Phase::Done;
    s.conclusion = Some(word.to_owned());
    s.failing_step = failing_step;
    s.minutes = minutes.unwrap_or_else(|| {
        let ms = now.saturating_sub(s.started_at.unwrap_or(now));
        u32::try_from(ms.div_ceil(60_000)).unwrap_or(u32::MAX)
    });
    let actions = vec![emit(&s)];
    (s, actions)
}

/// One transition. Pure: the same state, time and event always give the same result.
///
/// * `Queued`: only `Start` acts. It dispatches and moves to `Dispatched`.
/// * `Dispatched` and `Running`: a `Tick` first checks the time limit. Then, in `Dispatched`, a dispatch
///   older than `run_wait_ms` without a run is retried once, and the second loss ends as `no_run`. Then a
///   due poll is requested and the next one is set with exponential back off. `PollFailed` backs off
///   once more. `RunSeen` with a final conclusion ends the submission. Without one it moves to `Running`.
/// * `DispatchFailed` takes the same road as a lost dispatch.
/// * `Done` ignores every event.
pub fn step(state: &VerifyState, now: u64, event: Event) -> (VerifyState, Vec<Action>) {
    let s = state.clone();
    let active = matches!(s.phase, Phase::Dispatched | Phase::Running);
    match (s.phase, event) {
        (Phase::Queued, Event::Start) => send_dispatch(s, now),
        (Phase::Dispatched, Event::DispatchFailed) => retry_or_give_up(s, now),
        (
            _,
            Event::RunSeen {
                run_id,
                conclusion,
                minutes,
            },
        ) if active && conclusion.is_final() => {
            let word = conclusion.as_str();
            let step_name = match &conclusion {
                Conclusion::Failed { step } => step.clone(),
                _ => None,
            };
            let mut s = s;
            s.run_id = Some(run_id);
            finish(s, now, word, step_name, Some(minutes))
        }
        (
            Phase::Dispatched,
            Event::RunSeen {
                run_id, minutes, ..
            },
        ) => {
            let mut s = s;
            s.phase = Phase::Running;
            s.run_id = Some(run_id);
            s.minutes = minutes;
            s.polls = 0;
            s.next_poll_at = now.saturating_add(s.timers.first_poll_ms);
            let actions = vec![emit(&s)];
            (s, actions)
        }
        (
            Phase::Running,
            Event::RunSeen {
                run_id, minutes, ..
            },
        ) => {
            let mut s = s;
            s.run_id = Some(run_id);
            s.minutes = minutes;
            (s, Vec::new())
        }
        (_, Event::PollFailed) if active => {
            let mut s = s;
            s.polls = s.polls.saturating_add(1);
            s.next_poll_at = now.saturating_add(backoff(&s.timers, s.polls));
            (s, Vec::new())
        }
        (_, Event::Tick) if active => tick(s, now),
        _ => (s, Vec::new()),
    }
}

fn tick(mut s: VerifyState, now: u64) -> (VerifyState, Vec<Action>) {
    let started = s.started_at.unwrap_or(now);
    if now.saturating_sub(started) >= s.timers.time_limit_ms {
        return finish(s, now, "timed_out", None, None);
    }
    if s.phase == Phase::Dispatched
        && s.run_id.is_none()
        && now.saturating_sub(s.dispatched_at.unwrap_or(now)) >= s.timers.run_wait_ms
    {
        return retry_or_give_up(s, now);
    }
    if now < s.next_poll_at {
        return (s, Vec::new());
    }
    s.polls = s.polls.saturating_add(1);
    s.next_poll_at = now.saturating_add(backoff(&s.timers, s.polls));
    let actions = vec![Action::Poll {
        submission: s.submission.clone(),
        run_id: s.run_id,
    }];
    (s, actions)
}
