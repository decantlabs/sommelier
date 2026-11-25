//! `sommelier task` backlog commands (section 5.1, step 8).

use std::collections::HashMap;
use std::fs;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::brief;
use crate::client::{self, Opts};
use crate::exit;
use crate::task::{board_row_to_task_payload, parse_board_tsv};

const USAGE: &str = "\
usage:
  sommelier task add --kind K --title T --scope G --done-when D --brief FILE [--id ID] [--priority P] [--after ID] [--branch B] [--accepts C] [--prior-art] [--brief-sha256 H] [--json]
  sommelier task list [--state S] [--mine] [--json]
  sommelier task show ID [--json]
  sommelier task cancel ID [--reason TEXT] [--json]
  sommelier task requeue ID [--reason TEXT] [--note TEXT] [--json]
  sommelier task import-board FILE [--dry-run] [--json]
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

    fn all(&self, name: &str) -> Vec<String> {
        self.vals.get(name).cloned().unwrap_or_default()
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
    payload.insert("rid".into(), json!("cli-task"));
    client::request_ok(opts, Value::Object(payload)).await
}

fn slug_id(title: &str) -> String {
    let slug: String = title
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = slug.trim_matches('-');
    let short: String = trimmed.chars().take(32).collect();
    if short.is_empty() {
        format!("t-{}", crate::time::now_ms() / 1000)
    } else {
        short
    }
}

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    let Some((sub, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return Ok(exit::code(exit::WRONG_USAGE));
    };

    match sub.as_str() {
        "add" => cmd_add(rest).await,
        "list" => cmd_list(rest).await,
        "show" => cmd_show(rest).await,
        "cancel" => cmd_cancel(rest).await,
        "requeue" => cmd_requeue(rest).await,
        "import-board" => cmd_import_board(rest).await,
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            Ok(exit::code(exit::OK))
        }
        other => {
            eprintln!("sommelier: unknown subcommand `task {other}`\n{USAGE}");
            Ok(exit::code(exit::WRONG_USAGE))
        }
    }
}

async fn cmd_add(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(
        raw,
        &[
            "id",
            "kind",
            "title",
            "scope",
            "done-when",
            "brief",
            "brief-sha256",
            "priority",
            "after",
            "branch",
            "accepts",
            "track",
            "url",
            "token",
            "token-file",
        ],
        &["prior-art", "json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    let kind = a.get("kind").ok_or("--kind is required")?;
    let title = a.get("title").ok_or("--title is required")?;
    let scope = a.get("scope").ok_or("--scope is required")?;
    let done_when = a.get("done-when").ok_or("--done-when is required")?;
    let brief_path = a.get("brief").ok_or("--brief FILE is required")?;
    let id = a
        .get("id")
        .map(str::to_owned)
        .unwrap_or_else(|| slug_id(title));
    let brief_bytes =
        fs::read(brief_path).map_err(|e| format!("cannot read brief {brief_path}: {e}"))?;
    let expected_sha = a.get("brief-sha256");
    let rec = match brief::put_brief_http(&opts, &id, &brief_bytes, expected_sha).await {
        Ok(r) => r,
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": false, "code": exit::REFUSED, "reason": e })
                );
            } else {
                eprintln!("refused: {e}");
            }
            return Ok(exit::code(exit::REFUSED));
        }
    };

    let branch = a
        .get("branch")
        .map(str::to_owned)
        .unwrap_or_else(|| format!("work/{id}"));
    let payload = json!({
        "id": id,
        "kind": kind,
        "title": title,
        "scope": scope,
        "done_when": done_when,
        "priority": a.get("priority").unwrap_or("normal"),
        "deps": a.all("after"),
        "branch": branch,
        "accepts": a.get("accepts").unwrap_or(""),
        "track": a.get("track").unwrap_or(""),
        "prior_art": a.flag("prior-art"),
        "brief_sha256": rec.sha256,
    });

    match call_op(&opts, "task.add", payload).await {
        Ok(reply) => {
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "task": reply["task"],
                        "brief_sha256": rec.sha256,
                        "msg_id": reply["msg_id"],
                    })
                );
            } else {
                println!(
                    "ok task {} added (brief sha256 {})",
                    id,
                    &rec.sha256[..12.min(rec.sha256.len())]
                );
            }
            Ok(exit::code(exit::OK))
        }
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": false, "code": exit::REFUSED, "reason": e })
                );
            } else {
                eprintln!("refused: {e}");
            }
            Ok(exit::code(exit::REFUSED))
        }
    }
}

async fn cmd_list(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(
        raw,
        &["state", "url", "token", "token-file"],
        &["mine", "json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    let reply = call_op(
        &opts,
        "task.list",
        json!({
            "state": a.get("state").unwrap_or(""),
            "mine": a.flag("mine"),
        }),
    )
    .await?;
    if json_out {
        println!(
            "{}",
            json!({ "ok": true, "code": exit::OK, "tasks": reply["tasks"] })
        );
    } else if let Some(tasks) = reply["tasks"].as_array() {
        if tasks.is_empty() {
            println!("no tasks");
        } else {
            for t in tasks {
                println!(
                    "{} [{}] {} ({}, score {})",
                    t["id"].as_str().unwrap_or("?"),
                    t["state"].as_str().unwrap_or("?"),
                    t["title"].as_str().unwrap_or(""),
                    t["priority"].as_str().unwrap_or("normal"),
                    t["score"].as_i64().unwrap_or(0),
                );
            }
        }
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_show(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(raw, &["url", "token", "token-file"], &["json"]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(id) = a.pos.first() else {
        eprintln!("sommelier: usage: sommelier task show ID [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    match call_op(&opts, "task.show", json!({ "id": id })).await {
        Ok(reply) => {
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "task": reply["task"],
                        "score": reply["score"],
                        "lease": reply["lease"],
                        "checkpoints": reply["checkpoints"],
                    })
                );
            } else {
                let t = &reply["task"];
                println!(
                    "{} [{}] {} priority={} attempt={} scope={}",
                    t["id"].as_str().unwrap_or("?"),
                    t["state"].as_str().unwrap_or("?"),
                    t["title"].as_str().unwrap_or(""),
                    t["priority"].as_str().unwrap_or("normal"),
                    t["attempt"].as_u64().unwrap_or(1),
                    t["scope"].as_str().unwrap_or(""),
                );
            }
            Ok(exit::code(exit::OK))
        }
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": false, "code": exit::REFUSED, "reason": e })
                );
            } else {
                eprintln!("refused: {e}");
            }
            Ok(exit::code(exit::REFUSED))
        }
    }
}

async fn cmd_cancel(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(raw, &["reason", "url", "token", "token-file"], &["json"]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(id) = a.pos.first() else {
        eprintln!("sommelier: usage: sommelier task cancel ID [--reason TEXT] [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    match call_op(
        &opts,
        "task.cancel",
        json!({ "id": id, "reason": a.get("reason").unwrap_or("cancelled") }),
    )
    .await
    {
        Ok(reply) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": true, "code": exit::OK, "id": id, "state": "cancelled", "msg_id": reply["msg_id"] })
                );
            } else {
                println!("ok task {id} cancelled");
            }
            Ok(exit::code(exit::OK))
        }
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": false, "code": exit::REFUSED, "reason": e })
                );
            } else {
                eprintln!("refused: {e}");
            }
            Ok(exit::code(exit::REFUSED))
        }
    }
}

async fn cmd_requeue(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(
        raw,
        &["reason", "note", "url", "token", "token-file"],
        &["json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(id) = a.pos.first() else {
        eprintln!("sommelier: usage: sommelier task requeue ID [--reason TEXT] [--note TEXT] [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    match call_op(
        &opts,
        "task.requeue",
        json!({
            "id": id,
            "reason": a.get("reason").unwrap_or("requeued"),
            "resume_note": a.get("note").unwrap_or(""),
        }),
    )
    .await
    {
        Ok(reply) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": true, "code": exit::OK, "id": id, "attempt": reply["attempt"], "msg_id": reply["msg_id"] })
                );
            } else {
                println!(
                    "ok task {id} requeued (attempt {})",
                    reply["attempt"].as_u64().unwrap_or(1)
                );
            }
            Ok(exit::code(exit::OK))
        }
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": false, "code": exit::REFUSED, "reason": e })
                );
            } else {
                eprintln!("refused: {e}");
            }
            Ok(exit::code(exit::REFUSED))
        }
    }
}

async fn cmd_import_board(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(raw, &["url", "token", "token-file"], &["dry-run", "json"]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(file) = a.pos.first() else {
        eprintln!("sommelier: usage: sommelier task import-board FILE [--dry-run] [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let json_out = a.flag("json");
    let dry_run = a.flag("dry-run");
    let text = fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let rows = parse_board_tsv(&text)?;

    let mut imported = Vec::new();
    if dry_run {
        for row in &rows {
            imported.push(board_row_to_task_payload(row));
        }
    } else {
        let opts = client_opts(&a)?;
        for row in &rows {
            let mut payload = board_row_to_task_payload(row);
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("upsert".into(), json!(true));
            }
            call_op(&opts, "task.add", payload.clone()).await?;
            imported.push(payload);
        }
    }

    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "code": exit::OK,
                "count": imported.len(),
                "dry_run": dry_run,
                "tasks": imported,
            })
        );
    } else {
        println!(
            "ok {} {} tasks from {}",
            if dry_run { "parsed" } else { "imported" },
            imported.len(),
            file
        );
    }
    Ok(exit::code(exit::OK))
}
