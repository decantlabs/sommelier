//! High availability failure drill command (`sommelier drill`, section 13.7, step 12b).

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;

use crate::exit;
use crate::ha::run_drill;

const HELP: &str = "\
usage: sommelier drill <coordinator-dies|hub-dies|quota-drain|flapping> [--data DIR] [--json]
Runs an isolated failure drill on temporary ports and folders without touching the live hub.
";

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }

    let mut name: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut json_out = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => {
                let v = it.next().ok_or("--data requires a value")?;
                data_dir = Some(PathBuf::from(v));
            }
            other if other.starts_with('-') => {
                eprintln!("sommelier drill: unknown flag `{other}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
            pos => {
                if name.is_none() {
                    name = Some(pos.to_owned());
                } else {
                    eprintln!("sommelier drill: unexpected argument `{pos}`");
                    return Ok(exit::code(exit::WRONG_USAGE));
                }
            }
        }
    }

    let Some(name) = name else {
        eprintln!(
            "sommelier drill: drill name required (coordinator-dies, hub-dies, quota-drain, flapping)"
        );
        return Ok(exit::code(exit::WRONG_USAGE));
    };

    match run_drill(&name, data_dir.as_deref()).await {
        Ok(res) => {
            if json_out {
                println!("{}", json!({ "ok": true, "result": res }));
            } else {
                println!(
                    "PASS drill `{}` recover={}s (target <= {}s): {}",
                    res.drill, res.recover_sec, res.target_recover_sec, res.details
                );
            }
            Ok(exit::code(exit::OK))
        }
        Err(e) => {
            if json_out {
                println!("{}", json!({ "ok": false, "error": e }));
            } else {
                eprintln!("FAIL drill `{name}`: {e}");
            }
            Ok(exit::code(exit::INTERNAL_ERROR))
        }
    }
}
