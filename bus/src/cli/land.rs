//! Landing commands (`sommelier land`, `sommelier scan`, section 5.1, step 10a).

use std::collections::HashMap;
use std::fs;
use std::process::{Command, ExitCode};

use serde_json::{Value, json};

use crate::client::{self, Opts};
use crate::exit;
use crate::land::{BranchRow, DeleteRemoteBranch, scan_licence_markers};

const LAND_USAGE: &str = "\
usage:
  sommelier land enqueue REF SHA [--task ID] [--priority P] [--json]
  sommelier land next [--limit N] [--claim] [--batch-id ID] [--base-sha SHA] [--json]
  sommelier land result BATCH --result R [--ref REF] [--sha SHA] [--base-sha SHA] [--verifications N] [--fail-step S] [--fail-class C] [--log TEXT] [--json]
  sommelier land approve REF SHA [--by NAME] [--json]
  sommelier land list [--state S] [--json]
  sommelier land sweep [--repo DIR] [--orphan-hours N] [--keep-days N] [--dry-run] [--json]
";

const SCAN_USAGE: &str = "\
usage:
  sommelier scan REF [--repo DIR] [--base SHA] [--json]
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
                .ok_or("no token: pass --token, --token-file, or set SOMMELIER_TOKEN")?;
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
    payload.insert("rid".into(), json!("cli-land"));
    client::request_ok(opts, Value::Object(payload)).await
}

pub async fn run(cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    if cmd == "scan" {
        return run_scan(args);
    }
    let Some((sub, rest)) = args.split_first() else {
        return Err(LAND_USAGE.trim().to_owned());
    };
    match sub.as_str() {
        "enqueue" => cmd_enqueue(rest).await,
        "next" => cmd_next(rest).await,
        "result" => cmd_result(rest).await,
        "approve" => cmd_approve(rest).await,
        "list" => cmd_list(rest).await,
        "sweep" => cmd_sweep(rest).await,
        "--help" | "-h" | "help" => {
            println!("{LAND_USAGE}");
            Ok(exit::code(exit::OK))
        }
        other => Err(format!("unknown subcommand `land {other}`\n{LAND_USAGE}")),
    }
}

async fn cmd_enqueue(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(
        args,
        &[
            "url",
            "token",
            "token-file",
            "task",
            "priority",
            "ref",
            "sha",
        ],
        &["json"],
    )?;
    let ref_name = a
        .get("ref")
        .map(str::to_owned)
        .or_else(|| a.pos.first().cloned())
        .ok_or_else(|| "usage: sommelier land enqueue REF SHA [--json]".to_owned())?;
    let sha = a
        .get("sha")
        .map(str::to_owned)
        .or_else(|| a.pos.get(1).cloned())
        .ok_or_else(|| "usage: sommelier land enqueue REF SHA [--json]".to_owned())?;
    let opts = client_opts(&a)?;
    let resp = call_op(
        &opts,
        "land.enqueue",
        json!({
            "ref": ref_name,
            "sha": sha,
            "task_id": a.get("task").unwrap_or(""),
            "priority": a.get("priority").unwrap_or("normal"),
        }),
    )
    .await?;
    if a.flag("json") {
        println!("{resp}");
    } else if resp["duplicate"].as_bool() == Some(true) {
        println!("already queued: {ref_name} {}", &sha[..sha.len().min(10)]);
    } else {
        println!("queued: {ref_name} {}", &sha[..sha.len().min(10)]);
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_next(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(
        args,
        &[
            "url",
            "token",
            "token-file",
            "limit",
            "batch-id",
            "base-sha",
        ],
        &["claim", "json"],
    )?;
    let limit = a
        .get("limit")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(8);
    let opts = client_opts(&a)?;
    let resp = call_op(
        &opts,
        "land.next",
        json!({
            "limit": limit,
            "claim": a.flag("claim"),
            "batch_id": a.get("batch-id").unwrap_or(""),
            "base_sha": a.get("base-sha").unwrap_or(""),
        }),
    )
    .await?;
    if a.flag("json") {
        println!("{resp}");
    } else {
        let entries = resp["entries"].as_array().cloned().unwrap_or_default();
        if entries.is_empty() {
            println!("queue empty");
        } else {
            for e in &entries {
                println!(
                    "{} {}",
                    e["ref"].as_str().unwrap_or(""),
                    e["sha"].as_str().unwrap_or("")
                );
            }
        }
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
            "result",
            "ref",
            "sha",
            "base-sha",
            "verifications",
            "fail-step",
            "fail-class",
            "log",
        ],
        &["json"],
    )?;
    let batch_id = a.pos.first().cloned().unwrap_or_default();
    let result = a
        .get("result")
        .map(str::to_owned)
        .or_else(|| a.pos.get(1).cloned())
        .ok_or_else(|| "usage: sommelier land result BATCH --result R [--json]".to_owned())?;
    let verifications = a
        .get("verifications")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1);
    let opts = client_opts(&a)?;
    let resp = call_op(
        &opts,
        "land.result",
        json!({
            "batch_id": batch_id,
            "result": result,
            "ref": a.get("ref").unwrap_or(""),
            "sha": a.get("sha").unwrap_or(""),
            "base_sha": a.get("base-sha").unwrap_or(""),
            "verifications": verifications,
            "fail_step": a.get("fail-step").unwrap_or(""),
            "fail_class": a.get("fail-class").unwrap_or(""),
            "log": a.get("log").unwrap_or(""),
        }),
    )
    .await?;
    if a.flag("json") {
        println!("{resp}");
    } else {
        let n = resp["updated"].as_array().map_or(0, Vec::len);
        println!("ok land result: {result} ({n} entries updated)");
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_approve(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(args, &["url", "token", "token-file", "by"], &["json"])?;
    let ref_name = a
        .pos
        .first()
        .cloned()
        .ok_or_else(|| "usage: sommelier land approve REF SHA [--json]".to_owned())?;
    let sha = a
        .pos
        .get(1)
        .cloned()
        .ok_or_else(|| "usage: sommelier land approve REF SHA [--json]".to_owned())?;
    let opts = client_opts(&a)?;
    let resp = call_op(
        &opts,
        "land.approve",
        json!({
            "ref": ref_name,
            "sha": sha,
            "by": a.get("by").unwrap_or(""),
        }),
    )
    .await?;
    if a.flag("json") {
        println!("{resp}");
    } else {
        println!("approved: {ref_name} {}", &sha[..sha.len().min(10)]);
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_list(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(args, &["url", "token", "token-file", "state"], &["json"])?;
    let opts = client_opts(&a)?;
    let resp = call_op(
        &opts,
        "land.list",
        json!({ "state": a.get("state").unwrap_or("") }),
    )
    .await?;
    if a.flag("json") {
        println!("{resp}");
    } else {
        let rows = resp["landing"].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            println!("landing queue is empty");
        } else {
            for r in rows {
                println!(
                    "{}\t{}\t{}\t{}",
                    r["ref"].as_str().unwrap_or(""),
                    r["sha"].as_str().unwrap_or(""),
                    r["state"].as_str().unwrap_or(""),
                    r["batch_id"].as_str().unwrap_or("-"),
                );
            }
        }
    }
    Ok(exit::code(exit::OK))
}

fn collect_git_branches(repo: &str) -> Result<Vec<BranchRow>, String> {
    let out = Command::new("git")
        .args([
            "-C",
            repo,
            "for-each-ref",
            "--format=%(refname:short)%09%(objectname)%09%(committerdate:unix)",
            "refs/remotes",
        ])
        .output()
        .map_err(|e| format!("git for-each-ref failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let Some(full_ref) = parts.next() else {
            continue;
        };
        let Some(sha) = parts.next() else {
            continue;
        };
        let ts = parts
            .next()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if full_ref.ends_with("/HEAD") || full_ref.ends_with("/main") {
            continue;
        }
        let (remote, ref_name) = match full_ref.split_once('/') {
            Some((r, b)) => (r.to_owned(), b.to_owned()),
            None => ("origin".to_owned(), full_ref.to_owned()),
        };
        let ahead = Command::new("git")
            .args(["-C", repo, "rev-list", "--count", &format!("main..{sha}")])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8_lossy(&o.stdout)
                        .trim()
                        .parse::<u64>()
                        .ok()
                } else {
                    None
                }
            })
            .unwrap_or(1);
        rows.push(BranchRow {
            remote,
            ref_name,
            head_sha: sha.trim().to_owned(),
            ahead,
            behind: 0,
            last_commit_at: ts,
            task_id: String::new(),
            state: String::new(),
            seen_at: 0,
            msg_id: 0,
        });
    }
    Ok(rows)
}

async fn cmd_sweep(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(
        args,
        &[
            "url",
            "token",
            "token-file",
            "repo",
            "orphan-hours",
            "keep-days",
        ],
        &["dry-run", "json"],
    )?;
    let branches = match a.get("repo") {
        Some(repo) => Some(collect_git_branches(repo)?),
        None => None,
    };
    let mut body = serde_json::Map::new();
    if let Some(b) = branches {
        body.insert("branches".into(), json!(b));
    }
    if let Some(h) = a.get("orphan-hours").and_then(|s| s.parse::<u64>().ok()) {
        body.insert("orphan_hours".into(), json!(h));
    }
    if let Some(d) = a.get("keep-days").and_then(|s| s.parse::<u64>().ok()) {
        body.insert("keep_days".into(), json!(d));
    }

    let opts = client_opts(&a)?;
    let resp = call_op(&opts, "land.sweep", Value::Object(body)).await?;

    if !a.flag("dry-run")
        && let Some(repo) = a.get("repo")
        && let Ok(deletes) = serde_json::from_value::<Vec<DeleteRemoteBranch>>(
            resp["sweep"]["delete_branches"].clone(),
        )
    {
        for d in deletes {
            let _ = Command::new("git")
                .args(["-C", repo, "push", &d.remote, "--delete", &d.ref_name])
                .output();
        }
    }

    if a.flag("json") {
        println!("{resp}");
    } else {
        let updated = resp["sweep"]["updated_branches"]
            .as_array()
            .map_or(0, Vec::len);
        let orphans = resp["sweep"]["orphan_alerts"]
            .as_array()
            .map_or(0, Vec::len);
        let deletes = resp["sweep"]["delete_branches"]
            .as_array()
            .map_or(0, Vec::len);
        println!(
            "ok land sweep: {updated} branch updates, {orphans} orphan alerts, {deletes} remote deletions"
        );
    }
    Ok(exit::code(exit::OK))
}

fn run_scan(args: &[String]) -> Result<ExitCode, String> {
    let a = ParsedArgs::parse(args, &["repo", "base"], &["json"])?;
    if a.pos.first().map(String::as_str) == Some("--help") {
        println!("{SCAN_USAGE}");
        return Ok(exit::code(exit::OK));
    }
    let Some(ref_name) = a.pos.first() else {
        return Err(SCAN_USAGE.trim().to_owned());
    };
    let repo = a.get("repo").unwrap_or(".");
    let base = a.get("base").unwrap_or("main");
    let out = Command::new("git")
        .args(["-C", repo, "diff", "-U0", &format!("{base}...{ref_name}")])
        .output()
        .map_err(|e| format!("git diff failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    let diff = String::from_utf8_lossy(&out.stdout);
    let hits = scan_licence_markers(&diff);
    let clean = hits.is_empty();
    if a.flag("json") {
        println!(
            "{}",
            json!({ "ref": ref_name, "clean": clean, "hits": hits })
        );
    } else if clean {
        println!("clean: {ref_name}");
    } else {
        for h in &hits {
            println!("licence marker hit: {h}");
        }
    }
    if clean {
        Ok(exit::code(exit::OK))
    } else {
        Ok(exit::code(exit::REFUSED))
    }
}
