//! `sommelier doctor` self-check command (step 6).

use std::path::PathBuf;
use std::process::ExitCode;

use crate::agent::sandbox;
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
    let sandbox_report = sandbox::detect(&sandbox::SystemProbe);
    let sandbox_ok = sandbox_report.as_ref().is_ok_and(|r| r.fix.is_none());
    let warnings = sandbox_warnings(&env_var, &home, &sandbox::SystemProbe);
    if json_out {
        let mut value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
        value["sandbox"] = sandbox_json(&sandbox_report);
        value["sandbox_warnings"] = serde_json::json!(warnings);
        println!(
            "{}",
            serde_json::to_string(&value).map_err(|e| e.to_string())?
        );
    } else {
        print!("{}", doctor::format_text(&report));
        println!("{}", sandbox_text(&sandbox_report));
        for w in &warnings {
            println!("{w}");
        }
    }

    if report.ok && sandbox_ok {
        Ok(exit::code(exit::OK))
    } else {
        Ok(exit::code(exit::REFUSED))
    }
}

/// Warnings about the sandbox settings of the member. They never change the exit code. A vendor folder that is
/// the one the member's own tools use, a writable cache that the host builds use, and a runtime that was named
/// but does not report rootless mode.
pub fn sandbox_warnings(
    get: &dyn Fn(&str) -> Option<String>,
    home: &std::path::Path,
    probe: &dyn sandbox::SandboxProbe,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut lists = Vec::new();
    for key in [
        "SOMMELIER_SANDBOX_VENDOR_DIRS",
        "SOMMELIER_SANDBOX_CACHE_DIRS",
    ] {
        match sandbox::parse_mounts(&get(key).unwrap_or_default()) {
            Ok(m) => lists.push(m),
            Err(e) => {
                out.push(format!("sandbox: {key}: {e}"));
                lists.push(Vec::new());
            }
        }
    }
    out.extend(sandbox::mount_warnings(&lists[0], &lists[1], home));
    if let Some(rt) = get("SOMMELIER_SANDBOX_RUNTIME")
        && let Some(w) = sandbox::runtime_warning(probe, rt.trim())
    {
        out.push(w);
    }
    out
}

/// The one more doctor line: `sandbox: <level in force> (<what it can offer>)`, with the fix when needed.
pub fn sandbox_text(report: &Result<sandbox::SandboxReport, String>) -> String {
    match report {
        Ok(r) => sandbox::doctor_line(r),
        Err(e) => format!("sandbox: unknown | fix: {e}"),
    }
}

fn sandbox_json(report: &Result<sandbox::SandboxReport, String>) -> serde_json::Value {
    match report {
        Ok(r) => serde_json::json!({
            "in_force": r.in_force.as_str(),
            "requested": r.requested.map(|l| l.as_str()),
            "offers": r.offers.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            "runtime": r.runtime,
            "inside_container": r.inside_container,
            "notes": r.notes,
            "fix": r.fix,
        }),
        Err(e) => serde_json::json!({ "in_force": "unknown", "fix": e }),
    }
}

fn next_val(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_level_gives_one_line_with_fix() {
        let line = sandbox_text(&Err("unknown sandbox level `x`".to_owned()));
        assert!(line.starts_with("sandbox: unknown | fix: "));
        assert!(!line.contains('\n'));
    }
}
