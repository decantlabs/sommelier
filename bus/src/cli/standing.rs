//! `sommelier standing`: shows stamps and tiers, and lets a maintainer grant, clear, revert or mark intent.

use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use crate::client::{self, Opts};
use crate::exit;
use crate::key::client as kc;
use crate::{out, outln};

const HELP: &str = "\
usage: sommelier standing [MEMBER] [--json]
       sommelier standing grant MEMBER
       sommelier standing clear MEMBER STAMP_ID
       sommelier standing revert SUBMISSION --cause TEXT
       sommelier standing intent MEMBER --cause TEXT [--msg ID]
Shows the tier of a member (default: you), the stamps it holds by kind and what is missing for the
next tier. The subcommands are for maintainers and are refused for anyone else:
  grant    a tier 2 grant for a member, the one fact of tier 2 that only a maintainer can give
  clear    closes one penalty stamp (reverted or review_missed) by its id, as `standing` lists it
  revert   marks a landed submission as reverted for cause: the author and every reviewer that
           passed it lose a tier until a maintainer clears the stamp
  intent   a finding of bad intent: the member has no tier and its key is revoked
  --url URL        hub address, default ws://127.0.0.1:7300 (env SOMMELIER_URL)
  --token T        token (env SOMMELIER_TOKEN), --token-file F, else your key file from `sommelier join`
  --json           print the reply as one JSON line
";

/// The help text of this command.
pub fn help() -> &'static str {
    HELP
}

#[derive(Default)]
struct Args {
    url: Option<String>,
    token: Option<String>,
    token_file: Option<String>,
    cause: Option<String>,
    msg: Option<u64>,
    json: bool,
    rest: Vec<String>,
}

fn parse(raw: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = raw.iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = |what: &str| {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match name {
            "--url" => a.url = Some(value("--url")?),
            "--token" => a.token = Some(value("--token")?),
            "--token-file" => a.token_file = Some(value("--token-file")?),
            "--cause" => a.cause = Some(value("--cause")?),
            "--msg" => {
                let v = value("--msg")?;
                a.msg = Some(
                    v.parse()
                        .map_err(|_| format!("--msg needs a message id, got `{v}`"))?,
                );
            }
            "--json" => a.json = true,
            other if other.starts_with("--") => {
                return Err(format!(
                    "unknown flag {other}, see `sommelier standing --help`"
                ));
            }
            _ => a.rest.push(arg.clone()),
        }
    }
    Ok(a)
}

fn opts(a: &Args) -> Result<Opts, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = a
        .url
        .clone()
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
    let token = match (
        a.token.clone().or_else(|| env("SOMMELIER_TOKEN")),
        &a.token_file,
    ) {
        (Some(t), _) => t,
        (None, file) => {
            let path = file
                .clone()
                .or_else(|| env("SOMMELIER_TOKEN_FILE"))
                .or_else(kc::default_marker_file)
                .ok_or("no token: pass --token or --token-file, or run `sommelier join`")?;
            std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read token file {path}: {e}"))?
                .trim()
                .to_owned()
        }
    };
    Ok(Opts { url, token })
}

/// Entry point of `sommelier standing`.
pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        out!("{HELP}");
        return Ok(exit::code(exit::OK));
    }
    let a = parse(args)?;
    let rest: Vec<&str> = a.rest.iter().map(String::as_str).collect();
    match rest.as_slice() {
        ["grant", member] => maintainer(&a, "stamp.grant", json!({ "member": member })).await,
        ["clear", member, id] => {
            let id: u64 = id
                .parse()
                .map_err(|_| format!("STAMP_ID must be a number, got `{id}`"))?;
            maintainer(&a, "stamp.clear", json!({ "member": member, "stamp": id })).await
        }
        ["revert", sha] => {
            let cause = a.cause.clone().ok_or("revert needs --cause TEXT")?;
            maintainer(
                &a,
                "stamp.revert",
                json!({ "submission": sha, "cause": cause }),
            )
            .await
        }
        ["intent", member] => {
            let cause = a.cause.clone().ok_or("intent needs --cause TEXT")?;
            let mut body = json!({ "member": member, "cause": cause });
            if let Some(m) = a.msg {
                body["msg"] = json!(m);
            }
            maintainer(&a, "stamp.intent", body).await
        }
        [] => show(&a, None).await,
        [member] if !matches!(*member, "grant" | "clear" | "revert" | "intent") => {
            show(&a, Some(member)).await
        }
        _ => Err("usage: sommelier standing [MEMBER] | grant | clear | revert | intent, see `sommelier standing --help`".into()),
    }
}

async fn maintainer(a: &Args, op: &str, mut body: Value) -> Result<ExitCode, String> {
    let opts = opts(a)?;
    body["op"] = json!(op);
    body["rid"] = json!("cli-standing");
    let reply = client::request_ok(&opts, body).await?;
    if a.json {
        outln!("{reply}");
    } else {
        outln!("{op} done: {}", reply["ids"]);
        if reply["revoked"] == json!(true) {
            outln!("the key is revoked");
        }
        if let Some(note) = reply["revoke_note"].as_str() {
            outln!("note: {note}");
        }
    }
    Ok(exit::code(exit::OK))
}

/// Reads `/api/state` with the credentials of the caller.
async fn state(opts: &Opts) -> Result<Value, String> {
    let url = kc::http_url(
        opts.urls().first().ok_or("no hub URL configured")?,
        "/api/state",
    )?;
    let header = if kc::marker_path(&opts.token).is_some() {
        kc::auth_header(&opts.url, &opts.token)
            .await
            .map_err(|e| e.msg)?
    } else {
        format!("Bearer {}", opts.token.trim())
    };
    client::init();
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .header("authorization", header)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("the hub answered HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("bad state: {e}"))
}

fn tier_text(row: &Value) -> String {
    match row["tier"].as_i64() {
        Some(t) if t < 0 => "none (no standing)".into(),
        Some(t) => t.to_string(),
        None => "not tracked here (a token member has tier 2, the orchestrator tier 3)".into(),
    }
}

async fn show(a: &Args, member: Option<&str>) -> Result<ExitCode, String> {
    let opts = opts(a)?;
    let name = match member {
        Some(m) => m.to_owned(),
        None => client::request_ok(&opts, json!({"op": "whoami"})).await?["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    };
    let st = state(&opts).await?;
    let empty = Vec::new();
    let rows = st["standing"].as_array().unwrap_or(&empty);
    let row = rows
        .iter()
        .find(|r| r["member"] == name.as_str())
        .cloned()
        .unwrap_or_else(|| json!({"member": name, "counts": {}, "missing": []}));
    if a.json {
        outln!("{row}");
        return Ok(exit::code(exit::OK));
    }
    outln!("member {name}");
    outln!("tier {}", tier_text(&row));
    let counts: Vec<String> = row["counts"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, n)| n.as_u64().unwrap_or(0) > 0)
                .map(|(k, n)| format!("{k} {n}"))
                .collect()
        })
        .unwrap_or_default();
    outln!(
        "stamps {}",
        if counts.is_empty() {
            "none".into()
        } else {
            counts.join(", ")
        }
    );
    for r in row["reasons"].as_array().into_iter().flatten() {
        outln!("because {}", r.as_str().unwrap_or_default());
    }
    for p in row["open_penalties"].as_array().into_iter().flatten() {
        outln!(
            "open penalty stamp {} ({})",
            p["id"],
            p["kind"].as_str().unwrap_or_default()
        );
    }
    for m in row["missing"].as_array().into_iter().flatten() {
        outln!("missing {}", m.as_str().unwrap_or_default());
    }
    Ok(exit::code(exit::OK))
}
