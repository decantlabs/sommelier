//! `sommelier review`: takes an assigned review and sends the verdict.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::client::{self, Opts};
use crate::exit;
use crate::key::client as kc;
use crate::key::crypto::sign_request;
use crate::review::escape_line;
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

/// Removes the terminal escape sequences from text that a hub sent and shows every other control,
/// invisible or bidirectional character as a visible escape. A hostile hub cannot move the cursor, change
/// the colours or the title or hide text in what the reviewer reads. Line breaks stay.
pub(crate) fn strip_terminal(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut it = text.chars();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            kept.push(c);
            continue;
        }
        match it.next() {
            // CSI: parameters and intermediates up to the final byte 0x40 to 0x7e.
            Some('[') => {
                for d in it.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&d) {
                        break;
                    }
                }
            }
            // OSC, DCS, SOS, PM, APC: up to BEL or the string terminator ESC backslash.
            Some(']' | 'P' | 'X' | '^' | '_') => {
                while let Some(d) = it.next() {
                    if d == '\u{7}' {
                        break;
                    }
                    if d == '\u{1b}' {
                        it.next();
                        break;
                    }
                }
            }
            // Any other escape takes one character.
            _ => {}
        }
    }
    let mut out = String::with_capacity(kept.len());
    for (i, line) in kept.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&escape_line(line));
    }
    out
}

/// The hub id that this client pinned for `url`, from the pin file beside the key file.
fn pinned_hub_id(f: &Flags, url: &str) -> Result<String, String> {
    let pins = kc::pins_path(&kc::key_path(&f.data));
    let name = url.trim().trim_end_matches('/');
    let text = std::fs::read_to_string(&pins).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.rsplit_once(' '))
        .find(|(u, _)| *u == name)
        .map(|(_, id)| id.to_owned())
        .ok_or_else(|| format!("no hub id is pinned for {name} in {}", pins.display()))
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
            strip_terminal(r["task"].as_str().unwrap_or("?")),
            r["n"],
            strip_terminal(r["role"].as_str().unwrap_or("required")),
            strip_terminal(r["submission"].as_str().unwrap_or("?"))
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
    print!("{}", strip_terminal(prompt));
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
        strip_terminal(v["verdict"].as_str().unwrap_or("?")),
        task,
        n,
        strip_terminal(v["state"].as_str().unwrap_or("?"))
    );
    if let Some(why) = v["refusal"].as_str() {
        println!("not queued for landing: {}", strip_terminal(why));
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
        // The signature names the submission and the hub. The submission comes from the assigned review,
        // the hub id from the pin made when this client first connected to the hub.
        let list = call(f, "review.next", json!({})).await?;
        let submission = list["reviews"]
            .as_array()
            .and_then(|l| {
                l.iter()
                    .find(|r| r["task"].as_str() == Some(task.as_str()) && r["n"] == n)
            })
            .and_then(|r| r["submission"].as_str())
            .ok_or_else(|| format!("review {task}/{n} is not assigned to you"))?
            .to_owned();
        let hub_id = pinned_hub_id(f, &opts(f)?.url)?;
        body["sig"] = json!(sign_request(
            &key.key,
            &verdict_request(&task, n, &submission, &hub_id, &text)
        ));
    }
    let v = call(f, "review.verdict", body).await?;
    if f.json {
        println!("{v}");
        return Ok(exit::code(exit::OK));
    }
    println!(
        "recorded: {} for {}/{}, the submission is now {}",
        strip_terminal(v["verdict"].as_str().unwrap_or("?")),
        task,
        n,
        strip_terminal(v["state"].as_str().unwrap_or("?"))
    );
    if let Some(why) = v["refusal"].as_str() {
        println!("not queued for landing: {}", strip_terminal(why));
    }
    Ok(exit::code(exit::OK))
}

#[cfg(test)]
mod tests {
    use super::strip_terminal;

    #[test]
    fn terminal_sequences_from_a_hub_are_removed_and_the_rest_is_shown() {
        let hostile = "ok\u{1b}[2J\u{1b}[31mred\u{1b}]0;new title\u{7}more\u{1b}]8;;http://x\u{1b}\u{5c}link\u{1b}Pq data\u{1b}\u{5c}end\rhid\u{202e}rtl\u{9b}c1";
        let out = strip_terminal(hostile);
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(!out.contains('\r') && !out.contains('\u{7}') && !out.contains('\u{9b}'));
        assert!(!out.contains('\u{202e}'));
        assert!(out.starts_with("okredmorelinkend"), "{out:?}");
        assert!(out.contains("\\u{d}hid") && out.contains("\\u{202e}rtl"));
        assert_eq!(strip_terminal("a\nb\n"), "a\nb\n");
        assert_eq!(strip_terminal("plain ünï"), "plain ünï");
    }
}
