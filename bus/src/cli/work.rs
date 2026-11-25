//! Worker cycle commands (`next`, `work`, `checkpoint`, `done`, `stuck`, `decline`, `ask`, `offers`, section 5.1, 8.4, step 8).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::brief;
use crate::client::{self, Opts};
use crate::exit;

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
    payload.insert("rid".into(), json!("cli-work"));
    client::request_ok(opts, Value::Object(payload)).await
}

pub async fn run(cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    match cmd {
        "next" => cmd_next(args).await,
        "work" => cmd_work(args).await,
        "checkpoint" => cmd_checkpoint(args).await,
        "done" => cmd_done(args).await,
        "stuck" => cmd_stuck(args).await,
        "decline" => cmd_decline(args).await,
        "ask" => cmd_ask(args).await,
        "offers" => cmd_offers(args).await,
        other => Err(format!("unknown work command `{other}`")),
    }
}

async fn cmd_next(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(
        raw,
        &["wait", "class", "url", "token", "token-file"],
        &["peek", "json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let json_out = a.flag("json");
    let peek = a.flag("peek");
    let wait_secs: u64 = match a.get("wait") {
        Some(v) => v
            .parse()
            .map_err(|_| format!("--wait needs a number, got `{v}`"))?,
        None => 0,
    };
    let class = a.get("class").unwrap_or("");
    let opts = client_opts(&a)?;
    let deadline = Instant::now() + Duration::from_secs(wait_secs);

    loop {
        let reply = call_op(&opts, "task.next", json!({ "peek": peek, "class": class })).await?;
        if reply["found"].as_bool() == Some(true) {
            let task = &reply["task"];
            let ready_count = reply["ready_count"].as_u64().unwrap_or(1);
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "ready_count": ready_count,
                        "score": reply["score"],
                        "task": task,
                    })
                );
            } else {
                println!(
                    "{} ready for you, top: {} {} ({})",
                    ready_count,
                    task["id"].as_str().unwrap_or("?"),
                    task["title"].as_str().unwrap_or(""),
                    task["priority"].as_str().unwrap_or("normal"),
                );
            }
            return Ok(exit::code(exit::OK));
        }
        if Instant::now() >= deadline {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": false, "code": exit::NO_WORK, "reason": "no ready task" })
                );
            } else {
                println!("no work now");
            }
            return Ok(exit::code(exit::NO_WORK));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn cmd_work(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(
        raw,
        &[
            "worker",
            "machine",
            "class",
            "brief-out",
            "brief-sha256",
            "url",
            "token",
            "token-file",
        ],
        &["supervised", "json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(task_id) = a.pos.first() else {
        eprintln!("sommelier: usage: sommelier work ID [--brief-out PATH] [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    let reply = match call_op(
        &opts,
        "task.work",
        json!({
            "task": task_id,
            "worker": a.get("worker").unwrap_or(""),
            "machine": a.get("machine").unwrap_or(""),
            "class": a.get("class").unwrap_or(""),
            "supervised": a.flag("supervised"),
            "brief_sha256": a.get("brief-sha256").unwrap_or(""),
        }),
    )
    .await
    {
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

    let task = &reply["task"];
    let lease = &reply["lease"];
    let brief_sha = task["brief_sha256"].as_str().unwrap_or("");
    let brief_out = a.get("brief-out").unwrap_or(".sommelier/brief.md");
    if !brief_sha.is_empty() {
        let key = if brief::get_brief_http(&opts, task_id, Some(brief_sha))
            .await
            .is_ok()
        {
            task_id.as_str()
        } else {
            brief_sha
        };
        let (text, _actual) = match brief::get_brief_http(&opts, key, Some(brief_sha)).await {
            Ok(b) => b,
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
        let out_path = Path::new(brief_out);
        if let Some(parent) = out_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| format!("cannot create brief dir: {e}"))?;
        }
        fs::write(out_path, text).map_err(|e| format!("cannot write {brief_out}: {e}"))?;
    }

    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "code": exit::OK,
                "task": task,
                "lease": lease,
                "brief": brief_out,
            })
        );
    } else {
        println!(
            "ok task {} branch {} lease until {} brief {}",
            task_id,
            task["branch"].as_str().unwrap_or("-"),
            lease["expires_at"].as_u64().unwrap_or(0),
            brief_out,
        );
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_checkpoint(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(
        raw,
        &[
            "file",
            "branch",
            "sha",
            "source",
            "url",
            "token",
            "token-file",
        ],
        &["pushed", "json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(task_id) = a.pos.first() else {
        eprintln!("sommelier: usage: sommelier checkpoint ID --file FILE [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let Some(file) = a.get("file") else {
        eprintln!("sommelier: --file is required");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let note = fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?;
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    match call_op(
        &opts,
        "task.checkpoint",
        json!({
            "task": task_id,
            "note": note,
            "branch": a.get("branch").unwrap_or(""),
            "sha": a.get("sha").unwrap_or(""),
            "pushed": a.flag("pushed"),
            "source": a.get("source").unwrap_or("agent"),
        }),
    )
    .await
    {
        Ok(reply) => {
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "seq": reply["seq"],
                        "expires_at": reply["expires_at"],
                        "msg_id": reply["msg_id"],
                    })
                );
            } else {
                println!(
                    "ok checkpoint {} stored, lease until {}",
                    reply["seq"].as_u64().unwrap_or(1),
                    reply["expires_at"].as_u64().unwrap_or(0),
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

async fn cmd_done(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(
        raw,
        &[
            "ref",
            "branch",
            "sha",
            "result-file",
            "remote-sha",
            "url",
            "token",
            "token-file",
        ],
        &["json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(task_id) = a.pos.first() else {
        eprintln!(
            "sommelier: usage: sommelier done ID --ref BRANCH --sha SHA --result-file FILE [--json]"
        );
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let Some(result_file) = a.get("result-file") else {
        eprintln!("sommelier: --result-file is required");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let Some(sha) = a.get("sha") else {
        eprintln!("sommelier: --sha is required");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let result_text =
        fs::read_to_string(result_file).map_err(|e| format!("cannot read {result_file}: {e}"))?;
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    match call_op(
        &opts,
        "task.done",
        json!({
            "task": task_id,
            "ref": a.get("ref").or_else(|| a.get("branch")).unwrap_or(""),
            "sha": sha,
            "result_text": result_text,
            "remote_sha_path": a.get("remote-sha").unwrap_or(""),
        }),
    )
    .await
    {
        Ok(reply) => {
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "state": reply["state"],
                        "msg_id": reply["msg_id"],
                    })
                );
            } else {
                println!("ok delivered");
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

async fn cmd_stuck(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(raw, &["why", "url", "token", "token-file"], &["json"]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(task_id) = a.pos.first() else {
        eprintln!("sommelier: usage: sommelier stuck ID --why TEXT [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let Some(why) = a.get("why") else {
        eprintln!("sommelier: --why is required");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    match call_op(&opts, "task.stuck", json!({ "task": task_id, "why": why })).await {
        Ok(reply) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": true, "code": exit::OK, "state": "stuck", "msg_ids": reply["msg_ids"] })
                );
            } else {
                println!("ok stuck recorded for {task_id}");
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

async fn cmd_decline(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(raw, &["why", "url", "token", "token-file"], &["json"]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let Some(task_id) = a.pos.first() else {
        eprintln!("sommelier: usage: sommelier decline ID --why TEXT [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    match call_op(
        &opts,
        "task.decline",
        json!({ "task": task_id, "why": a.get("why").unwrap_or("declined") }),
    )
    .await
    {
        Ok(reply) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "ok": true, "code": exit::OK, "msg_ids": reply["msg_ids"] })
                );
            } else {
                println!("ok declined offer for {task_id}");
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

async fn cmd_ask(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(raw, &["task", "url", "token", "token-file"], &["json"]) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let (task_id, text_parts): (&str, &[String]) = if let Some(tid) = a.get("task") {
        (tid, &a.pos)
    } else if let Some((first, rest)) = a.pos.split_first() {
        (first.as_str(), rest)
    } else {
        eprintln!("sommelier: usage: sommelier ask --task ID TEXT [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let text = text_parts.join(" ");
    if text.trim().is_empty() {
        eprintln!("sommelier: ask requires question text");
        return Ok(exit::code(exit::WRONG_USAGE));
    }
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    let reply = call_op(&opts, "task.ask", json!({ "task": task_id, "text": text })).await?;
    if json_out {
        println!(
            "{}",
            json!({ "ok": true, "code": exit::OK, "msg_id": reply["msg_id"] })
        );
    } else {
        println!("ok consult sent to the brain");
    }
    Ok(exit::code(exit::OK))
}

async fn cmd_offers(raw: &[String]) -> Result<ExitCode, String> {
    let a = match ParsedArgs::parse(
        raw,
        &["top", "url", "token", "token-file"],
        &["all", "json"],
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    let top: u64 = match a.get("top") {
        Some(v) => v
            .parse()
            .map_err(|_| format!("--top needs a number, got `{v}`"))?,
        None => 20,
    };
    let json_out = a.flag("json");
    let opts = client_opts(&a)?;
    let reply = call_op(
        &opts,
        "task.offers",
        json!({ "top": top, "all": a.flag("all") }),
    )
    .await?;
    if json_out {
        println!(
            "{}",
            json!({ "ok": true, "code": exit::OK, "offers": reply["offers"] })
        );
    } else if let Some(offers) = reply["offers"].as_array() {
        if offers.is_empty() {
            println!("no open offers");
        } else {
            for o in offers {
                println!(
                    "{} task {} to {} class {} expires {}",
                    o["id"].as_str().unwrap_or("?"),
                    o["task_id"].as_str().unwrap_or("?"),
                    o["to_owner"].as_str().unwrap_or("?"),
                    o["class"].as_str().unwrap_or("-"),
                    o["expires_at"].as_u64().unwrap_or(0),
                );
            }
        }
    }
    Ok(exit::code(exit::OK))
}
