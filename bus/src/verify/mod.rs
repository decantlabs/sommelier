//! The clean build on a runner without secrets: starting the verify workflow and reading its conclusion.
//!
//! Part of the public factory, see `docs/factory.md` sections 8.3, 8.5, 9.2 and 10. The hub starts the
//! workflow `factory-verify` with a `workflow_dispatch` and later reads only the conclusion of that run
//! through the GitHub REST API with a read only token. The runner reports nothing itself.
//!
//! The module is library code in this wave and is not wired into the hub yet. Its parts:
//!
//! * [`request`]: pure builders for the HTTP requests (no token value inside).
//! * [`parse`]: parsers for the runs, the run and the jobs, [`find_run`] and [`conclusion`].
//! * [`api`]: the trait [`GitHubApi`], a `reqwest` implementation and a fake for tests.
//! * [`machine`]: the pure state machine [`step`] of one submission and the FACT body.
//! * [`queue`]: [`next_to_verify`], which submissions start now.

pub mod api;
pub mod machine;
pub mod ops;
pub mod parse;
pub mod queue;
pub mod request;

pub use api::{ApiError, FakeGitHub, GitHubApi, ReqwestGitHub};
pub use machine::{Action, Event, FactBody, Phase, Timers, VerifyState, fact_topic, step};
pub use parse::{
    Conclusion, Job, JobStep, Run, conclusion, find_run, job_minutes, parse_jobs, parse_run,
    parse_runs,
};
pub use queue::{Entry, VerifyRules, next_to_verify};
pub use request::{
    DispatchInputs, HttpRequestSpec, dispatch_request, jobs_request, list_runs_request, run_request,
};
