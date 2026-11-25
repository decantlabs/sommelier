//! `sommelier doctor` self check logic (section 8.5).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sysinfo::Disks;

use super::PolicyInfo;

pub const CLAUDE_SETTINGS_SNIPPET: &str = r#"{
  "statusLine": {
    "type": "command",
    "command": "sommelier quota ingest claude-statusline",
    "refreshInterval": 60
  },
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|compact",
        "hooks": [
          {
            "type": "command",
            "command": "sommelier guide --role coordinator --vendor claude"
          }
        ]
      }
    ]
  }
}"#;

#[derive(Clone, Debug, Serialize)]
pub struct CheckItem {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub items: Vec<CheckItem>,
    pub claude_snippet: Option<String>,
}

#[derive(Clone, Debug)]
pub struct HubProbe {
    pub reachable: bool,
    pub rtt_ms: u64,
    pub token_accepted: bool,
    pub policy_hash: Option<String>,
    pub hub_as_of_secs: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DoctorEnv {
    pub data_dir: PathBuf,
    pub policy: PolicyInfo,
    pub binary_version: String,
    pub binary_sha256: Option<String>,
    pub token_present: bool,
    pub hub_probe: HubProbe,
    pub hooks_dir: PathBuf,
    pub hook_vendors: Vec<String>,
    pub local_now_secs: u64,
    pub free_disk_gb: Option<u64>,
    pub path_var: Option<String>,
    pub claude_settings_path: PathBuf,
    pub codex_sessions_dir: PathBuf,
    pub claude_quota_path: PathBuf,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn exe_sha256() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let bytes = fs::read(exe).ok()?;
    Some(format!("{:x}", Sha256::digest(&bytes)))
}

pub async fn probe_hub(url: &str, token: Option<&str>) -> HubProbe {
    let Ok(mut base) = reqwest::Url::parse(url.trim()) else {
        return HubProbe {
            reachable: false,
            rtt_ms: 0,
            token_accepted: false,
            policy_hash: None,
            hub_as_of_secs: None,
            error: Some("invalid hub URL".into()),
        };
    };
    let scheme = match base.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => {
            return HubProbe {
                reachable: false,
                rtt_ms: 0,
                token_accepted: false,
                policy_hash: None,
                hub_as_of_secs: None,
                error: Some("invalid hub URL scheme".into()),
            };
        }
    };
    let _ = base.set_scheme(scheme);
    base.set_query(None);
    base.set_fragment(None);

    crate::client::init();
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
    else {
        return HubProbe {
            reachable: false,
            rtt_ms: 0,
            token_accepted: false,
            policy_hash: None,
            hub_as_of_secs: None,
            error: Some("cannot build HTTP client".into()),
        };
    };

    let Some(tok) = token.filter(|t| !t.trim().is_empty()) else {
        return HubProbe {
            reachable: false,
            rtt_ms: 0,
            token_accepted: false,
            policy_hash: None,
            hub_as_of_secs: None,
            error: Some("no token available to query hub".into()),
        };
    };

    let mut pol_url = base.clone();
    pol_url.set_path("/api/policy");
    let started = Instant::now();
    let resp = match client.get(pol_url).bearer_auth(tok.trim()).send().await {
        Ok(r) => r,
        Err(e) => {
            return HubProbe {
                reachable: false,
                rtt_ms: 0,
                token_accepted: false,
                policy_hash: None,
                hub_as_of_secs: None,
                error: Some(e.without_url().to_string()),
            };
        }
    };
    let rtt_ms = started.elapsed().as_millis() as u64;
    if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
        return HubProbe {
            reachable: true,
            rtt_ms,
            token_accepted: false,
            policy_hash: None,
            hub_as_of_secs: None,
            error: Some(format!("HTTP {}", resp.status())),
        };
    }
    if !resp.status().is_success() {
        return HubProbe {
            reachable: true,
            rtt_ms,
            token_accepted: false,
            policy_hash: None,
            hub_as_of_secs: None,
            error: Some(format!("HTTP {}", resp.status())),
        };
    }
    let policy_hash = match resp.bytes().await {
        Ok(b) => serde_json::from_slice::<Value>(&b)
            .ok()
            .and_then(|v| v["hash"].as_str().map(str::to_owned)),
        Err(_) => None,
    };

    let mut st_url = base;
    st_url.set_path("/api/state");
    let hub_as_of_secs = match client.get(st_url).bearer_auth(tok.trim()).send().await {
        Ok(r) if r.status().is_success() => r
            .bytes()
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["as_of"].as_u64().or_else(|| v["hub"]["as_of"].as_u64())),
        _ => None,
    };

    HubProbe {
        reachable: true,
        rtt_ms,
        token_accepted: true,
        policy_hash,
        hub_as_of_secs,
        error: None,
    }
}

pub fn run_checks(env: &DoctorEnv) -> DoctorReport {
    let mut items = vec![
        // 1. Binary version vs min_client
        check_version(&env.binary_version, &env.policy.min_client),
        // 2. Binary sha256 vs release.sha256
        check_sha256(
            env.binary_sha256.as_deref(),
            env.policy.release_sha256.as_deref(),
        ),
        // 3. Token present and accepted (never print token)
        check_token(env.token_present, &env.hub_probe),
        // 4. Hub reachable + RTT
        check_hub(&env.hub_probe),
        // 5. Policy cache hash equals hub's
        check_policy_cache(&env.data_dir, &env.hub_probe),
        // 6. Launcher hooks present and executable
        check_hooks(&env.hooks_dir, &env.hook_vendors),
        // 7. Clock offset against hub (> 30 s warns/fails)
        check_clock(env.local_now_secs, &env.hub_probe),
        // 8. Free disk vs disk.min_free_gb
        check_disk(&env.data_dir, env.free_disk_gb, env.policy.disk_min_free_gb),
        // 9. Vendor tools on PATH
        check_vendor_tools(env.path_var.as_deref()),
        // 10. Quota sources on this machine
        check_quota_sources(
            &env.codex_sessions_dir,
            &env.claude_quota_path,
            env.path_var.as_deref(),
        ),
    ];

    // 11. Claude Code settings (statusLine + SessionStart hook)
    let claude_item = check_claude_settings(&env.claude_settings_path);
    let claude_snippet = if claude_item.ok {
        None
    } else {
        Some(CLAUDE_SETTINGS_SNIPPET.to_owned())
    };
    items.push(claude_item);

    let ok = items.iter().all(|i| i.ok);
    DoctorReport {
        ok,
        items,
        claude_snippet,
    }
}

fn check_version(bin: &str, min_client: &str) -> CheckItem {
    let ok = match (Version::parse(bin), Version::parse(min_client)) {
        (Ok(b), Ok(m)) => b >= m,
        _ => false,
    };
    CheckItem {
        name: "binary_version".into(),
        ok,
        detail: format!("binary {bin}, min_client {min_client}"),
        fix: (!ok).then(|| format!("update sommelier to at least {min_client}")),
    }
}

fn check_sha256(actual: Option<&str>, expected: Option<&str>) -> CheckItem {
    let Some(exp) = expected else {
        return CheckItem {
            name: "binary_sha256".into(),
            ok: true,
            detail: "release.sha256 not set in policy".into(),
            fix: None,
        };
    };
    let ok = actual.is_some_and(|a| a.eq_ignore_ascii_case(exp));
    CheckItem {
        name: "binary_sha256".into(),
        ok,
        detail: if ok {
            format!("matches {}", &exp[..8.min(exp.len())])
        } else {
            format!("mismatch against {}", &exp[..8.min(exp.len())])
        },
        fix: (!ok).then(|| "install the sommelier binary matching policy release.sha256".into()),
    }
}

fn check_token(present: bool, probe: &HubProbe) -> CheckItem {
    if !present {
        return CheckItem {
            name: "token".into(),
            ok: false,
            detail: "no token configured".into(),
            fix: Some("set SOMMELIER_TOKEN_FILE or pass --token-file".into()),
        };
    }
    if probe.reachable && !probe.token_accepted {
        return CheckItem {
            name: "token".into(),
            ok: false,
            detail: "token rejected by hub".into(),
            fix: Some("ask the hub owner for a valid token with sommelier token add".into()),
        };
    }
    CheckItem {
        name: "token".into(),
        ok: true,
        detail: "present and accepted".into(),
        fix: None,
    }
}

fn check_hub(probe: &HubProbe) -> CheckItem {
    if probe.reachable {
        CheckItem {
            name: "hub".into(),
            ok: true,
            detail: format!("reachable, rtt {} ms", probe.rtt_ms),
            fix: None,
        }
    } else {
        let reason = probe.error.as_deref().unwrap_or("unreachable");
        CheckItem {
            name: "hub".into(),
            ok: false,
            detail: format!("not reachable: {reason}"),
            fix: Some("check --url or SOMMELIER_URL and ensure sommelier serve is running".into()),
        }
    }
}

fn check_policy_cache(data_dir: &Path, probe: &HubProbe) -> CheckItem {
    let cache = data_dir.join("policy").join("swarm.policy.json");
    let cached = match fs::read_to_string(&cache) {
        Ok(s) => PolicyInfo::from_json_str(&s).ok().map(|p| p.hash),
        Err(_) => None,
    };
    match (cached, probe.policy_hash.as_deref()) {
        (None, _) => CheckItem {
            name: "policy_cache".into(),
            ok: false,
            detail: "no cached policy".into(),
            fix: Some("run: sommelier sync".into()),
        },
        (Some(c), Some(h)) if c != h => CheckItem {
            name: "policy_cache".into(),
            ok: false,
            detail: format!(
                "cached {} differs from hub {}",
                &c[..8.min(c.len())],
                &h[..8.min(h.len())]
            ),
            fix: Some("run: sommelier sync".into()),
        },
        (Some(c), _) => CheckItem {
            name: "policy_cache".into(),
            ok: true,
            detail: format!("hash {}", &c[..8.min(c.len())]),
            fix: None,
        },
    }
}

fn check_hooks(hooks_dir: &Path, vendors: &[String]) -> CheckItem {
    let mut missing = Vec::new();
    for v in vendors {
        let p = hooks_dir.join(format!("hook-{v}.sh"));
        if !is_executable_file(&p) {
            missing.push(format!("hook-{v}.sh"));
        }
    }
    if missing.is_empty() {
        CheckItem {
            name: "launcher_hooks".into(),
            ok: true,
            detail: format!("{} hooks ready", vendors.len()),
            fix: None,
        }
    } else {
        CheckItem {
            name: "launcher_hooks".into(),
            ok: false,
            detail: format!("missing or not executable: {}", missing.join(", ")),
            fix: Some(format!(
                "add executable {} in {}",
                missing.join(", "),
                hooks_dir.display()
            )),
        }
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(md) = fs::metadata(path) else {
        return false;
    };
    if !md.is_file() || md.len() == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn check_clock(local_secs: u64, probe: &HubProbe) -> CheckItem {
    let Some(hub_secs) = probe.hub_as_of_secs else {
        return CheckItem {
            name: "clock".into(),
            ok: probe.reachable,
            detail: if probe.reachable {
                "hub time unavailable".into()
            } else {
                "skipped while hub unreachable".into()
            },
            fix: (!probe.reachable).then(|| "reach hub to check clock offset".into()),
        };
    };
    let diff = local_secs.abs_diff(hub_secs);
    let ok = diff <= 30;
    CheckItem {
        name: "clock".into(),
        ok,
        detail: format!("offset {diff} s"),
        fix: (!ok).then(|| "sync system clock with NTP (offset exceeds 30 s)".into()),
    }
}

fn check_disk(data_dir: &Path, override_gb: Option<u64>, min_free_gb: u64) -> CheckItem {
    let free_gb = override_gb.unwrap_or_else(|| measure_free_gb(data_dir));
    let ok = free_gb >= min_free_gb;
    CheckItem {
        name: "disk".into(),
        ok,
        detail: format!("{free_gb} GB free (min {min_free_gb} GB)"),
        fix: (!ok).then(|| format!("free disk space to at least {min_free_gb} GB")),
    }
}

fn measure_free_gb(target: &Path) -> u64 {
    let probe = if target.exists() {
        target.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    let canon = probe.canonicalize().unwrap_or(probe);
    let Ok(disks) = Disks::new_with_refreshed_list() else {
        return 0;
    };
    let best = disks
        .iter()
        .filter(|d| canon.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .or_else(|| disks.first());
    best.map_or(0, |d| d.available_space() / (1024 * 1024 * 1024))
}

fn find_on_path(bin: &str, path_var: Option<&str>) -> bool {
    let Some(raw) = path_var else {
        return false;
    };
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(raw) {
        for ext in exts {
            let p = dir.join(format!("{bin}{ext}"));
            if p.is_file() {
                return true;
            }
        }
    }
    false
}

fn check_vendor_tools(path_var: Option<&str>) -> CheckItem {
    let tools = ["claude", "codex", "agy"];
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for t in tools {
        if find_on_path(t, path_var) {
            found.push(t);
        } else {
            missing.push(t);
        }
    }
    let ok = missing.is_empty();
    CheckItem {
        name: "vendor_tools".into(),
        ok,
        detail: if ok {
            format!("on PATH: {}", found.join(", "))
        } else {
            format!(
                "found [{}], missing [{}]",
                found.join(", "),
                missing.join(", ")
            )
        },
        fix: (!ok).then(|| format!("install missing tools on PATH: {}", missing.join(", "))),
    }
}

fn check_quota_sources(
    codex_sessions_dir: &Path,
    claude_quota_path: &Path,
    path_var: Option<&str>,
) -> CheckItem {
    let mut sources = Vec::new();
    if codex_sessions_dir.is_dir() || find_on_path("codex", path_var) {
        sources.push("codex");
    }
    if claude_quota_path.is_file() {
        sources.push("claude");
    }
    if find_on_path("agy", path_var) {
        sources.push("gemini");
    }
    let ok = !sources.is_empty();
    CheckItem {
        name: "quota_sources".into(),
        ok,
        detail: if ok {
            format!("available: {}", sources.join(", "))
        } else {
            "no local quota source found".into()
        },
        fix: (!ok).then(|| "enable Claude statusLine, Codex sessions/CLI, or agy on PATH".into()),
    }
}

fn check_claude_settings(path: &Path) -> CheckItem {
    let Ok(text) = fs::read_to_string(path) else {
        return CheckItem {
            name: "claude_settings".into(),
            ok: false,
            detail: "Claude settings file not found".into(),
            fix: Some(format!(
                "add statusLine and SessionStart hook to {}",
                path.display()
            )),
        };
    };
    let Ok(val) = serde_json::from_str::<Value>(&text) else {
        return CheckItem {
            name: "claude_settings".into(),
            ok: false,
            detail: "Claude settings file is not valid JSON".into(),
            fix: Some(format!("fix JSON in {}", path.display())),
        };
    };
    let has_status = val["statusLine"]["command"]
        .as_str()
        .is_some_and(|c| c.contains("sommelier quota ingest claude-statusline"));
    let has_hook = val["hooks"]["SessionStart"].as_array().is_some_and(|arr| {
        arr.iter().any(|entry| {
            entry["hooks"].as_array().is_some_and(|hs| {
                hs.iter().any(|h| {
                    h["command"]
                        .as_str()
                        .is_some_and(|c| c.contains("sommelier guide"))
                })
            }) || entry["command"]
                .as_str()
                .is_some_and(|c| c.contains("sommelier guide"))
        })
    });
    let ok = has_status && has_hook;
    let detail = match (has_status, has_hook) {
        (true, true) => "statusLine and SessionStart hook configured".into(),
        (false, true) => "statusLine missing".into(),
        (true, false) => "SessionStart hook missing".into(),
        (false, false) => "statusLine and SessionStart hook missing".into(),
    };
    CheckItem {
        name: "claude_settings".into(),
        ok,
        detail,
        fix: (!ok).then(|| "add statusLine and SessionStart hook (snippet printed below)".into()),
    }
}

pub fn format_text(report: &DoctorReport) -> String {
    let mut out = String::new();
    for item in &report.items {
        let tag = if item.ok { "OK" } else { "FAIL" };
        if let Some(fix) = &item.fix {
            out.push_str(&format!(
                "[{tag}] {}: {} | fix: {fix}\n",
                item.name, item.detail
            ));
        } else {
            out.push_str(&format!("[{tag}] {}: {}\n", item.name, item.detail));
        }
    }
    if let Some(snippet) = &report.claude_snippet {
        out.push_str("\nClaude Code settings snippet (do not auto-edit):\n");
        out.push_str(snippet);
        out.push('\n');
    }
    out
}
