//! `sommelier verify`: the owner's requests for the clean build of a submission.

use std::process::ExitCode;

use serde_json::json;

use super::review::{call, parse};
use crate::exit;

const HELP: &str = "\
usage:
  sommelier verify override SUBMISSION --conclusion passed|failed --reason TEXT [--json]
  sommelier verify retry SUBMISSION [--json]
  common flags: --url URL, --token T, --token-file FILE, --data DIR
override  orchestrator only. Marks a `gated` submission verified or failed without a run, with your name and
          the reason in the log. It is the way to go on while GitHub is down.
retry     orchestrator only. Puts a submission whose clean build failed back to `gated`.
";

/// The help text of this command.
pub fn help() -> &'static str {
    HELP
}

/// Entry point of `sommelier verify`.
pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }
    let f = parse(args)?;
    let sub = |f: &super::review::Flags| -> Result<String, String> {
        f.rest
            .get(1)
            .cloned()
            .ok_or_else(|| "name the submission: its 64 character sha256".to_string())
    };
    let (op, body) = match f.rest.first().map(String::as_str) {
        Some("override") => (
            "verify.override",
            json!({
                "submission": sub(&f)?,
                "conclusion": f.conclusion.as_deref().ok_or("--conclusion passed|failed is needed")?,
                "reason": f.reason.as_deref().ok_or("--reason TEXT is needed")?,
            }),
        ),
        Some("retry") => ("verify.retry", json!({"sha256": sub(&f)?})),
        _ => {
            return Err(
                "usage: sommelier verify override SUBMISSION --conclusion passed|failed --reason TEXT | retry SUBMISSION"
                    .into(),
            );
        }
    };
    let v = call(&f, op, body).await?;
    if f.json {
        println!("{v}");
    } else {
        println!(
            "{}: the submission is now {}",
            op,
            v["state"].as_str().unwrap_or("?")
        );
    }
    Ok(exit::code(exit::OK))
}
