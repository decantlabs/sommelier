//! `sommelier doctor` self-check command (step 6).

use std::path::PathBuf;
use std::process::ExitCode;

use crate::exit;
use crate::guide::PolicyInfo;
use crate::guide::doctor::{self, DoctorEnv};

const HELP: &str = "\
usage: sommelier doctor [--url URL] [--token TOKEN | --token-file PATH] [--data DIR] [--policy FILE] [--hooks-dir DIR] [--claude-settings PATH] [--json]
Checks binary version, sha256, token, hub reachability, policy cache, launcher hooks, clock offset, disk space, vendor tools, quota sources, and Claude settings.
Exit 0 when all pass, 3 otherwise.
";

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }

    let mut json_out = false;
    let mut url: Option<String> = None;
    let mut token: Option<String> = None;
    let mut token_file: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut policy_path: Option<PathBuf> = None;
    let mut hooks_dir: Option<PathBuf> = None;
    let mut claude_settings: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--url" => url = Some(next_val(&mut it, "--url")?),
            "--token" => token = Some(next_val(&mut it, "--token")?),
            "--token-file" => token_file = Some(next_val(&mut it, "--token-file")?),
            "--data" => data_dir = Some(PathBuf::from(next_val(&mut it, "--data")?)),
            "--policy" => policy_path = Some(PathBuf::from(next_val(&mut it, "--policy")?)),
            "--hooks-dir" => hooks_dir = Some(PathBuf::from(next_val(&mut it, "--hooks-dir")?)),
            "--claude-settings" => {
                claude_settings = Some(PathBuf::from(next_val(&mut it, "--claude-settings")?))
            }
            other if other.starts_with("--url=") => {
                url = Some(other.trim_start_matches("--url=").to_owned())
            }
            other if other.starts_with("--token=") => {
                token = Some(other.trim_start_matches("--token=").to_owned())
            }
            other if other.starts_with("--token-file=") => {
                token_file = Some(other.trim_start_matches("--token-file=").to_owned())
            }
            other if other.starts_with("--data=") => {
                data_dir = Some(PathBuf::from(other.trim_start_matches("--data=")))
            }
            other if other.starts_with("--policy=") => {
                policy_path = Some(PathBuf::from(other.trim_start_matches("--policy=")))
            }
            other if other.starts_with("--hooks-dir=") => {
                hooks_dir = Some(PathBuf::from(other.trim_start_matches("--hooks-dir=")))
            }
            other if other.starts_with("--claude-settings=") => {
                claude_settings = Some(PathBuf::from(
                    other.trim_start_matches("--claude-settings="),
                ))
            }
            other => {
                eprintln!("sommelier doctor: unknown argument `{other}`");
                return Ok(exit::code(exit::WRONG_USAGE));
            }
        }
    }

    let env_var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let data = data_dir.unwrap_or_else(crate::default_data_dir);
    let policy = PolicyInfo::load(policy_path.as_deref(), &data)?;

    let resolved_url = url
        .or_else(|| env_var("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));

    let resolved_token = token.or_else(|| env_var("SOMMELIER_TOKEN")).or_else(|| {
        let p = token_file.or_else(|| env_var("SOMMELIER_TOKEN_FILE"))?;
        std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
    });

    let token_present = resolved_token.as_ref().is_some_and(|t| !t.is_empty());
    let hub_probe = doctor::probe_hub(&resolved_url, resolved_token.as_deref()).await;

    let home = env_var("HOME")
        .or_else(|| env_var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let resolved_hooks = hooks_dir
        .or_else(|| env_var("SOMMELIER_HOOKS_DIR").map(PathBuf::from))
        .unwrap_or_else(|| data.join("hooks"));
    let resolved_claude = claude_settings
        .or_else(|| env_var("SOMMELIER_CLAUDE_SETTINGS").map(PathBuf::from))
        .unwrap_or_else(|| home.join(".claude").join("settings.json"));
    let codex_sessions = env_var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
        .join("sessions");

    let doctor_env = DoctorEnv {
        data_dir: data.clone(),
        policy,
        binary_version: env!("CARGO_PKG_VERSION").to_owned(),
        binary_sha256: doctor::exe_sha256(),
        token_present,
        hub_probe,
        hooks_dir: resolved_hooks,
        hook_vendors: vec!["claude".into(), "codex".into(), "gemini".into()],
        local_now_secs: doctor::now_secs(),
        free_disk_gb: None,
        path_var: env_var("PATH"),
        claude_settings_path: resolved_claude,
        codex_sessions_dir: codex_sessions,
        claude_quota_path: data.join("quota").join("claude.json"),
    };

    let report = doctor::run_checks(&doctor_env);
    if json_out {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|e| e.to_string())?
        );
    } else {
        print!("{}", doctor::format_text(&report));
    }

    if report.ok {
        Ok(exit::code(exit::OK))
    } else {
        Ok(exit::code(exit::REFUSED))
    }
}

fn next_val(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}
