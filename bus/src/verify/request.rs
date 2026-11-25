//! Pure builders for the GitHub REST requests the verifier needs.
//!
//! A request is plain data. It never holds the token: the caller adds `Authorization` when it sends the
//! request (see [`crate::verify::api`]). So a spec can be logged, stored and compared in tests.

use serde_json::{Value, json};

/// The API version header value. Long stable, see the README of the workflow templates.
pub const API_VERSION: &str = "2022-11-28";
/// The default API root. The factory uses github.com.
pub const API_ROOT: &str = "https://api.github.com";
/// The client name sent as `User-Agent`, which GitHub requires.
pub const USER_AGENT: &str = "sommelier-verify";

/// One HTTP request as data: method, URL, headers without any token value, and an optional JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequestSpec {
    /// `GET` or `POST`.
    pub method: String,
    /// The full URL including the query.
    pub url: String,
    /// Header name and value pairs. Never `Authorization`.
    pub headers: Vec<(String, String)>,
    /// The JSON body of a `POST`.
    pub body: Option<Value>,
}

/// The inputs of the workflow `factory-verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchInputs {
    /// The SHA 256 of the bundle, 64 lower case hex characters.
    pub submission: String,
    /// The order id, for example `t412`.
    pub order: String,
    /// The base commit the bundle applies on.
    pub base: String,
    /// The base URL of the hub that serves the bundle and the order.
    pub hub: String,
    /// The attempt of the order that this submission belongs to.
    pub attempt: u32,
    /// Which dispatch of the current round this is: 1, or 2 for the one retry of a lost dispatch.
    pub dispatch: u32,
    /// The start of the round in milliseconds. A `verify.retry` begins a new round, so a run of an
    /// earlier round never carries the name of the run that the hub waits for now.
    pub round: u64,
}

impl DispatchInputs {
    /// The run name the hub expects, which the template sets with
    /// `run-name: verify <submission> a<attempt> d<dispatch> t<round>`.
    pub fn run_name(&self) -> String {
        run_name(&self.submission, self.attempt, self.dispatch, self.round)
    }
}

/// The run name for one dispatch. See [`DispatchInputs::run_name`].
pub fn run_name(submission: &str, attempt: u32, dispatch: u32, round: u64) -> String {
    format!("verify {submission} a{attempt} d{dispatch} t{round}")
}

/// Percent encodes one URL path segment. Only unreserved characters pass.
fn segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Encodes `owner/name` as two segments joined by a slash.
fn repo_path(repo: &str) -> String {
    repo.split('/').map(segment).collect::<Vec<_>>().join("/")
}

fn base_headers(post: bool) -> Vec<(String, String)> {
    let mut h = vec![
        (
            "Accept".to_owned(),
            "application/vnd.github+json".to_owned(),
        ),
        ("X-GitHub-Api-Version".to_owned(), API_VERSION.to_owned()),
        ("User-Agent".to_owned(), USER_AGENT.to_owned()),
    ];
    if post {
        h.push(("Content-Type".to_owned(), "application/json".to_owned()));
    }
    h
}

fn get(url: String) -> HttpRequestSpec {
    HttpRequestSpec {
        method: "GET".to_owned(),
        url,
        headers: base_headers(false),
        body: None,
    }
}

/// The request that starts `workflow_file` on `git_ref` of `repo` (`owner/name`) with the four inputs.
///
/// GitHub answers `204 No Content` and no run id, so the run is found later with [`list_runs_request`]
/// and [`crate::verify::find_run`]. Pure.
pub fn dispatch_request(
    repo: &str,
    workflow_file: &str,
    git_ref: &str,
    inputs: &DispatchInputs,
) -> HttpRequestSpec {
    HttpRequestSpec {
        method: "POST".to_owned(),
        url: format!(
            "{API_ROOT}/repos/{}/actions/workflows/{}/dispatches",
            repo_path(repo),
            segment(workflow_file)
        ),
        headers: base_headers(true),
        body: Some(json!({
            "ref": git_ref,
            "inputs": {
                "submission": inputs.submission,
                "order": inputs.order,
                "base": inputs.base,
                "hub": inputs.hub,
                "attempt": inputs.attempt.to_string(),
                "dispatch": inputs.dispatch.to_string(),
                "round": inputs.round.to_string(),
            },
        })),
    }
}

/// The request for the newest runs of a workflow that came from a `workflow_dispatch`.
pub fn list_runs_request(repo: &str, workflow_file: &str) -> HttpRequestSpec {
    get(format!(
        "{API_ROOT}/repos/{}/actions/workflows/{}/runs?event=workflow_dispatch&per_page=100",
        repo_path(repo),
        segment(workflow_file)
    ))
}

/// The request for one run by id.
pub fn run_request(repo: &str, run_id: u64) -> HttpRequestSpec {
    get(format!(
        "{API_ROOT}/repos/{}/actions/runs/{run_id}",
        repo_path(repo)
    ))
}

/// The request for the jobs of the latest attempt of a run.
pub fn jobs_request(repo: &str, run_id: u64) -> HttpRequestSpec {
    get(format!(
        "{API_ROOT}/repos/{}/actions/runs/{run_id}/jobs?filter=latest&per_page=100",
        repo_path(repo)
    ))
}
