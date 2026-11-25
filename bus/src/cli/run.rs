//! Game run commands (`sommelier run`, section 5.1, step 10a).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::client::{self, Opts};
use crate::exit;
use crate::land::RunFolderInfo;

const USAGE: &str = "\
usage:
  sommelier run request --sha S [--secs N] [--need K=V] [--label L] [--kind K] [--task ID] [--machine M] [--json]
  sommelier run register [--run-id ID] [--machine M] [--sha S] [--label L] [--kind K] [--task ID] [--folder DIR] [--json]
  sommelier run result RUN --outcome O [--folder DIR] [--sha S] [--label L] [--json]
  sommelier run list [--sha S] [--unregistered] [--json]
  sommelier run sweep --dir DIR [--machine M] [--json]
";

struct ParsedArgs {
    pos: Vec<String>,
    vals: HashMap<String, Vec<String>>,
    bools: Vec<String>,
}

impl ParsedArgs {
    fn parse(raw: &[String], value_flags: &[&str], bool_flags: &[&str]) -> Result<Self, String> {
        let mut a = Self {
            pos: Vec::new(),
            vals: HashMap::new(),
            bools: Vec::new(),
        };
        let mut it = raw.iter();
        while let Some(s) = it.next() {
            if s == "--" {
                a.pos.extend(it.by_ref().cloned());
                break;
            }
            let Some(flag) = s.strip_prefix("--") else {
                a.pos.push(s.clone());
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (flag, None),
            };
            if bool_flags.contains(&name) {
                a.bools.push(name.into());
            } else if value_flags.contains(&name) {
                let v = match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .cloned()
                        .ok_or_else(|| format!("--{name} needs a value"))?,
                };
                a.vals.entry(name.into()).or_default().push(v);
            } else {
                return Err(format!("unknown flag --{name}"));
            }
        }
        Ok(a)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.vals
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    fn flag(&self, name: &str) -> bool {
        self.bools.iter().any(|b| b == name)
    }
}

fn client_opts(a: &ParsedArgs) -> Result<Opts, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = a
        .get("url")
        .map(str::to_owned)
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
    let token = match (
        a.get("token")
            .map(str::to_owned)
            .or_else(|| env("SOMMELIER_TOKEN")),
        a.get("token-file"),
    ) {
        (Some(t), _) => t,
        (None, file) => {
            let path = file
                .map(str::to_owned)
                .or_else(|| env("SOMMELIER_TOKEN_FILE"))
                .or_else(crate::key::client::default_marker_file)
                .ok_or("no token: pass --token, --token-file, set SOMMELIER_TOKEN, or run `sommelier join`")?;
            fs::read_to_string(&path)
                .map_err(|e| format!("cannot read token file {path}: {e}"))?
                .trim()
                .to_owned()
        }
    };
    Ok(Opts { url, token })
}

async fn call_op(opts: &Opts, op: &str, body: Value) -> Result<Value, String> {
    let mut payload = match body {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    payload.insert("op".into(), json!(op));
    payload.insert("rid".into(), json!("cli-run"));
    client::request_ok(opts, Value::Object(payload)).await
}

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(USAGE.trim().to_owned());
    };
    match sub.as_str() {
        "request" => cmd_request(rest).await,
        "register" => cmd_register(rest).await,
        "result" => cmd_result(rest).await,
        "list" => cmd_list(rest).await,
        "sweep" => cmd_sweep(rest).await,
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            Ok(exit::code(exit::OK))
        }
        other => Err(format!("unknown subcommand `run {other}`\n{USAGE}")),
    }
}

async fn cmd_request(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(
        args,
        &[
            "url",
            "token",
            "token-file",
            "sha",
            "secs",
            "need",
            "label",
            "kind",
            "task",
            "machine",
        ],
        &["json"],
    )?;
    let sha = a
        .get("sha")
        .map(str::to_owned)
        .ok_or_else(|| "usage: sommelier run request --sha S [--json]".to_owned())?;
    let opts = client_opts(&a)?;
    let resp = call_op(
        &opts,
        "run.request",
        json!({
            "sha": sha,
            "secs": a.get("secs").and_then(|s| s.parse::<u64>().ok()).unwrap_or(60),
            "need": a.get("need").unwrap_or(""),
            "label": a.get("label").unwrap_or("requested"),
            "kind": a.get("kind").unwrap_or("check"),
            "task_id": a.get("task").unwrap_or(""),
            "machine": a.get("machine").unwrap_or(""),
        }),
    )
    .await?;
    if a.flag("json") {
        println!("{resp}");
    } else {
        println!(
            "requested run: {}",
            resp["run"]["run_id"].as_str().unwrap_or("")
        );
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_register(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(
        args,
        &[
            "url",
            "token",
            "token-file",
            "run-id",
            "machine",
            "sha",
            "label",
            "kind",
            "task",
            "folder",
        ],
        &["json"],
    )?;
    let opts = client_opts(&a)?;
    let resp = call_op(
        &opts,
        "run.register",
        json!({
            "run_id": a.get("run-id").unwrap_or(""),
            "machine": a.get("machine").unwrap_or(""),
            "sha": a.get("sha").unwrap_or(""),
            "label": a.get("label").unwrap_or(""),
            "kind": a.get("kind").unwrap_or("check"),
            "task_id": a.get("task").unwrap_or(""),
            "folder": a.get("folder").unwrap_or(""),
            "registered": true,
        }),
    )
    .await?;
    if a.flag("json") {
        println!("{resp}");
    } else {
        println!(
            "registered run: {}",
            resp["run"]["run_id"].as_str().unwrap_or("")
        );
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_result(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(
        args,
        &[
            "url",
            "token",
            "token-file",
            "run-id",
            "outcome",
            "folder",
            "sha",
            "label",
        ],
        &["json"],
    )?;
    let run_id = a
        .get("run-id")
        .map(str::to_owned)
        .or_else(|| a.pos.first().cloned())
        .ok_or_else(|| "usage: sommelier run result RUN --outcome O [--json]".to_owned())?;
    let outcome = a
        .get("outcome")
        .map(str::to_owned)
        .or_else(|| a.pos.get(1).cloned())
        .ok_or_else(|| "usage: sommelier run result RUN --outcome O [--json]".to_owned())?;
    let opts = client_opts(&a)?;
    let resp = call_op(
        &opts,
        "run.result",
        json!({
            "run_id": run_id,
            "outcome": outcome,
            "folder": a.get("folder").unwrap_or(""),
            "sha": a.get("sha").unwrap_or(""),
            "label": a.get("label").unwrap_or(""),
        }),
    )
    .await?;
    if a.flag("json") {
        println!("{resp}");
    } else {
        println!(
            "run result: {} -> {}",
            resp["run"]["run_id"].as_str().unwrap_or(""),
            resp["run"]["outcome"].as_str().unwrap_or("")
        );
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_list(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(
        args,
        &["url", "token", "token-file", "sha"],
        &["unregistered", "json"],
    )?;
    let opts = client_opts(&a)?;
    let resp = call_op(
        &opts,
        "run.list",
        json!({
            "sha": a.get("sha").unwrap_or(""),
            "unregistered": a.flag("unregistered"),
        }),
    )
    .await?;
    if a.flag("json") {
        println!("{resp}");
    } else {
        let runs = resp["runs"].as_array().cloned().unwrap_or_default();
        if runs.is_empty() {
            println!("no runs recorded");
        } else {
            for r in runs {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    r["run_id"].as_str().unwrap_or(""),
                    r["kind"].as_str().unwrap_or(""),
                    r["sha"].as_str().unwrap_or(""),
                    r["outcome"].as_str().unwrap_or("-"),
                    if r["registered"].as_bool() == Some(true) {
                        "registered"
                    } else {
                        "unregistered"
                    },
                );
            }
        }
    }
    Ok(exit::code(exit::OK))
}

fn scan_run_folders(dir: &Path, machine: &str) -> Result<Vec<RunFolderInfo>, String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("cannot read runs dir {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if !path.join("run.txt").exists() && !path.join("summary.txt").exists() {
            continue;
        }
        let label = name
            .split_once('-')
            .and_then(|(_, rest)| rest.split_once('-').map(|(_, l)| l))
            .unwrap_or(name)
            .to_owned();
        out.push(RunFolderInfo {
            run_id: name.to_owned(),
            folder: path.to_string_lossy().into_owned(),
            machine: machine.to_owned(),
            sha: String::new(),
            label,
        });
    }
    Ok(out)
}

async fn cmd_sweep(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(
        args,
        &["url", "token", "token-file", "dir", "machine"],
        &["json"],
    )?;
    let dir = a
        .get("dir")
        .ok_or_else(|| "usage: sommelier run sweep --dir DIR [--json]".to_owned())?;
    let machine = a.get("machine").unwrap_or("");
    let folders = scan_run_folders(Path::new(dir), machine)?;
    let opts = client_opts(&a)?;
    let resp = call_op(&opts, "run.sweep", json!({ "folders": folders })).await?;
    if a.flag("json") {
        println!("{resp}");
    } else {
        let n = resp["sweep"]["unregistered_runs"]
            .as_array()
            .map_or(0, Vec::len);
        println!("ok run sweep: {n} unregistered runs found");
    }
    Ok(exit::code(exit::OK))
}
