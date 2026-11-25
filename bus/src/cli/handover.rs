//! `sommelier handover` command (steps 6 and 8).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use crate::exit;
use crate::guide::{PolicyInfo, handover};

const HELP: &str = "\
usage: sommelier handover --role <role> [--url URL] [--token TOKEN | --token-file PATH] [--data DIR] [--state-file FILE] [--json]
Prints the bounded takeover page (at most 80 lines) built from /api/state.
";

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }

    let mut role: Option<String> = None;
    let mut url: Option<String> = None;
    let mut token: Option<String> = None;
    let mut token_file: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut state_file: Option<PathBuf> = None;
    let mut json_out = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--role" => role = Some(next_val(&mut it, "--role")?),
            "--url" => url = Some(next_val(&mut it, "--url")?),
            "--token" => token = Some(next_val(&mut it, "--token")?),
            "--token-file" => token_file = Some(next_val(&mut it, "--token-file")?),
            "--data" => data_dir = Some(PathBuf::from(next_val(&mut it, "--data")?)),
            "--state-file" => state_file = Some(PathBuf::from(next_val(&mut it, "--state-file")?)),
            other if other.starts_with("--role=") => {
                role = Some(other.trim_start_matches("--role=").to_owned())
            }
            other if other.starts_with("--url=") => {
                url = Some(other.trim_start_matches("--url=").to_owned())
            }
            other if other.starts_with("--token=") => {
                token = Some(other.trim_start_matches("--token=").to_owned())
            }
            other if other.starts_with("--token-file=") => {
                token_file = Some(other.trim_start_matches("--token-file=").to_owned())
            }
            other if other.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(other.trim_start_matches("--data=")))
            }
            other if other.starts_with("--state-file=") => {
                state_file = Some(PathBuf::from(other.trim_start_matches("--state-file=")))
            }
            other => {
                eprintln!("sommelier handover: unknown or unimplemented argument `{other}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }

    let Some(role) = role else {
        eprintln!("sommelier handover: --role <role> is required");
        return Ok(exit::code(exit::WRONG_USAGE));
    };

    let data = data_dir.unwrap_or_else(crate::default_data_dir);
    let policy = PolicyInfo::load(None, &data)?;

    let state = if let Some(path) = state_file {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read state file {}: {e}", path.display()))?;
        serde_json::from_str::<Value>(&raw).map_err(|e| format!("invalid state JSON: {e}"))?
    } else {
        let env_var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let resolved_url = url
            .or_else(|| env_var("SOMMELIER_URL"))
            .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
        let resolved_token = match (token.or_else(|| env_var("SOMMELIER_TOKEN")), token_file) {
            (Some(t), _) => t,
            (None, file) => {
                let path = file
                    .or_else(|| env_var("SOMMELIER_TOKEN_FILE"))
                    .or_else(crate::key::client::default_marker_file)
                .ok_or("no token: pass --token, --token-file, set SOMMELIER_TOKEN, or run `sommelier join`")?;
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read token file {path}: {e}"))?
                    .trim()
                    .to_owned()
            }
        };
        match fetch_api_state(&resolved_url, &resolved_token).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("sommelier handover: {e}");
                return Ok(exit::code(exit::HUB_UNREACHABLE));
            }
        }
    };

    let page = handover::build(&role, &state, &policy);
    if json_out {
        println!("{}", json!({ "ok": true, "handover": page }));
    } else {
        print!("{}", page.text);
    }
    Ok(exit::code(exit::OK))
}

async fn fetch_api_state(url: &str, token: &str) -> Result<Value, String> {
    let mut u = reqwest::Url::parse(url.trim()).map_err(|_| "invalid hub URL".to_owned())?;
    let scheme = match u.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return Err("invalid hub URL scheme".into()),
    };
    u.set_scheme(scheme)
        .map_err(|_| "invalid hub URL scheme".to_owned())?;
    u.set_path("/api/state");
    u.set_query(None);
    u.set_fragment(None);
    crate::client::init();
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(u)
        .bearer_auth(token.trim())
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("state request failed with HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| e.without_url().to_string())?;
    serde_json::from_slice::<Value>(&bytes).map_err(|e| format!("invalid state JSON: {e}"))
}

fn next_val(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}
