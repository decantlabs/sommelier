//! `sommelier review`: takes an assigned review and sends the verdict.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::client::{self, Opts};
use crate::exit;
use crate::key::client as kc;
use crate::key::crypto::sign_request;
use crate::review::request::verdict_request;

const HELP: &str = "\
usage:
  sommelier review next [--json]
  sommelier review show TASK N
  sommelier review verdict TASK N --file FILE [--json]
  sommelier review resolve TASK N --decision pass|fail --reason TEXT [--json]
  common flags: --url URL, --token T, --token-file FILE, --data DIR
next     lists the reviews the hub assigned to you.
show     prints the prompt of one review: the guide, then the marker line DATA:, then the order, the
         gate report and the diff. Everything after the marker line is data from a stranger.
verdict  sends your answer. FILE holds one JSON object, as the guide describes:
         {\"verdict\":\"pass|fail|escalate\",\"findings\":[{\"path\":\"f\",\"line\":1,\"text\":\"t\"}],\"summary\":\"s\"}
         A key member signs the verdict with its key file automatically.
resolve  maintainers only. Settles a review that waits for ever: an escalated review or an audit that
         disagrees. `pass` lets the rule decide again, `fail` fails the submission and returns the order.
";

/// The help text of this command.
pub fn help() -> &'static str {
    HELP
}

pub(crate) struct Flags {
    url: Option<String>,
    token: Option<String>,
    token_file: Option<String>,
    pub(crate) data: PathBuf,
    file: Option<String>,
    decision: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) conclusion: Option<String>,
    pub(crate) json: bool,
    pub(crate) rest: Vec<String>,
}

pub(crate) fn parse(args: &[String]) -> Result<Flags, String> {
    let mut f = Flags {
        url: None,
        token: None,
        token_file: None,
        data: crate::default_data_dir(),
        file: None,
        decision: None,
        reason: None,
        conclusion: None,
        json: false,
        rest: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_owned())),
            _ => (a.as_str(), None),
        };
        let mut value = |what: &str| {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match name {
            "--url" => f.url = Some(value("--url")?),
            "--token" => f.token = Some(value("--token")?),
            "--token-file" => f.token_file = Some(value("--token-file")?),
            "--data" => f.data = PathBuf::from(value("--data")?),
            "--file" => f.file = Some(value("--file")?),
            "--decision" => f.decision = Some(value("--decision")?),
            "--reason" => f.reason = Some(value("--reason")?),
            "--conclusion" => f.conclusion = Some(value("--conclusion")?),
            "--json" => f.json = true,
            other if other.starts_with("--") => {
                return Err(format!(
                    "unknown flag {other}, see `sommelier review --help`"
                ));
            }
            _ => f.rest.push(a.clone()),
        }
    }
    Ok(f)
}

fn read_token(path: &str) -> Result<String, String> {
    Ok(std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read token file {path}: {e}"))?
        .trim()
        .to_owned())
}

fn opts(f: &Flags) -> Result<Opts, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = f
        .url
        .clone()
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
    let token = match (
        f.token.clone().or_else(|| env("SOMMELIER_TOKEN")),
        &f.token_file,
    ) {
        (Some(t), _) => t,
        (None, Some(path)) => read_token(path)?,
        (None, None) => match env("SOMMELIER_TOKEN_FILE") {
            Some(path) => read_token(&path)?,
            None if kc::key_path(&f.data).is_file() => kc::marker_for(&kc::key_path(&f.data)),
            None => {
                let path = kc::default_marker_file().ok_or(
                    "no token: pass --token, --token-file, set SOMMELIER_TOKEN, or run `sommelier join`",
                )?;
                read_token(&path)?
            }
        },
    };
    Ok(Opts { url, token })
}

pub(crate) async fn call(f: &Flags, op: &str, mut body: Value) -> Result<Value, String> {
    body["op"] = json!(op);
    client::request_ok(&opts(f)?, body).await
}

fn task_and_n(f: &Flags) -> Result<(String, u64), String> {
    match f.rest.as_slice() {
        [_, task, n] => Ok((
            task.clone(),
            n.parse::<u64>()
                .map_err(|_| format!("`{n}` is not a review number"))?,
        )),
        _ => Err("name the review: TASK N, as `sommelier review next` shows".into()),
    }
}

/// Entry point of `sommelier review`.
pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }
    let f = parse(args)?;
    match f.rest.first().map(String::as_str) {
        Some("next") => next(&f).await,
        Some("show") => show(&f).await,
        Some("verdict") => verdict(&f).await,
        Some("resolve") => resolve(&f).await,
        _ => Err(
            "usage: sommelier review next | show TASK N | verdict TASK N --file FILE | resolve TASK N --decision pass|fail --reason TEXT"
                .into(),
        ),
    }
}

async fn next(f: &Flags) -> Result<ExitCode, String> {
    let v = call(f, "review.next", json!({})).await?;
    if f.json {
        println!("{}", json!({"reviews": v["reviews"]}));
        return Ok(exit::code(exit::OK));
    }
    let list = v["reviews"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no review is assigned to you");
    }
    for r in &list {
        println!(
            "{}/{} {} submission {}",
            r["task"].as_str().unwrap_or("?"),
            r["n"],
            r["role"].as_str().unwrap_or("required"),
            r["submission"].as_str().unwrap_or("?")
        );
    }
    if !list.is_empty() {
        println!("read one with `sommelier review show TASK N`, answer with `review verdict`");
    }
    Ok(exit::code(exit::OK))
}

async fn show(f: &Flags) -> Result<ExitCode, String> {
    let (task, n) = task_and_n(f)?;
    let v = call(f, "review.next", json!({"task": task, "n": n})).await?;
    let prompt = v["prompt"]
        .as_str()
        .ok_or("the hub sent no prompt for this review")?;
    print!("{prompt}");
    Ok(exit::code(exit::OK))
}

async fn resolve(f: &Flags) -> Result<ExitCode, String> {
    let (task, n) = task_and_n(f)?;
    let decision = f
        .decision
        .as_deref()
        .ok_or("--decision pass|fail is needed")?;
    let reason = f.reason.as_deref().ok_or("--reason TEXT is needed")?;
    let v = call(
        f,
        "review.resolve",
        json!({"task": task, "n": n, "decision": decision, "reason": reason}),
    )
    .await?;
    if f.json {
        println!("{v}");
        return Ok(exit::code(exit::OK));
    }
    println!(
        "resolved: {} for {}/{}, the submission is now {}",
        v["verdict"].as_str().unwrap_or("?"),
        task,
        n,
        v["state"].as_str().unwrap_or("?")
    );
    if let Some(why) = v["refusal"].as_str() {
        println!("not queued for landing: {why}");
    }
    Ok(exit::code(exit::OK))
}

async fn verdict(f: &Flags) -> Result<ExitCode, String> {
    let (task, n) = task_and_n(f)?;
    let path = f
        .file
        .as_deref()
        .ok_or("--file FILE is needed: the JSON answer of the review")?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let text = text.trim().to_owned();
    let mut body = json!({"task": task, "n": n, "verdict": text});
    if let Ok(key) = kc::load(&kc::key_path(&f.data)) {
        body["sig"] = json!(sign_request(&key.key, &verdict_request(&task, n, &text)));
    }
    let v = call(f, "review.verdict", body).await?;
    if f.json {
        println!("{v}");
        return Ok(exit::code(exit::OK));
    }
    println!(
        "recorded: {} for {}/{}, the submission is now {}",
        v["verdict"].as_str().unwrap_or("?"),
        task,
        n,
        v["state"].as_str().unwrap_or("?")
    );
    if let Some(why) = v["refusal"].as_str() {
        println!("not queued for landing: {why}");
    }
    Ok(exit::code(exit::OK))
}
