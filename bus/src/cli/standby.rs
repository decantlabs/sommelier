//! Standby hub follower command (`sommelier standby`, section 13.5, step 12b).

use std::path::PathBuf;
use std::process::ExitCode;

use crate::exit;
use crate::ha::{StandbyConfig, run_standby};

const HELP: &str = "\
usage: sommelier standby --of <URL> [--listen ADDR] [--data DIR] [--token TOKEN | --token-file PATH] [--epoch N] [--bundle-dir DIR] [--quiet]
Follows the primary hub's log byte-for-byte, keeps a copy of state.db, and answers HTTP 503 until promoted.
";

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }

    let mut of_url: Option<String> = None;
    let mut listen = String::from("127.0.0.1:7301");
    let mut data_dir: Option<PathBuf> = None;
    let mut token: Option<String> = None;
    let mut token_file: Option<String> = None;
    let mut epoch: u64 = 1;
    let mut bundle_dir: Option<PathBuf> = None;
    let mut quiet = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--of" => of_url = Some(next_val(&mut it, "--of")?),
            "--listen" => listen = next_val(&mut it, "--listen")?,
            "--data" => data_dir = Some(PathBuf::from(next_val(&mut it, "--data")?)),
            "--token" => token = Some(next_val(&mut it, "--token")?),
            "--token-file" => token_file = Some(next_val(&mut it, "--token-file")?),
            "--epoch" => {
                let raw = next_val(&mut it, "--epoch")?;
                epoch = raw
                    .parse()
                    .map_err(|_| format!("--epoch needs a number, got `{raw}`"))?;
            }
            "--bundle-dir" => bundle_dir = Some(PathBuf::from(next_val(&mut it, "--bundle-dir")?)),
            "--quiet" => quiet = true,
            other => {
                eprintln!("sommelier standby: unknown argument `{other}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }

    let Some(of_url) = of_url else {
        eprintln!("sommelier standby: --of <URL> is required");
        return Ok(exit::code(exit::WRONG_USAGE));
    };

    let env_var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let resolved_token = match (token.or_else(|| env_var("SOMMELIER_TOKEN")), token_file) {
        (Some(t), _) => t,
        (None, file) => {
            let path = file
                .or_else(|| env_var("SOMMELIER_TOKEN_FILE"))
                .ok_or("no token: pass --token, --token-file, or set SOMMELIER_TOKEN")?;
            std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read token file {path}: {e}"))?
                .trim()
                .to_owned()
        }
    };

    let mut cfg = StandbyConfig::new(
        of_url,
        resolved_token,
        data_dir.unwrap_or_else(crate::default_data_dir),
    );
    cfg.listen_addr = listen;
    cfg.initial_epoch = epoch;
    cfg.bundle_source_dir = bundle_dir;
    cfg.verbose = !quiet;

    let handle = run_standby(cfg, None).await?;
    if !quiet {
        eprintln!(
            "sommelier standby listening on {} (epoch {}, following)",
            handle.addr,
            handle.epoch()
        );
    }
    handle.join().await?;
    Ok(exit::code(exit::OK))
}

fn next_val<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}
