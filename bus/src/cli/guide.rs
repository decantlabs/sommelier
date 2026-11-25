//! `sommelier guide` command (step 6).

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;

use crate::exit;
use crate::guide::{self, Format, GuideRole, PolicyInfo, Shell, Vendor};

const HELP: &str = "\
usage:
  sommelier guide [--role worker|lead|coordinator|brain|guest-lead] [--vendor claude|codex|gemini|other] [--format md|json] [--data DIR] [--policy FILE]
  sommelier guide --loop [--role R] [--shell sh|ps]
  sommelier guide --oneshot <situation> [--json]
";

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }

    let mut role = GuideRole::Worker;
    let mut vendor = Vendor::Other;
    let mut format = Format::Md;
    let mut shell: Option<Shell> = None;
    let mut loop_mode = false;
    let mut oneshot: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut policy_path: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--loop" => loop_mode = true,
            "--json" => format = Format::Json,
            "--role" => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier guide: --role needs a value");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                match GuideRole::parse(v) {
                    Ok(r) => role = r,
                    Err(e) => {
                        eprintln!("sommelier guide: {e}");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    }
                }
            }
            "--vendor" => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier guide: --vendor needs a value");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                match Vendor::parse(v) {
                    Ok(v) => vendor = v,
                    Err(e) => {
                        eprintln!("sommelier guide: {e}");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    }
                }
            }
            "--format" => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier guide: --format needs a value");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                match Format::parse(v) {
                    Ok(f) => format = f,
                    Err(e) => {
                        eprintln!("sommelier guide: {e}");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    }
                }
            }
            "--shell" => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier guide: --shell needs a value");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                match Shell::parse(v) {
                    Ok(s) => shell = Some(s),
                    Err(e) => {
                        eprintln!("sommelier guide: {e}");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    }
                }
            }
            "--oneshot" => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier guide: --oneshot needs a situation");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                oneshot = Some(v.clone());
            }
            "--data" => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier guide: --data needs a directory");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                data_dir = Some(PathBuf::from(v));
            }
            "--policy" => {
                let Some(v) = it.next() else {
                    eprintln!("sommelier guide: --policy needs a file");
                    return Ok(exit::code(exit::WRONG_USAGE));
                };
                policy_path = Some(PathBuf::from(v));
            }
            other if other.starts_with("--role=") => {
                match GuideRole::parse(other.trim_start_matches("--role=")) {
                    Ok(r) => role = r,
                    Err(e) => {
                        eprintln!("sommelier guide: {e}");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    }
                }
            }
            other if other.starts_with("--vendor=") => {
                match Vendor::parse(other.trim_start_matches("--vendor=")) {
                    Ok(v) => vendor = v,
                    Err(e) => {
                        eprintln!("sommelier guide: {e}");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    }
                }
            }
            other if other.starts_with("--format=") => {
                match Format::parse(other.trim_start_matches("--format=")) {
                    Ok(f) => format = f,
                    Err(e) => {
                        eprintln!("sommelier guide: {e}");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    }
                }
            }
            other if other.starts_with("--shell=") => {
                match Shell::parse(other.trim_start_matches("--shell=")) {
                    Ok(s) => shell = Some(s),
                    Err(e) => {
                        eprintln!("sommelier guide: {e}");
                        return Ok(exit::code(exit::WRONG_USAGE));
                    }
                }
            }
            other if other.starts_with("--oneshot=") => {
                oneshot = Some(other.trim_start_matches("--oneshot=").to_owned());
            }
            other if other.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(other.trim_start_matches("--data=")));
            }
            other if other.starts_with("--policy=") => {
                policy_path = Some(PathBuf::from(other.trim_start_matches("--policy=")));
            }
            other => {
                eprintln!("sommelier guide: unknown argument `{other}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }

    if let Some(sit) = oneshot {
        let Some(item) = guide::oneshot::lookup(&sit) else {
            eprintln!(
                "sommelier guide: unknown situation `{sit}`, expected one of: start, progress, finish, stuck, quota, context, branch, run, ask, queue"
            );
            return Ok(exit::code(exit::WRONG_USAGE));
        };
        if format == Format::Json {
            println!("{}", json!({ "ok": true, "oneshot": item }));
        } else {
            print!("{}", guide::oneshot::format_text(item));
        }
        return Ok(exit::code(exit::OK));
    }

    if loop_mode {
        let text = guide::loops::render(role, shell);
        if format == Format::Json {
            println!(
                "{}",
                json!({
                    "ok": true,
                    "role": role.as_str(),
                    "shell": shell.map(|s| match s { Shell::Sh => "sh", Shell::Ps => "ps" }),
                    "script": text,
                })
            );
        } else {
            print!("{text}");
        }
        return Ok(exit::code(exit::OK));
    }

    let data = data_dir.unwrap_or_else(crate::default_data_dir);
    let policy = PolicyInfo::load(policy_path.as_deref(), &data)?;
    let rendered = guide::templates::render(role, vendor, &policy);
    if format == Format::Json {
        println!("{}", json!({ "ok": true, "guide": rendered }));
    } else {
        print!("{}", rendered.text);
    }
    Ok(exit::code(exit::OK))
}
