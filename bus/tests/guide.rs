//! Integration tests for `sommelier guide`, `sommelier doctor`, `sommelier handover`, and `exit`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;
use sommelier::exit;
use sommelier::guide::doctor::{self, DoctorEnv, HubProbe};
use sommelier::guide::{
    GuideRole, MAX_GUIDE_LINES, MAX_HANDOVER_LINES, PolicyInfo, Shell, Vendor, handover, loops,
    oneshot, templates,
};
use sommelier::hub::{self, HubConfig};
use sommelier::msg::Role;
use sommelier::policy::{DEFAULT_POLICY, Snapshot};
use sommelier::tokens::TokenFile;
use tempfile::TempDir;
use tokio::net::TcpListener;

fn sommelier_bin(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run sommelier")
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("guide")
}

#[test]
fn guides_match_golden_files_and_line_limit() {
    let policy = PolicyInfo::from_toml_str(DEFAULT_POLICY).unwrap();
    let dir = golden_dir();
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    if update {
        fs::create_dir_all(&dir).unwrap();
    }

    for &role in GuideRole::ALL {
        for &vendor in Vendor::ALL {
            let rendered = templates::render(role, vendor, &policy);
            assert!(
                rendered.line_count <= MAX_GUIDE_LINES,
                "{}/{} exceeded {MAX_GUIDE_LINES} lines: {}",
                role.as_str(),
                vendor.as_str(),
                rendered.line_count
            );
            assert_eq!(
                rendered.text.lines().count(),
                rendered.line_count,
                "line_count mismatch"
            );
            let path = dir.join(format!("{}_{}.md", role.as_str(), vendor.as_str()));
            if update {
                fs::write(&path, &rendered.text).unwrap();
            } else {
                let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
                    panic!("missing golden file {}: {e}", path.display());
                });
                let expected_norm = expected.replace("\r\n", "\n");
                assert_eq!(
                    rendered.text,
                    expected_norm,
                    "golden mismatch for {}",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn guide_header_changes_when_policy_hash_changes() {
    let tmp = TempDir::new().unwrap();
    let d1 = tmp.path().join("d1");
    let d2 = tmp.path().join("d2");

    let snap1 = Snapshot::parse(DEFAULT_POLICY).unwrap();
    snap1.write_cache(&d1).unwrap();

    let modified = DEFAULT_POLICY.replace("offer_min = 5", "offer_min = 9");
    let snap2 = Snapshot::parse(&modified).unwrap();
    assert_ne!(snap1.hash, snap2.hash);
    snap2.write_cache(&d2).unwrap();

    let out1 = sommelier_bin(&["guide", "--role", "worker", "--data", d1.to_str().unwrap()]);
    assert!(out1.status.success());
    let s1 = String::from_utf8_lossy(&out1.stdout);
    let h1 = s1.lines().next().unwrap();

    let out2 = sommelier_bin(&["guide", "--role", "worker", "--data", d2.to_str().unwrap()]);
    assert!(out2.status.success());
    let s2 = String::from_utf8_lossy(&out2.stdout);
    let h2 = s2.lines().next().unwrap();

    assert_ne!(h1, h2);
    assert!(h1.contains(&snap1.hash[..8]));
    assert!(h2.contains(&snap2.hash[..8]));
}

#[test]
fn loop_recipes_pass_sh_and_powershell_parse() {
    let tmp = TempDir::new().unwrap();
    for &role in GuideRole::ALL {
        let sh_src = loops::recipe(role, Shell::Sh);
        let sh_file = tmp.path().join(format!("{}.sh", role.as_str()));
        fs::write(&sh_file, sh_src).unwrap();
        let sh_out = Command::new("sh")
            .args(["-n", sh_file.to_str().unwrap()])
            .output()
            .expect("run sh -n");
        assert!(
            sh_out.status.success(),
            "sh -n failed for {}: {}",
            role.as_str(),
            String::from_utf8_lossy(&sh_out.stderr)
        );

        let ps_src = loops::recipe(role, Shell::Ps);
        let ps_file = tmp.path().join(format!("{}.ps1", role.as_str()));
        fs::write(&ps_file, ps_src).unwrap();
        let ps_cmd = if cfg!(windows) { "powershell" } else { "pwsh" };
        if let Ok(ps_out) = Command::new(ps_cmd)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$e=$null; [void][System.Management.Automation.Language.Parser]::ParseInput((Get-Content -Raw $env:PS_FILE), [ref]$null, [ref]$e); if ($e.Count -gt 0) { $e | ForEach-Object { Write-Error $_.Message }; exit 1 }",
            ])
            .env("PS_FILE", &ps_file)
            .output()
        {
            assert!(
                ps_out.status.success(),
                "PowerShell parse failed for {}: {}",
                role.as_str(),
                String::from_utf8_lossy(&ps_out.stderr)
            );
        }
    }
}

#[test]
fn oneshot_and_exit_codes_cover_all_spec_entries() {
    let situations = [
        "start", "progress", "finish", "stuck", "quota", "context", "branch", "run", "ask", "queue",
    ];
    for sit in situations {
        assert!(oneshot::lookup(sit).is_some(), "missing oneshot {sit}");
        let out = sommelier_bin(&["guide", "--oneshot", sit, "--json"]);
        assert!(out.status.success(), "oneshot {sit} failed");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["oneshot"]["situation"], sit);
    }

    let bad = sommelier_bin(&["guide", "--oneshot", "nonexistent"]);
    assert_eq!(bad.status.code(), Some(i32::from(exit::WRONG_USAGE)));

    for c in 0..=7u8 {
        assert!(exit::spec(c).is_some(), "missing exit spec {c}");
    }
}

#[test]
fn handover_page_never_exceeds_80_lines() {
    let policy = PolicyInfo::from_toml_str(DEFAULT_POLICY).unwrap();

    // Rich state with many items to test truncation at 80 lines.
    let roles: Vec<_> = (0..10)
        .map(|i| {
            json!({
                "role": format!("role_{i}"),
                "holder": format!("seat_{i}"),
                "epoch": i + 1,
                "heartbeat_age_secs": 12,
                "last_reason": "drain"
            })
        })
        .collect();
    let alerts: Vec<_> = (0..20)
        .map(|i| {
            json!({
                "id": format!("a{i}"),
                "code": "IDLE",
                "severity": "warn",
                "state": "open",
                "age_secs": 300
            })
        })
        .collect();
    let pools: Vec<_> = (0..10)
        .map(|i| {
            json!({
                "pool": format!("pool_{i}"),
                "used_pct": 50 + i,
                "line": 80,
                "t_line": "2h",
                "state": "open"
            })
        })
        .collect();
    let blockers: Vec<_> = (0..15)
        .map(|i| json!({"id": format!("t{i}"), "holder": "alice", "age_secs": 900}))
        .collect();
    let stuck: Vec<_> = (0..8)
        .map(|i| json!({"id": format!("s{i}"), "reason": "test failure"}))
        .collect();
    let last: Vec<_> = (0..15).map(|i| format!("ack alert a{i}")).collect();
    let note = (0..15)
        .map(|i| format!("note line {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    let state = json!({
        "hub": {"id": 42, "now": "2026-11-25T12:00:00Z", "as_of": 1795608000},
        "policy": {"version": 2, "hash": "ab12cd34ef567890", "mode": "enforce"},
        "roles": roles,
        "alerts": alerts,
        "pools": pools,
        "work": {"counts": {"ready": 4, "running": 2}, "blockers": blockers, "stuck": stuck},
        "waiting": [{"reason": "no headroom"}, {"reason": "no headroom"}],
        "landing": {"queued": 3, "held": 1, "failed": 0},
        "runs": {"lock_holder": "owner", "waiting": 1},
        "members": [{"name": "guest1", "online": true, "hello": {"mode": "own"}}],
        "last": last,
        "note": note,
    });

    let page = handover::build("coordinator", &state, &policy);
    assert!(
        page.line_count <= MAX_HANDOVER_LINES,
        "handover exceeded {MAX_HANDOVER_LINES} lines: {}",
        page.line_count
    );
    assert!(page.text.starts_with("HANDOVER coordinator "));
    assert!(page.text.contains("ROLES"));
    assert!(page.text.contains("ALERTS"));
    assert!(page.text.contains("POOLS"));
    assert!(page.text.contains("WORK"));
    assert!(page.text.contains("LANDING"));
    assert!(page.text.contains("DATA: NOTE"));
}

fn create_fake_tool(dir: &Path, name: &str) {
    let file = if cfg!(windows) {
        dir.join(format!("{name}.cmd"))
    } else {
        dir.join(name)
    };
    fs::write(&file, "@echo off\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&file, perms).unwrap();
    }
}

fn create_hook(dir: &Path, vendor: &str) {
    let file = dir.join(format!("hook-{vendor}.sh"));
    fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&file, perms).unwrap();
    }
}

#[test]
fn doctor_reports_every_failing_item_with_fix_and_passes_when_healthy() {
    let tmp = TempDir::new().unwrap();
    let mut policy = PolicyInfo::from_toml_str(DEFAULT_POLICY).unwrap();
    policy.min_client = "9.9.9".into();
    policy.release_sha256 = Some("deadbeef".repeat(8));

    let failing_env = DoctorEnv {
        data_dir: tmp.path().join("missing-data"),
        policy: policy.clone(),
        binary_version: "0.1.0".into(),
        binary_sha256: Some("0000".repeat(16)),
        token_present: false,
        hub_probe: HubProbe {
            reachable: false,
            rtt_ms: 0,
            token_accepted: false,
            policy_hash: None,
            hub_as_of_secs: None,
            error: Some("connection refused".into()),
        },
        hooks_dir: tmp.path().join("no-hooks"),
        hook_vendors: vec!["claude".into(), "codex".into(), "gemini".into()],
        local_now_secs: 1000,
        free_disk_gb: Some(5),
        path_var: Some(tmp.path().join("empty-bin").to_string_lossy().into_owned()),
        claude_settings_path: tmp.path().join("no-claude.json"),
        codex_sessions_dir: tmp.path().join("no-codex"),
        claude_quota_path: tmp.path().join("no-claude-quota.json"),
    };

    let fail_report = doctor::run_checks(&failing_env);
    assert!(!fail_report.ok);
    assert!(fail_report.claude_snippet.is_some());
    for item in &fail_report.items {
        assert!(!item.ok, "expected {} to fail", item.name);
        assert!(item.fix.is_some(), "expected fix line for {}", item.name);
    }

    // Now construct a healthy fixture environment.
    let healthy_data = tmp.path().join("data");
    let snap = Snapshot::parse(DEFAULT_POLICY).unwrap();
    snap.write_cache(&healthy_data).unwrap();
    let hooks_dir = tmp.path().join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    for v in ["claude", "codex", "gemini"] {
        create_hook(&hooks_dir, v);
    }
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    for t in ["claude", "codex", "agy"] {
        create_fake_tool(&bin_dir, t);
    }
    let codex_sessions = tmp.path().join("codex-sessions");
    fs::create_dir_all(&codex_sessions).unwrap();
    let claude_quota = tmp.path().join("claude-quota.json");
    fs::write(&claude_quota, "{}").unwrap();
    let claude_settings = tmp.path().join("claude-settings.json");
    fs::write(&claude_settings, doctor::CLAUDE_SETTINGS_SNIPPET).unwrap();

    let mut ok_policy = PolicyInfo::from_toml_str(DEFAULT_POLICY).unwrap();
    let sha = "abcd".repeat(16);
    ok_policy.release_sha256 = Some(sha.clone());

    let ok_env = DoctorEnv {
        data_dir: healthy_data,
        policy: ok_policy,
        binary_version: env!("CARGO_PKG_VERSION").into(),
        binary_sha256: Some(sha),
        token_present: true,
        hub_probe: HubProbe {
            reachable: true,
            rtt_ms: 2,
            token_accepted: true,
            policy_hash: Some(snap.hash),
            hub_as_of_secs: Some(1005),
            error: None,
        },
        hooks_dir,
        hook_vendors: vec!["claude".into(), "codex".into(), "gemini".into()],
        local_now_secs: 1000,
        free_disk_gb: Some(100),
        path_var: Some(bin_dir.to_string_lossy().into_owned()),
        claude_settings_path: claude_settings,
        codex_sessions_dir: codex_sessions,
        claude_quota_path: claude_quota,
    };

    let ok_report = doctor::run_checks(&ok_env);
    assert!(ok_report.ok, "{}", doctor::format_text(&ok_report));
    assert!(ok_report.claude_snippet.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handover_and_doctor_cli_against_in_process_hub() {
    let tmp = TempDir::new().unwrap();
    let hub_dir = tmp.path().join("hub");
    let mut t = TokenFile::default();
    let token = t.add("alice", Role::Member).unwrap();
    t.save(&hub_dir.join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(&hub_dir);
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(hub::run(cfg, listener));

    let token_file = tmp.path().join("token.txt");
    fs::write(&token_file, &token).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args([
            "handover",
            "--role",
            "coordinator",
            "--url",
            &url,
            "--token-file",
            token_file.to_str().unwrap(),
            "--data",
            hub_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let page = String::from_utf8_lossy(&out.stdout);
    assert!(page.starts_with("HANDOVER coordinator "), "{page}");
    assert!(page.lines().count() <= MAX_HANDOVER_LINES);

    // Doctor against the live in-process hub with missing hooks/cache exits 3 and never prints token.
    let doc_out = Command::new(env!("CARGO_BIN_EXE_sommelier"))
        .args([
            "doctor",
            "--url",
            &url,
            "--token-file",
            token_file.to_str().unwrap(),
            "--data",
            tmp.path().join("client-data").to_str().unwrap(),
            "--hooks-dir",
            tmp.path().join("no-hooks").to_str().unwrap(),
            "--claude-settings",
            tmp.path().join("no-settings.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(doc_out.status.code(), Some(i32::from(exit::REFUSED)));
    let doc_text = String::from_utf8_lossy(&doc_out.stdout);
    assert!(doc_text.contains("[OK] hub: reachable"), "{doc_text}");
    assert!(
        doc_text.contains("[OK] token: present and accepted"),
        "{doc_text}"
    );
    assert!(doc_text.contains("fix: run: sommelier sync"), "{doc_text}");
    assert!(!doc_text.contains(&token), "doctor must never print token");
}
