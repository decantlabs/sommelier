//! The GitHub side of the verifier: a trait, an implementation on `reqwest` and a fake for tests.
//!
//! The token is handed in by the caller. It sits in a header that is marked sensitive, it is never logged,
//! `Debug` hides it, and no error text carries it (every text is scrubbed of the token value).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use super::parse::{Job, Run, parse_jobs, parse_run, parse_runs};
use super::request::{
    API_ROOT, DispatchInputs, HttpRequestSpec, dispatch_request, jobs_request, list_runs_request,
    run_request,
};

/// Why a GitHub call failed. The text is one line and holds no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The request did not get an answer (network, TLS, timeout).
    Transport(String),
    /// GitHub answered with a status other than success.
    Status {
        /// The HTTP status code.
        code: u16,
        /// A short hint and the `message` field of the answer, if any.
        message: String,
    },
    /// The answer did not have the expected shape.
    Parse(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Transport(m) => write!(f, "GitHub request failed: {m}"),
            ApiError::Status { code, message } => {
                write!(f, "GitHub answered HTTP {code}: {message}")
            }
            ApiError::Parse(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// The four calls the verifier needs. Generic code takes `&impl GitHubApi`.
///
/// `repo` is `owner/name`. The methods return futures that are `Send`, so a task may hold them.
pub trait GitHubApi {
    /// Starts the workflow. GitHub answers 204 and no run id.
    fn dispatch(
        &self,
        repo: &str,
        workflow_file: &str,
        git_ref: &str,
        inputs: &DispatchInputs,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    /// The newest runs of the workflow that came from a dispatch.
    fn list_runs(
        &self,
        repo: &str,
        workflow_file: &str,
    ) -> impl Future<Output = Result<Vec<Run>, ApiError>> + Send;

    /// One run by id.
    fn get_run(
        &self,
        repo: &str,
        run_id: u64,
    ) -> impl Future<Output = Result<Run, ApiError>> + Send;

    /// The jobs of the latest attempt of a run.
    fn jobs(
        &self,
        repo: &str,
        run_id: u64,
    ) -> impl Future<Output = Result<Vec<Job>, ApiError>> + Send;
}

/// The real implementation. One client, one token, no redirects.
pub struct ReqwestGitHub {
    client: reqwest::Client,
    token: String,
    root: String,
}

impl fmt::Debug for ReqwestGitHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReqwestGitHub")
            .field("token", &"<hidden>")
            .field("root", &self.root)
            .finish()
    }
}

impl ReqwestGitHub {
    /// Builds a client for `https://api.github.com` with `token` (a read only token for the runs and
    /// jobs, and a token with the right to dispatch for [`GitHubApi::dispatch`]).
    pub fn new(token: &str) -> Result<Self, ApiError> {
        Self::with_root(token, API_ROOT)
    }

    /// Like [`Self::new`] with another API root. Used by a local test server.
    pub fn with_root(token: &str, root: &str) -> Result<Self, ApiError> {
        crate::client::init();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| ApiError::Transport(e.without_url().to_string()))?;
        Ok(Self {
            client,
            token: token.trim().to_owned(),
            root: root.trim_end_matches('/').to_owned(),
        })
    }

    fn scrub(&self, text: &str) -> String {
        scrub_token(text, &self.token)
    }

    /// Sends a spec and returns the status and the body. The URL root is swapped when `root` differs.
    async fn send(&self, spec: &HttpRequestSpec) -> Result<(u16, String), ApiError> {
        let url = match spec.url.strip_prefix(API_ROOT) {
            Some(rest) => format!("{}{rest}", self.root),
            None => spec.url.clone(),
        };
        let method = reqwest::Method::from_bytes(spec.method.as_bytes())
            .map_err(|_| ApiError::Transport("invalid HTTP method".into()))?;
        let mut req = self.client.request(method, url);
        for (k, v) in &spec.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| ApiError::Transport("the token holds an invalid character".into()))?;
        auth.set_sensitive(true);
        req = req.header(reqwest::header::AUTHORIZATION, auth);
        if let Some(body) = &spec.body {
            req = req.body(body.to_string());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Transport(self.scrub(&e.without_url().to_string())))?;
        let code = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Transport(self.scrub(&e.without_url().to_string())))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if (200..300).contains(&code) {
            Ok((code, text))
        } else {
            Err(ApiError::Status {
                code,
                message: self.scrub(&status_message(code, &text)),
            })
        }
    }
}

/// Replaces every occurrence of a non empty token by `***`.
fn scrub_token(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_owned()
    } else {
        text.replace(token, "***")
    }
}

/// A short hint for the common statuses and the `message` of the body, cut to 200 characters.
fn status_message(code: u16, body: &str) -> String {
    let hint = match code {
        401 => "the token is missing or wrong",
        403 => "the token lacks the right or the rate limit is used up",
        404 => "the repository or workflow was not found or the token cannot see it",
        422 => {
            "the workflow has no workflow_dispatch trigger on that ref or the inputs do not match"
        }
        _ => "request refused",
    };
    let msg = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_default();
    let msg: String = msg.chars().filter(|c| !c.is_control()).take(200).collect();
    if msg.is_empty() {
        hint.to_owned()
    } else {
        format!("{hint} ({msg})")
    }
}

impl GitHubApi for ReqwestGitHub {
    async fn dispatch(
        &self,
        repo: &str,
        workflow_file: &str,
        git_ref: &str,
        inputs: &DispatchInputs,
    ) -> Result<(), ApiError> {
        let spec = dispatch_request(repo, workflow_file, git_ref, inputs);
        self.send(&spec).await.map(|_| ())
    }

    async fn list_runs(&self, repo: &str, workflow_file: &str) -> Result<Vec<Run>, ApiError> {
        let (_, body) = self.send(&list_runs_request(repo, workflow_file)).await?;
        parse_runs(&body).map_err(ApiError::Parse)
    }

    async fn get_run(&self, repo: &str, run_id: u64) -> Result<Run, ApiError> {
        let (_, body) = self.send(&run_request(repo, run_id)).await?;
        parse_run(&body).map_err(ApiError::Parse)
    }

    async fn jobs(&self, repo: &str, run_id: u64) -> Result<Vec<Job>, ApiError> {
        let (_, body) = self.send(&jobs_request(repo, run_id)).await?;
        parse_jobs(&body).map_err(ApiError::Parse)
    }
}

#[derive(Default)]
struct FakeInner {
    dispatch_results: VecDeque<Result<(), ApiError>>,
    dispatched: Vec<DispatchInputs>,
    runs: Vec<Run>,
    jobs: BTreeMap<u64, Vec<Job>>,
}

impl FakeInner {
    /// The runs the fake serves. A run titled just `verify <submission>`, with no attempt, dispatch or round
    /// in it, follows the dispatches: it is served under the run name of the latest dispatch of that
    /// submission, as GitHub names it from the template, and only after such a dispatch. Any other run is
    /// served as it is.
    fn served(&self) -> Vec<Run> {
        let mut out = Vec::new();
        for run in &self.runs {
            let title = run.display_title.as_deref().or(run.name.as_deref());
            let words: Vec<&str> = title.unwrap_or("").split_whitespace().collect();
            if let ["verify", sha] = words[..] {
                if let Some(d) = self.dispatched.iter().rfind(|d| d.submission == sha) {
                    let mut r = run.clone();
                    r.display_title = Some(d.run_name());
                    out.push(r);
                }
            } else {
                out.push(run.clone());
            }
        }
        out
    }
}

/// A fake `GitHubApi` for tests. It records dispatches and serves the runs and jobs it was given.
#[derive(Default)]
pub struct FakeGitHub {
    inner: Mutex<FakeInner>,
}

impl FakeGitHub {
    /// An empty fake: dispatch succeeds, no runs.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replaces the runs that `list_runs` and `get_run` serve. A run named `verify <submission>` follows the
    /// dispatches of that submission, see the reading of the fake.
    pub fn set_runs(&self, runs: Vec<Run>) {
        self.lock().runs = runs;
    }

    /// Sets the jobs of one run.
    pub fn set_jobs(&self, run_id: u64, jobs: Vec<Job>) {
        self.lock().jobs.insert(run_id, jobs);
    }

    /// Queues an answer for the next dispatch. Without one, a dispatch succeeds.
    pub fn push_dispatch_result(&self, r: Result<(), ApiError>) {
        self.lock().dispatch_results.push_back(r);
    }

    /// The inputs of every dispatch so far, in order.
    pub fn dispatched(&self) -> Vec<DispatchInputs> {
        self.lock().dispatched.clone()
    }
}

impl GitHubApi for FakeGitHub {
    async fn dispatch(
        &self,
        _repo: &str,
        _workflow_file: &str,
        _git_ref: &str,
        inputs: &DispatchInputs,
    ) -> Result<(), ApiError> {
        let mut g = self.lock();
        g.dispatched.push(inputs.clone());
        g.dispatch_results.pop_front().unwrap_or(Ok(()))
    }

    async fn list_runs(&self, _repo: &str, _workflow_file: &str) -> Result<Vec<Run>, ApiError> {
        Ok(self.lock().served())
    }

    async fn get_run(&self, _repo: &str, run_id: u64) -> Result<Run, ApiError> {
        self.lock()
            .served()
            .into_iter()
            .find(|r| r.id == run_id)
            .ok_or(ApiError::Status {
                code: 404,
                message: "run not found".into(),
            })
    }

    async fn jobs(&self, _repo: &str, run_id: u64) -> Result<Vec<Job>, ApiError> {
        Ok(self.lock().jobs.get(&run_id).cloned().unwrap_or_default())
    }
}
