//! Role lease commands (`sommelier role`, sections 5.1, 8.4, 13.1-13.3, and step 7).
//!
//! Subcommands:
//! - `sommelier role list [--data DIR] [--json]`
//! - `sommelier role claim <ROLE> [--who WHO] [--scope full|limited] [--data DIR] [--json]`
//! - `sommelier role release <ROLE> [--who WHO] [--epoch N] [--data DIR] [--json]`
//! - `sommelier role handover <ROLE> --note-file <FILE> [--who WHO] [--epoch N] [--data DIR] [--json]`
//! - `sommelier role check <ROLE> [--epoch N] [--who WHO] [--cmd COMMAND] [--data DIR] [--json]`
//! - `sommelier role veto <WHO> [--data DIR] [--json]`
//! - `sommelier role heartbeat <ROLE> [--who WHO] [--data DIR] [--json]`
//! - `sommelier role tick [<ROLE>] [--now SEC] [--data DIR] [--json]`

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;

use crate::exit;
use crate::role::{LeaseScope, RoleCommand, RoleStore};
use crate::time::now_ms;

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    let Some((sub, rest)) = args.split_first() else {
        eprintln!(
            "usage: sommelier role list | claim R | release R | handover R --note-file F | check R | veto WHO"
        );
        return Ok(exit::code(exit::WRONG_USAGE));
    };

    let parsed = match parse_role_args(rest) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("sommelier: {msg}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };

    let store = RoleStore::new(parsed.data_dir.unwrap_or_else(crate::default_data_dir));
    let mut table = store.load().map_err(|e| e.to_string())?;
    let now_sec = parsed.now_sec.unwrap_or_else(|| now_ms() / 1000);

    match sub.as_str() {
        "list" => {
            let rows = table.list();
            if parsed.json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "roles": rows,
                    })
                );
            } else if rows.is_empty() {
                println!("no roles");
            } else {
                for r in rows {
                    println!(
                        "{} ({}) holder {} scope {} epoch {} state {} heartbeat {}",
                        r.role,
                        r.scope,
                        r.holder.as_deref().unwrap_or("-"),
                        r.lease_scope.as_str(),
                        r.epoch,
                        r.state.as_str(),
                        r.heartbeat_at
                    );
                }
            }
            Ok(exit::code(exit::OK))
        }
        "claim" => {
            let Some(role) = parsed.pos.first() else {
                eprintln!(
                    "sommelier: usage: sommelier role claim <ROLE> [--who WHO] [--scope full|limited]"
                );
                return Ok(exit::code(exit::WRONG_USAGE));
            };
            let who = parsed.who.unwrap_or_else(|| {
                std::env::var("SOMMELIER_WHO").unwrap_or_else(|_| "owner".into())
            });
            match table.claim(role, &who, parsed.scope, now_sec) {
                Ok(row) => {
                    store.save(&table).map_err(|e| e.to_string())?;
                    if parsed.json_out {
                        println!(
                            "{}",
                            json!({
                                "ok": true,
                                "code": exit::OK,
                                "role": row.role,
                                "holder": row.holder,
                                "lease_scope": row.lease_scope.as_str(),
                                "epoch": row.epoch,
                                "state": row.state.as_str(),
                            })
                        );
                    } else {
                        println!(
                            "ok role {} claimed by {} epoch {} scope {}",
                            row.role,
                            row.holder.as_deref().unwrap_or(""),
                            row.epoch,
                            row.lease_scope.as_str()
                        );
                    }
                    Ok(exit::code(exit::OK))
                }
                Err(refusal) => Ok(output_refusal(&refusal.message, parsed.json_out)),
            }
        }
        "release" => {
            let Some(role) = parsed.pos.first() else {
                eprintln!(
                    "sommelier: usage: sommelier role release <ROLE> [--who WHO] [--epoch N]"
                );
                return Ok(exit::code(exit::WRONG_USAGE));
            };
            match table.release(role, parsed.who.as_deref(), parsed.epoch, now_sec) {
                Ok(action) => {
                    store.save(&table).map_err(|e| e.to_string())?;
                    if parsed.json_out {
                        println!(
                            "{}",
                            json!({
                                "ok": true,
                                "code": exit::OK,
                                "action": action,
                                "row": table.get(role),
                            })
                        );
                    } else {
                        println!("ok role {role} released");
                    }
                    Ok(exit::code(exit::OK))
                }
                Err(refusal) => Ok(output_refusal(&refusal.message, parsed.json_out)),
            }
        }
        "handover" => {
            let Some(role) = parsed.pos.first() else {
                eprintln!("sommelier: usage: sommelier role handover <ROLE> --note-file <FILE>");
                return Ok(exit::code(exit::WRONG_USAGE));
            };
            let Some(note_path) = parsed.note_file else {
                eprintln!("sommelier: `role handover` requires --note-file <FILE>");
                return Ok(exit::code(exit::WRONG_USAGE));
            };
            let note = fs::read_to_string(&note_path)
                .map_err(|e| format!("cannot read note file {}: {e}", note_path.display()))?;
            match table.handover(
                role,
                parsed.who.as_deref(),
                parsed.epoch,
                note.trim().to_owned(),
                now_sec,
            ) {
                Ok(action) => {
                    store.save(&table).map_err(|e| e.to_string())?;
                    if parsed.json_out {
                        println!(
                            "{}",
                            json!({
                                "ok": true,
                                "code": exit::OK,
                                "action": action,
                                "row": table.get(role),
                            })
                        );
                    } else {
                        println!("ok role {role} handed over");
                    }
                    Ok(exit::code(exit::OK))
                }
                Err(refusal) => Ok(output_refusal(&refusal.message, parsed.json_out)),
            }
        }
        "check" => {
            let Some(role) = parsed.pos.first() else {
                eprintln!(
                    "sommelier: usage: sommelier role check <ROLE> [--epoch N] [--who WHO] [--cmd COMMAND]"
                );
                return Ok(exit::code(exit::WRONG_USAGE));
            };
            let Some(row) = table.get(role) else {
                return Ok(output_refusal(
                    &format!(
                        "you are no longer {role} (your epoch {}, current 0), stop",
                        parsed.epoch.unwrap_or(0)
                    ),
                    parsed.json_out,
                ));
            };
            let given_epoch = parsed.epoch.unwrap_or(row.epoch);
            match table.check_command(role, parsed.who.as_deref(), given_epoch, parsed.cmd) {
                Ok(valid_row) => {
                    if parsed.json_out {
                        println!(
                            "{}",
                            json!({
                                "ok": true,
                                "code": exit::OK,
                                "role": valid_row.role,
                                "holder": valid_row.holder,
                                "epoch": valid_row.epoch,
                                "lease_scope": valid_row.lease_scope.as_str(),
                            })
                        );
                    } else {
                        println!(
                            "ok role {} held by {} epoch {} scope {}",
                            valid_row.role,
                            valid_row.holder.as_deref().unwrap_or(""),
                            valid_row.epoch,
                            valid_row.lease_scope.as_str()
                        );
                    }
                    Ok(exit::code(exit::OK))
                }
                Err(refusal) => Ok(output_refusal(&refusal.message, parsed.json_out)),
            }
        }
        "veto" => {
            let Some(who) = parsed.pos.first() else {
                eprintln!("sommelier: usage: sommelier role veto <WHO>");
                return Ok(exit::code(exit::WRONG_USAGE));
            };
            let actions = table.veto(who, now_sec);
            store.save(&table).map_err(|e| e.to_string())?;
            if parsed.json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "vetoed": who,
                        "actions": actions,
                    })
                );
            } else {
                println!("ok vetoed {who}");
            }
            Ok(exit::code(exit::OK))
        }
        "heartbeat" => {
            let Some(role) = parsed.pos.first() else {
                eprintln!("sommelier: usage: sommelier role heartbeat <ROLE> --who <WHO>");
                return Ok(exit::code(exit::WRONG_USAGE));
            };
            let who = parsed.who.unwrap_or_else(|| {
                std::env::var("SOMMELIER_WHO").unwrap_or_else(|_| "owner".into())
            });
            let updated = table.heartbeat(role, &who, now_sec);
            store.save(&table).map_err(|e| e.to_string())?;
            if parsed.json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "role": role,
                        "who": who,
                        "holder_renewed": updated,
                    })
                );
            } else {
                println!("ok heartbeat for {role} by {who}");
            }
            Ok(exit::code(exit::OK))
        }
        "tick" => {
            let roles: Vec<String> = if let Some(r) = parsed.pos.first() {
                vec![r.clone()]
            } else {
                table.rows.keys().map(|(r, _)| r.clone()).collect()
            };
            let mut actions = Vec::new();
            for r in roles {
                actions.push(table.role_tick(&r, now_sec));
            }
            store.save(&table).map_err(|e| e.to_string())?;
            if parsed.json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::OK,
                        "actions": actions,
                    })
                );
            } else {
                println!("ok ticked {} roles", actions.len());
            }
            Ok(exit::code(exit::OK))
        }
        other => {
            eprintln!("sommelier: unknown role subcommand `{other}`");
            Ok(exit::code(exit::WRONG_USAGE))
        }
    }
}

fn output_refusal(reason: &str, json_out: bool) -> ExitCode {
    if json_out {
        println!(
            "{}",
            json!({
                "ok": false,
                "code": exit::REFUSED,
                "reason": reason,
            })
        );
    } else {
        eprintln!("sommelier: {reason}");
    }
    exit::code(exit::REFUSED)
}

#[derive(Default)]
struct RoleCliArgs {
    pos: Vec<String>,
    who: Option<String>,
    epoch: Option<u64>,
    scope: Option<LeaseScope>,
    cmd: Option<RoleCommand>,
    note_file: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    now_sec: Option<u64>,
    json_out: bool,
}

fn parse_role_args(args: &[String]) -> Result<RoleCliArgs, String> {
    let mut out = RoleCliArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--json" {
            out.json_out = true;
            continue;
        }
        if let Some(stripped) = arg.strip_prefix("--") {
            let (flag, val) = match stripped.split_once('=') {
                Some((k, v)) => (k, v.to_owned()),
                None => {
                    let v = it
                        .next()
                        .ok_or_else(|| format!("--{stripped} needs a value"))?;
                    (stripped, v.clone())
                }
            };
            match flag {
                "who" => out.who = Some(val),
                "epoch" => {
                    out.epoch = Some(
                        val.parse()
                            .map_err(|_| format!("--epoch needs a number, got `{val}`"))?,
                    );
                }
                "scope" => {
                    out.scope =
                        Some(LeaseScope::parse(&val).ok_or_else(|| {
                            format!("--scope must be full or limited, got `{val}`")
                        })?);
                }
                "cmd" | "command" => {
                    out.cmd = Some(
                        RoleCommand::parse(&val)
                            .ok_or_else(|| format!("unknown role command `{val}`"))?,
                    );
                }
                "note-file" => out.note_file = Some(PathBuf::from(val)),
                "data" => out.data_dir = Some(PathBuf::from(val)),
                "now" => {
                    out.now_sec = Some(
                        val.parse()
                            .map_err(|_| format!("--now needs a number, got `{val}`"))?,
                    );
                }
                "url" | "token" | "token-file" => {}
                other => return Err(format!("unknown flag --{other} for `sommelier role`")),
            }
        } else {
            out.pos.push(arg.clone());
        }
    }
    Ok(out)
}
