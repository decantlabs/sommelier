//! Hub side of the clean build: the verify queue, starting runs and reading their conclusions.
//!
//! The tick picks gated submissions with [`next_to_verify`], drives each one with the pure [`step`]
//! and carries out its actions. The GitHub calls run on tasks of their own, off the state lock, and
//! park their answer as an event for the next tick. Every change is a FACT `verify/<sha256>`. A
//! restart rebuilds the machines from the VERIFY rows.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use super::api::{ApiError, GitHubApi, ReqwestGitHub};
use super::machine::{Action, Event, Phase, VerifyState, step};
use super::parse::{Job, Run, conclusion, find_run, job_minutes};
use super::queue::{Entry, VerifyRules, next_to_verify};
use super::request::DispatchInputs;
use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::intake::facts::{
    SubRow, fact, one_line, query_by_states, query_submission, requeue_message, submission_fact,
};
use crate::msg::{Message, Role};

/// A boxed future, so the API can be used through a trait object.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The four calls of [`GitHubApi`] in a form that works as a trait object. Every `GitHubApi` that is
/// `Send + Sync` is one. A test passes a [`super::FakeGitHub`] through [`VerifyConfig::api`].
pub trait DynGitHub: Send + Sync {
    fn dispatch<'a>(
        &'a self,
        repo: &'a str,
        workflow_file: &'a str,
        git_ref: &'a str,
        inputs: &'a DispatchInputs,
    ) -> BoxFut<'a, Result<(), ApiError>>;
    fn list_runs<'a>(
        &'a self,
        repo: &'a str,
        workflow_file: &'a str,
    ) -> BoxFut<'a, Result<Vec<Run>, ApiError>>;
    fn get_run<'a>(&'a self, repo: &'a str, run_id: u64) -> BoxFut<'a, Result<Run, ApiError>>;
    fn jobs<'a>(&'a self, repo: &'a str, run_id: u64) -> BoxFut<'a, Result<Vec<Job>, ApiError>>;
}

impl<T: GitHubApi + Send + Sync> DynGitHub for T {
    fn dispatch<'a>(
        &'a self,
        repo: &'a str,
        workflow_file: &'a str,
        git_ref: &'a str,
        inputs: &'a DispatchInputs,
    ) -> BoxFut<'a, Result<(), ApiError>> {
        Box::pin(GitHubApi::dispatch(
            self,
            repo,
            workflow_file,
            git_ref,
            inputs,
        ))
    }
    fn list_runs<'a>(
        &'a self,
        repo: &'a str,
        workflow_file: &'a str,
    ) -> BoxFut<'a, Result<Vec<Run>, ApiError>> {
        Box::pin(GitHubApi::list_runs(self, repo, workflow_file))
    }
    fn get_run<'a>(&'a self, repo: &'a str, run_id: u64) -> BoxFut<'a, Result<Run, ApiError>> {
        Box::pin(GitHubApi::get_run(self, repo, run_id))
    }
    fn jobs<'a>(&'a self, repo: &'a str, run_id: u64) -> BoxFut<'a, Result<Vec<Job>, ApiError>> {
        Box::pin(GitHubApi::jobs(self, repo, run_id))
    }
}

/// A shared GitHub API with a `Debug` that shows nothing.
#[derive(Clone)]
pub struct SharedApi(pub Arc<dyn DynGitHub>);

impl std::fmt::Debug for SharedApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedApi")
    }
}

/// One machine of a submission in the clean build, with what the tick needs to report on it.
#[derive(Debug)]
struct Slot {
    st: VerifyState,
    row: SubRow,
    entry: Entry,
}

/// State that lives as long as the hub and is not part of the log.
#[derive(Debug, Default)]
pub(crate) struct Runtime {
    slots: Mutex<HashMap<String, Slot>>,
    /// Answers of GitHub calls that wait for the next tick.
    inbox: Mutex<Vec<(String, Event)>>,
    /// The API built from the token file, made on first use.
    api: Mutex<Option<SharedApi>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Settings of this part of the hub. Lives in `HubConfig.verify`. Everything is off by default.
#[derive(Clone, Debug)]
pub struct VerifyConfig {
    /// Master switch. Off: a gated submission stays `gated`.
    pub enabled: bool,
    /// The repository of the project, `owner/name`.
    pub repository: String,
    /// The file name of the workflow, for example `factory-verify.yml`.
    pub workflow_file: String,
    /// The git ref the workflow runs from. The workflow file comes from there, so use main.
    pub git_ref: String,
    /// A file that holds the GitHub token. Read when the first run starts, never logged.
    pub token_file: Option<PathBuf>,
    /// The public address of the hub, `https://...`, which the runner fetches the bundle from.
    pub public_address: String,
    /// How many runs at once and the share of tier 0.
    pub rules: VerifyRules,
    /// Poll back off and time limits.
    pub timers: super::machine::Timers,
    /// A test passes a fake API here. None: the real one from `token_file`.
    pub api: Option<SharedApi>,
    pub(crate) runtime: Arc<Runtime>,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repository: String::new(),
            workflow_file: "factory-verify.yml".into(),
            git_ref: "main".into(),
            token_file: None,
            public_address: String::new(),
            rules: VerifyRules::default(),
            timers: super::machine::Timers::default(),
            api: None,
            runtime: Arc::default(),
        }
    }
}

impl VerifyConfig {
    /// The API to use, built from the token file on first use. `None` when it cannot be built.
    fn api(&self) -> Option<SharedApi> {
        if let Some(a) = &self.api {
            return Some(a.clone());
        }
        let mut cached = lock(&self.runtime.api);
        if cached.is_none() {
            let path = self.token_file.as_ref()?;
            let token = std::fs::read_to_string(path).ok()?;
            let api = ReqwestGitHub::new(token.trim()).ok()?;
            *cached = Some(SharedApi(Arc::new(api)));
        }
        cached.clone()
    }
}

/// One row of the VERIFY table.
#[derive(Clone, Debug)]
struct VerifyRow {
    state: String,
    run_id: Option<u64>,
    conclusion: Option<String>,
    failing_step: Option<String>,
    minutes: u32,
    dispatches: u32,
    started_ms: u64,
}

fn query_verify(conn: &Connection, sha: &str) -> Option<VerifyRow> {
    conn.query_row(
        "SELECT state, run_id, conclusion, failing_step, minutes, dispatches, started_ms
         FROM VERIFY WHERE sha256 = ?1",
        params![sha],
        |r| {
            Ok(VerifyRow {
                state: r.get(0)?,
                run_id: r.get::<_, Option<i64>>(1)?.map(|n| n.max(0) as u64),
                conclusion: r.get(2)?,
                failing_step: r.get(3)?,
                minutes: r.get::<_, i64>(4)?.max(0) as u32,
                dispatches: r.get::<_, i64>(5)?.max(0) as u32,
                started_ms: r.get::<_, i64>(6)?.max(0) as u64,
            })
        },
    )
    .optional()
    .unwrap_or(None)
}

/// The FACT `verify/<sha256>` for a machine: the body of the machine plus what a restart needs.
fn verify_fact(slot: &Slot, now_sec: u64) -> Message {
    let s = &slot.st;
    let mut body = serde_json::to_value(s.fact()).unwrap_or(Value::Null);
    body["order"] = json!(s.order);
    body["dispatches"] = json!(s.dispatches);
    body["started_ms"] = json!(s.started_at.unwrap_or(0));
    body["at"] = json!(now_sec);
    fact(super::machine::fact_topic(&s.submission), &body)
}

/// Builds the machine again from the VERIFY row of a submission that is `verifying`.
fn rebuild(row: &SubRow, v: Option<&VerifyRow>, cfg: &VerifyConfig, now: u64) -> VerifyState {
    let mut st = VerifyState::new(&row.sha256, &row.task, row.updated_at * 1000, cfg.timers);
    let Some(v) = v else {
        return st;
    };
    st.run_id = v.run_id;
    st.minutes = v.minutes;
    match v.state.as_str() {
        "dispatched" | "running" => {
            st.phase = if v.state == "running" {
                Phase::Running
            } else {
                Phase::Dispatched
            };
            st.dispatches = u8::try_from(v.dispatches.clamp(1, 2)).unwrap_or(1);
            st.started_at = Some(if v.started_ms > 0 { v.started_ms } else { now });
            st.dispatched_at = Some(now);
            st.next_poll_at = now;
        }
        "done" => {
            st.phase = Phase::Done;
            st.dispatches = u8::try_from(v.dispatches.clamp(1, 2)).unwrap_or(1);
            st.started_at = Some(v.started_ms);
            st.conclusion = v.conclusion.clone();
            st.failing_step = v.failing_step.clone();
        }
        _ => {}
    }
    st
}

/// The resume note for an order whose clean build did not pass.
fn failure_note(conclusion: &str, step: Option<&str>) -> String {
    match (conclusion, step) {
        ("failed", Some(s)) => format!(
            "the clean build failed in the step `{s}`, run the commands of the order locally, fix this and submit again"
        ),
        ("failed", None) => "the clean build failed, run the commands of the order locally, fix this and submit again".into(),
        ("cancelled", _) => "the clean build was cancelled, submit again".into(),
        ("timed_out", _) => "the clean build ran out of time, make the change smaller or faster and submit again".into(),
        ("no_run", _) => "the clean build did not start after two tries, submit again later".into(),
        (other, _) => format!("the clean build ended with `{other}`, submit again"),
    }
}

/// Everything one tick decided, collected while the slots are locked and carried out after.
#[derive(Default)]
struct Plan {
    messages: Vec<Message>,
    jobs: Vec<Call>,
    /// Submissions whose build did not pass: the row, the conclusion and the failing step.
    failures: Vec<(SubRow, String, Option<String>)>,
}

enum Call {
    Dispatch(DispatchInputs),
    Poll { sha: String, run_id: Option<u64> },
}

/// Applies one event to a slot and turns the actions into facts and calls.
fn drive(slot: &mut Slot, now: u64, event: Event, plan: &mut Plan, announce: bool) {
    let before = slot.st.clone();
    let (next, actions) = step(&before, now, event);
    slot.st = next;
    let sec = now / 1000;
    let mut emitted = false;
    for a in actions {
        match a {
            Action::Emit { .. } => {
                emitted = true;
                plan.messages.push(verify_fact(slot, sec));
            }
            Action::Dispatch { .. } => {}
            Action::Poll { submission, run_id } => plan.jobs.push(Call::Poll {
                sha: submission,
                run_id,
            }),
        }
    }
    if slot.st.dispatches > before.dispatches {
        // A second dispatch has no fact of its own in the machine, the table keeps the count.
        if !emitted {
            plan.messages.push(verify_fact(slot, sec));
        }
        plan.jobs.push(Call::Dispatch(DispatchInputs {
            submission: slot.row.sha256.clone(),
            order: slot.row.task.clone(),
            base: slot.row.base.clone(),
            hub: String::new(),
        }));
    }
    if announce && before.phase == Phase::Queued && slot.st.phase != Phase::Queued {
        plan.messages
            .push(submission_fact(&slot.row, "verifying", sec));
    }
    if before.phase != Phase::Done && slot.st.phase == Phase::Done {
        let word = slot.st.conclusion.clone().unwrap_or_default();
        if word == "passed" {
            plan.messages
                .push(submission_fact(&slot.row, "verified", sec));
        } else {
            plan.messages
                .push(submission_fact(&slot.row, "verify_failed", sec));
            plan.failures
                .push((slot.row.clone(), word, slot.st.failing_step.clone()));
        }
    }
}

fn spawn<F: Future<Output = ()> + Send + 'static>(fut: F) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
    } else {
        std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(fut);
            }
        });
    }
}

/// One poll: find the run, read it and its jobs, say what the machine should learn.
async fn poll(
    api: &dyn DynGitHub,
    repo: &str,
    workflow: &str,
    sha: &str,
    run_id: Option<u64>,
) -> Event {
    let run = match run_id {
        Some(id) => match api.get_run(repo, id).await {
            Ok(r) => r,
            Err(_) => return Event::PollFailed,
        },
        None => match api.list_runs(repo, workflow).await {
            Ok(runs) => match find_run(&runs, sha) {
                Some(r) => r.clone(),
                None => return Event::RunNotFound,
            },
            Err(_) => return Event::PollFailed,
        },
    };
    let Ok(jobs) = api.jobs(repo, run.id).await else {
        return Event::PollFailed;
    };
    Event::RunSeen {
        run_id: run.id,
        conclusion: conclusion(&run, &jobs),
        minutes: job_minutes(&jobs),
    }
}

fn launch(cfg: &VerifyConfig, api: SharedApi, calls: Vec<Call>) {
    for call in calls {
        let (api, rt) = (api.clone(), cfg.runtime.clone());
        let (repo, workflow, git_ref) = (
            cfg.repository.clone(),
            cfg.workflow_file.clone(),
            cfg.git_ref.clone(),
        );
        match call {
            Call::Dispatch(mut inputs) => {
                inputs.hub = cfg.public_address.clone();
                spawn(async move {
                    if api
                        .0
                        .dispatch(&repo, &workflow, &git_ref, &inputs)
                        .await
                        .is_err()
                    {
                        lock(&rt.inbox).push((inputs.submission, Event::DispatchFailed));
                    }
                });
            }
            Call::Poll { sha, run_id } => spawn(async move {
                let event = poll(api.0.as_ref(), &repo, &workflow, &sha, run_id).await;
                lock(&rt.inbox).push((sha, event));
            }),
        }
    }
}

/// What the tick reads from the tables before it touches any machine.
struct Snapshot {
    gated: Vec<SubRow>,
    verifying: Vec<(SubRow, Option<VerifyRow>)>,
}

fn entry_of(hub: &Hub, row: &SubRow) -> Entry {
    Entry {
        submission: row.sha256.clone(),
        tier: row.tier.clamp(0, 3) as u8,
        member: row.member.clone(),
        block: crate::door::block_of(hub, &row.member)
            .unwrap_or_else(|| format!("member:{}", row.member)),
        queued_at: row.updated_at * 1000,
    }
}

/// Called on every hub tick. Returns messages for the hub to append.
pub fn on_tick(hub: &Hub, now: u64) -> Vec<Message> {
    let cfg = &hub.config().verify;
    if !cfg.enabled || cfg.repository.is_empty() {
        return Vec::new();
    }
    let Some(snap) = hub.with_store(|s| {
        let conn = s.connection();
        Snapshot {
            gated: query_by_states(conn, &["gated"]),
            verifying: query_by_states(conn, &["verifying"])
                .into_iter()
                .map(|r| {
                    let v = query_verify(conn, &r.sha256);
                    (r, v)
                })
                .collect(),
        }
    }) else {
        return Vec::new();
    };
    let Some(api) = cfg.api() else {
        return Vec::new();
    };
    let mut plan = Plan::default();
    {
        let mut slots = lock(&cfg.runtime.slots);
        // Machines of submissions that were `verifying` before a restart.
        for (row, v) in &snap.verifying {
            if slots.contains_key(&row.sha256) {
                continue;
            }
            let st = rebuild(row, v.as_ref(), cfg, now);
            let mut slot = Slot {
                st,
                row: row.clone(),
                entry: entry_of(hub, row),
            };
            match slot.st.phase {
                Phase::Done => {
                    // The fact of the end is in the log, the submission has not caught up.
                    let word = slot.st.conclusion.clone().unwrap_or_default();
                    let sec = now / 1000;
                    if word == "passed" {
                        plan.messages
                            .push(submission_fact(&slot.row, "verified", sec));
                    } else {
                        plan.messages
                            .push(submission_fact(&slot.row, "verify_failed", sec));
                        plan.failures
                            .push((slot.row.clone(), word, slot.st.failing_step.clone()));
                    }
                    continue;
                }
                Phase::Queued => {
                    // `verifying` with no fact of the build: start it again.
                    // The submission is `verifying` already, so nothing is announced.
                    drive(&mut slot, now, Event::Start, &mut plan, false);
                }
                _ => {}
            }
            slots.insert(row.sha256.clone(), slot);
        }
        // Answers of GitHub calls.
        let events: Vec<(String, Event)> = std::mem::take(&mut *lock(&cfg.runtime.inbox));
        for (sha, ev) in events {
            if let Some(slot) = slots.get_mut(&sha) {
                drive(slot, now, ev, &mut plan, true);
            }
        }
        // New runs by the queue rules.
        let running: Vec<Entry> = slots
            .values()
            .filter(|s| matches!(s.st.phase, Phase::Dispatched | Phase::Running))
            .map(|s| s.entry.clone())
            .collect();
        let pending: Vec<Entry> = snap
            .gated
            .iter()
            .filter(|r| !slots.contains_key(&r.sha256))
            .map(|r| entry_of(hub, r))
            .collect();
        for sha in next_to_verify(&pending, &running, &cfg.rules) {
            let Some(row) = snap.gated.iter().find(|r| r.sha256 == sha) else {
                continue;
            };
            let mut slot = Slot {
                st: VerifyState::new(&sha, &row.task, row.updated_at * 1000, cfg.timers),
                row: row.clone(),
                entry: entry_of(hub, row),
            };
            drive(&mut slot, now, Event::Start, &mut plan, true);
            slots.insert(sha, slot);
        }
        // Time passes for every running machine.
        for slot in slots.values_mut() {
            if matches!(slot.st.phase, Phase::Dispatched | Phase::Running) {
                drive(slot, now, Event::Tick, &mut plan, true);
            }
        }
        slots.retain(|_, s| s.st.phase != Phase::Done);
    }
    let Plan {
        mut messages,
        jobs,
        failures,
    } = plan;
    launch(cfg, api, jobs);
    for (row, word, step) in failures {
        let note = failure_note(&word, step.as_deref());
        if let Some(Some(m)) =
            hub.with_store(|s| requeue_message(s.connection(), &row, "verify_failed", &note, now))
        {
            messages.push(m);
        }
    }
    messages
}

/// Handles the requests `verify.*`.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    match op {
        "verify.retry" => retry(ctx, body),
        "verify.override" => override_run(ctx, body),
        other => Err(format!(
            "unknown op `{other}` (verify.retry, verify.override)"
        )),
    }
}

/// Longest reason of an override, in characters.
const MAX_REASON: usize = 200;

/// `verify.override {submission, conclusion, reason}`: the orchestrator marks a `gated` submission verified
/// (`passed`) or failed (`failed`) without a run. It is the way to go on while GitHub is down. The FACT
/// `verify/<sha256>` names who did it and why, so the log shows that no run happened.
fn override_run(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    if !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub) {
        return Err("only an orchestrator may run `verify.override`".into());
    }
    let sha = body
        .get("submission")
        .or_else(|| body.get("sha256"))
        .and_then(Value::as_str)
        .filter(|s| crate::intake::facts::is_sha256(s))
        .ok_or("verify.override needs `submission`, 64 lower case hex characters")?
        .to_owned();
    let conclusion = match body.get("conclusion").and_then(Value::as_str) {
        Some(c @ ("passed" | "failed")) => c.to_owned(),
        _ => return Err("verify.override needs `conclusion`: `passed` or `failed`".into()),
    };
    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or("verify.override needs a `reason`")?;
    let reason = one_line(reason, MAX_REASON);
    let row = query_submission(ctx.store.connection(), &sha)
        .ok_or_else(|| format!("no submission {}", one_line(&sha, 64)))?;
    if row.state != "gated" {
        return Err(format!(
            "only a submission in the state `gated` can be overridden, this one is `{}`",
            row.state
        ));
    }
    let sec = ctx.now_ms / 1000;
    let done = json!({
        "v": 1, "submission": sha, "order": row.task, "run_id": null, "state": "done",
        "conclusion": conclusion, "failing_step": null, "minutes": 0, "dispatches": 0,
        "started_ms": 0, "at": sec, "overridden": true, "by": ctx.caller.name, "reason": reason,
    });
    ctx.emit(fact(format!("verify/{sha}"), &done))?;
    let verified = conclusion == "passed";
    let state = if verified {
        "verified"
    } else {
        "verify_failed"
    };
    let id = ctx.emit(submission_fact(&row, state, sec))?;
    if !verified {
        let note = format!("the clean build was marked failed by the owner: {reason}");
        let conn = ctx.store.connection();
        if let Some(msg) = requeue_message(conn, &row, "verify_failed", &note, ctx.now_ms) {
            ctx.emit(msg)?;
        }
    }
    let mut out = Map::new();
    out.insert("submission".into(), sha.into());
    out.insert("state".into(), state.into());
    out.insert("conclusion".into(), conclusion.into());
    out.insert("id".into(), id.into());
    Ok(out)
}

/// `verify.retry <sha256>`: a submission whose clean build failed goes back to `gated`.
fn retry(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    if !matches!(ctx.caller.role, Role::Orchestrator | Role::Hub) {
        return Err("only an orchestrator may run `verify.retry`".into());
    }
    let sha = body
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|s| crate::intake::facts::is_sha256(s))
        .ok_or("verify.retry needs `sha256`, 64 lower case hex characters")?
        .to_owned();
    let row = query_submission(ctx.store.connection(), &sha)
        .ok_or_else(|| format!("no submission {}", one_line(&sha, 64)))?;
    if row.state != "verify_failed" {
        return Err(format!(
            "only a submission in the state `verify_failed` can be retried, this one is `{}`",
            row.state
        ));
    }
    let sec = ctx.now_ms / 1000;
    let reset = json!({
        "v": 1, "submission": sha, "order": row.task, "run_id": null, "state": "queued",
        "conclusion": null, "failing_step": null, "minutes": 0, "dispatches": 0,
        "started_ms": 0, "at": sec,
    });
    ctx.emit(fact(format!("verify/{sha}"), &reset))?;
    let id = ctx.emit(submission_fact(&row, "gated", sec))?;
    let mut out = Map::new();
    out.insert("submission".into(), sha.into());
    out.insert("state".into(), "gated".into());
    out.insert("id".into(), id.into());
    Ok(out)
}
