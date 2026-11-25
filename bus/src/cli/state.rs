//! Store state commands (`snapshot`, `replay`, step 4, section 5.1 and 8.4).

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;

use crate::exit;
use crate::store::Store;
use crate::time::now_ms;

pub async fn run(cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    match cmd {
        "snapshot" => cmd_snapshot(args),
        "replay" => cmd_replay(args),
        _ => super::unimplemented(cmd),
    }
}

fn cmd_snapshot(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut data_dir: Option<PathBuf> = None;
    let mut out_path: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => {
                let val = it.next().ok_or_else(|| "--data needs a value".to_owned())?;
                data_dir = Some(PathBuf::from(val));
            }
            "--out" | "--file" => {
                let val = it.next().ok_or_else(|| format!("{arg} needs a value"))?;
                out_path = Some(PathBuf::from(val));
            }
            other if other.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(other.trim_start_matches("--data=")));
            }
            other if other.starts_with("--out=") => {
                out_path = Some(PathBuf::from(other.trim_start_matches("--out=")));
            }
            other if other.starts_with("--file=") => {
                out_path = Some(PathBuf::from(other.trim_start_matches("--file=")));
            }
            other => {
                return Err(format!(
                    "unknown argument `{other}` for `sommelier snapshot`"
                ));
            }
        }
    }

    let dir = data_dir.unwrap_or_else(crate::default_data_dir);
    let log_path = dir.join("bus.jsonl");
    let mut store = Store::open(&dir)?;
    if log_path.exists() {
        store.catch_up_from_log(&log_path)?;
    }
    let now_sec = now_ms() / 1000;
    let (report, _payload) = store.write_snapshot(&dir, out_path.as_deref(), now_sec)?;

    if json_out {
        let out = json!({
            "ok": true,
            "code": exit::OK,
            "snapshot_id": report.snapshot_id,
            "cursor": report.cursor,
            "schema_version": report.schema_version,
            "path": report.path,
            "state_hash": report.state_hash,
            "table_hashes": report.table_hashes,
        });
        println!("{out}");
    } else {
        println!(
            "ok snapshot {} cursor #{} hash {} file {}",
            report.snapshot_id,
            report.cursor,
            report.state_hash.get(..12).unwrap_or(&report.state_hash),
            report.path
        );
    }
    Ok(exit::code(exit::OK))
}

fn cmd_replay(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut verify = false;
    let mut data_dir: Option<PathBuf> = None;
    let mut log_override: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--verify" => verify = true,
            "--data" => {
                let val = it.next().ok_or_else(|| "--data needs a value".to_owned())?;
                data_dir = Some(PathBuf::from(val));
            }
            "--log" | "--file" => {
                let val = it.next().ok_or_else(|| format!("{arg} needs a value"))?;
                log_override = Some(PathBuf::from(val));
            }
            other if other.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(other.trim_start_matches("--data=")));
            }
            other if other.starts_with("--log=") => {
                log_override = Some(PathBuf::from(other.trim_start_matches("--log=")));
            }
            other if other.starts_with("--file=") => {
                log_override = Some(PathBuf::from(other.trim_start_matches("--file=")));
            }
            other if !other.starts_with('-') && log_override.is_none() => {
                log_override = Some(PathBuf::from(other));
            }
            other => {
                return Err(format!("unknown argument `{other}` for `sommelier replay`"));
            }
        }
    }

    let dir = match (&data_dir, &log_override) {
        (Some(d), _) => d.clone(),
        (None, Some(l)) => l
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(crate::default_data_dir),
        (None, None) => crate::default_data_dir(),
    };
    let log_path = log_override.unwrap_or_else(|| dir.join("bus.jsonl"));
    let mut store = Store::open(&dir)?;

    let report = if verify {
        store.replay_verify(&log_path)?
    } else {
        store.rebuild_from_log(&log_path)?
    };

    if json_out {
        let out = json!({
            "ok": true,
            "code": exit::OK,
            "verified": report.verified,
            "messages_applied": report.messages_applied,
            "cursor": report.cursor,
            "state_hash": report.state_hash,
            "table_hashes": report.table_hashes,
        });
        println!("{out}");
    } else if report.verified {
        println!(
            "ok replay verified {} messages cursor #{} hash {}",
            report.messages_applied,
            report.cursor,
            report.state_hash.get(..12).unwrap_or(&report.state_hash)
        );
    } else {
        println!(
            "ok replayed {} messages cursor #{} hash {}",
            report.messages_applied,
            report.cursor,
            report.state_hash.get(..12).unwrap_or(&report.state_hash)
        );
    }
    Ok(exit::code(exit::OK))
}
