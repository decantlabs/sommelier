//! `sommelier submit`: sends the work of a lease to the hub as a signed bundle.
//!
//! The command packs the commits from the base of the lease to the head with `bundle::pack`, signs
//! the request when you work with a key, uploads the file with `PUT /api/submit/<task>/<attempt>` and
//! prints the hash and the state. See PROTOCOL.md, section Submissions.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use crate::bundle::pack;
use crate::client::{self, Opts};
use crate::exit;
use crate::key::client as kc;
use crate::key::crypto::sign_request;

const HELP: &str = "\
usage: sommelier submit TASK [--attempt N] [--repo DIR] [--base B] [--head H] [--json]
                             [--url URL] [--token T | --token-file FILE]
Packs the commits from the base of your lease to the head into one git bundle, signs the request
when you joined with a key, uploads it and prints the hash and the state of the submission.
  TASK        the order you hold the lease of
  --attempt N the attempt of the lease (default: the one the hub shows for the order)
  --repo DIR  the repository with your work (default: the current folder)
  --base B    the base commit of the lease (default: the base the hub shows for the order)
  --head H    the last commit to send (default: HEAD)
  --url URL   hub address, default ws://127.0.0.1:7300 (env SOMMELIER_URL)
A member that joined with a key may not read the order: pass --attempt and --base then.
The bundle may hold no more than your tier allows. The hub runs the static gates next, and the
request `submit.status` shows the state of each of your submissions. Each order and attempt takes one
live submission: send the next one only after the first failed and the order came back. A tier also
caps the submissions of 24 hours.
";

/// The help text of this command.
pub fn help() -> &'static str {
    HELP
}

struct Args {
    task: String,
    attempt: Option<u32>,
    repo: PathBuf,
    base: Option<String>,
    head: String,
    json: bool,
    url: Option<String>,
    token: Option<String>,
    token_file: Option<String>,
}

fn parse(raw: &[String]) -> Result<Args, String> {
    let mut a = Args {
        task: String::new(),
        attempt: None,
        repo: PathBuf::from("."),
        base: None,
        head: "HEAD".into(),
        json: false,
        url: None,
        token: None,
        token_file: None,
    };
    let mut it = raw.iter();
    while let Some(s) = it.next() {
        let (name, inline) = match s.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_owned())),
            _ => (s.as_str(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => it.next().cloned().ok_or(format!("{what} needs a value")),
            }
        };
        match name {
            "--attempt" => {
                let v = value("--attempt")?;
                a.attempt = Some(v.parse().map_err(|_| "--attempt must be a number")?);
            }
            "--repo" => a.repo = PathBuf::from(value("--repo")?),
            "--base" => a.base = Some(value("--base")?),
            "--head" => a.head = value("--head")?,
            "--url" => a.url = Some(value("--url")?),
            "--token" => a.token = Some(value("--token")?),
            "--token-file" => a.token_file = Some(value("--token-file")?),
            "--json" => a.json = true,
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            _ if a.task.is_empty() => a.task = s.clone(),
            _ => return Err("only one TASK may be named".into()),
        }
    }
    if a.task.is_empty() {
        return Err("name the order: sommelier submit TASK".into());
    }
    Ok(a)
}

fn client_opts(a: &Args) -> Result<Opts, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = a
        .url
        .clone()
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
    let token = match a.token.clone().or_else(|| env("SOMMELIER_TOKEN")) {
        Some(t) => t,
        None => {
            let path = a
                .token_file
                .clone()
                .or_else(|| env("SOMMELIER_TOKEN_FILE"))
                .or_else(kc::default_marker_file)
                .ok_or("no token: pass --token, --token-file, set SOMMELIER_TOKEN, or run `sommelier join`")?;
            fs::read_to_string(&path)
                .map_err(|_| "cannot read the token file".to_owned())?
                .trim()
                .to_owned()
        }
    };
    Ok(Opts { url, token })
}

/// The attempt and the base of the lease, as the hub shows them for the order.
async fn lease_facts(opts: &Opts, task: &str) -> Result<(u32, String), String> {
    let v = client::request_ok(opts, json!({"op": "task.show", "id": task})).await?;
    let attempt = v["lease"]["attempt"]
        .as_u64()
        .or_else(|| v["task"]["attempt"].as_u64())
        .ok_or("the hub shows no attempt for this order")?;
    let base = v["task"]["base_sha"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    Ok((attempt as u32, base))
}

/// Uploads a bundle file for a lease. A member that joined with a key signs the request. Returns the
/// JSON answer of the hub: `submission`, `state` and `known`.
pub async fn upload(opts: &Opts, task: &str, attempt: u32, bundle: &Path) -> Result<Value, String> {
    let body = fs::read(bundle).map_err(|e| format!("cannot read the bundle: {e}"))?;
    let sha = crate::brief::sha256_hex(&body);
    let raw = opts
        .urls()
        .into_iter()
        .next()
        .ok_or("no hub URL configured")?;
    let mut url = kc::http_url(&raw, &format!("/api/submit/{task}/{attempt}"))?;
    let mut query = format!("sha256={sha}");
    let auth = match kc::marker_path(&opts.token) {
        Some(path) => {
            let file = kc::load(&path)?;
            let request = json!({"op": "submit", "task": task, "attempt": attempt, "sha256": sha});
            query.push_str(&format!("&sig={}", sign_request(&file.key, &request)));
            kc::auth_header(&raw, &opts.token)
                .await
                .map_err(|e| e.msg)?
        }
        None => format!("Bearer {}", opts.token.trim()),
    };
    url.set_query(Some(&query));
    client::init();
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?
        .put(url)
        .header("authorization", auth)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("cannot reach the hub: {}", e.without_url()))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("the hub sent no answer: {}", e.without_url()))?;
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("the hub sent no usable answer: HTTP {status}"))?;
    if !status.is_success() || v["ok"].as_bool() != Some(true) {
        return Err(v["error"]
            .as_str()
            .unwrap_or("the hub refused the submission")
            .to_owned());
    }
    Ok(v)
}

/// Entry point of `sommelier submit`.
pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return Ok(exit::code(exit::OK));
    }
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}\n{HELP}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let opts = client_opts(&a)?;
    let (attempt, base) = match (a.attempt, a.base.clone()) {
        (Some(n), Some(b)) => (n, b),
        (n, b) => {
            let (hub_attempt, hub_base) = lease_facts(&opts, &a.task).await.map_err(|e| {
                format!("cannot read the lease from the hub ({e}): pass --attempt and --base")
            })?;
            (n.unwrap_or(hub_attempt), b.unwrap_or(hub_base))
        }
    };
    if base.is_empty() {
        return Err("the order has no base commit: pass --base".into());
    }
    let dir = tempfile::tempdir().map_err(|e| format!("cannot make a temporary folder: {e}"))?;
    let file = dir.path().join("submission.bundle");
    pack(&a.repo, &base, &a.head, &file)?;
    let v = upload(&opts, &a.task, attempt, &file).await?;
    if a.json {
        println!("{v}");
    } else {
        println!(
            "submitted {} for {} attempt {attempt}: state {}{}",
            v["submission"].as_str().unwrap_or("?"),
            a.task,
            v["state"].as_str().unwrap_or("?"),
            if v["known"].as_bool() == Some(true) {
                " (the hub had it already)"
            } else {
                ""
            }
        );
    }
    Ok(exit::code(exit::OK))
}
