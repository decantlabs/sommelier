//! Standby hub promotion command (`sommelier promote`, section 13.5, step 12b).

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;

use crate::exit;
use crate::ha::{FileWitness, HaConfig, HttpWitness, PromoteOpts, Witness, handback, promote};
use crate::time::now_ms;

const HELP: &str = "\
usage: sommelier promote [--confirm CODE | --handback] [--primary URL] [--witness URL_OR_FILE] [--witness-token TOKEN] [--holder NAME] [--data DIR] [--guest WHO]... [--json]
Promotes a standby hub to primary after verifying stale witness, unreachable primary, and owner confirmation.
";

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }

    let mut confirm: Option<String> = None;
    let mut is_handback = false;
    let mut primary_url: Option<String> = None;
    let mut witness_spec: Option<String> = None;
    let mut witness_token: Option<String> = None;
    let mut holder = String::from("standby");
    let mut data_dir: Option<PathBuf> = None;
    let mut guests: Vec<String> = Vec::new();
    let mut json_out = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--handback" => is_handback = true,
            "--confirm" => confirm = Some(next_val(&mut it, "--confirm")?),
            "--primary" => primary_url = Some(next_val(&mut it, "--primary")?),
            "--witness" => witness_spec = Some(next_val(&mut it, "--witness")?),
            "--witness-token" => witness_token = Some(next_val(&mut it, "--witness-token")?),
            "--holder" => holder = next_val(&mut it, "--holder")?,
            "--data" => data_dir = Some(PathBuf::from(next_val(&mut it, "--data")?)),
            "--guest" => guests.push(next_val(&mut it, "--guest")?),
            other => {
                eprintln!("sommelier promote: unknown argument `{other}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }

    let data = data_dir.unwrap_or_else(crate::default_data_dir);
    let witness = match witness_spec {
        Some(spec) if spec.starts_with("http://") || spec.starts_with("https://") => {
            let tok = witness_token
                .or_else(|| std::env::var("SOMMELIER_WITNESS_TOKEN").ok())
                .unwrap_or_default();
            Witness::Http(HttpWitness::new(spec, tok))
        }
        Some(path) => {
            let p = path.strip_prefix("file://").unwrap_or(&path);
            Witness::File(FileWitness::new(p))
        }
        None => Witness::File(FileWitness::new(data.join("witness.json"))),
    };

    let now_sec = now_ms() / 1000;

    if is_handback {
        let primary = primary_url
            .as_deref()
            .ok_or("sommelier promote --handback requires --primary <URL>")?;
        match handback(&data, &holder, primary, &witness, now_sec).await {
            Ok(outcome) => {
                if json_out {
                    println!("{}", json!({ "ok": true, "handback": outcome }));
                } else {
                    println!(
                        "handed back to {} at epoch {} (previous {})",
                        outcome.holder, outcome.epoch, outcome.previous_epoch
                    );
                }
                return Ok(exit::code(exit::OK));
            }
            Err(e) => {
                eprintln!("sommelier promote: {e}");
                return Ok(exit::code(exit::REFUSED));
            }
        }
    }

    let opts = PromoteOpts {
        data_dir: data,
        primary_url,
        new_holder: holder,
        confirm_code: confirm,
        expected_confirm: std::env::var("SOMMELIER_PROMOTE_CODE").ok(),
        guest_confirmations: guests,
        now_sec,
        ha_config: HaConfig::default(),
    };

    match promote(&opts, &witness).await {
        Ok(outcome) => {
            if json_out {
                println!("{}", json!({ "ok": true, "promote": outcome }));
            } else {
                println!(
                    "promoted {} to primary at epoch {} (previous {})",
                    outcome.holder, outcome.epoch, outcome.previous_epoch
                );
            }
            Ok(exit::code(exit::OK))
        }
        Err(e) => {
            if json_out {
                println!("{}", json!({ "ok": false, "error": e }));
            } else {
                eprintln!("sommelier promote: {e}");
            }
            Ok(exit::code(exit::REFUSED))
        }
    }
}

fn next_val<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}
