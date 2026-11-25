//! The `sommelier` binary itself, run as a child process against an in process hub.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sommelier::hub::{self, HubConfig};
use sommelier::msg::Role;
use sommelier::tokens::TokenFile;
use tokio::net::TcpListener;

struct Bus {
    url: String,
    dir: std::path::PathBuf,
    token: String,
}

async fn start(tag: &str) -> Bus {
    let dir = std::env::temp_dir().join(format!("sommelier-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    start_in(dir).await
}

async fn start_in(dir: std::path::PathBuf) -> Bus {
    let mut t = TokenFile::default();
    let token = t.add("alice", Role::Member).unwrap();
    t.save(&dir.join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(&dir);
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(hub::run(cfg, listener));
    Bus { url, dir, token }
}

fn sommelier(bus: &Bus, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sommelier"));
    c.args(args)
        .env("SOMMELIER_URL", &bus.url)
        .env("SOMMELIER_TOKEN", &bus.token)
        .env("SOMMELIER_DATA", &bus.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

/// Waits for the child without blocking the runtime, fails after `secs`.
async fn finish(mut child: std::process::Child, secs: u64) -> (bool, String, String) {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let out = child.wait_with_output().unwrap();
            return (
                status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            );
        }
        if start.elapsed() > Duration::from_secs(secs) {
            let _ = child.kill();
            panic!("sommelier did not finish within {secs} s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn send_never_reads_an_open_stdin_unasked() {
    let bus = start("stdin").await;
    // A stdin that stays open and never delivers anything, like some agent harnesses.
    let child = sommelier(&bus, &["send", "READY"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let (ok, _, err) = finish(child, 10).await;
    assert!(!ok);
    assert!(err.contains("no body for READY"), "{err}");

    // CLAIM without a body is fine and does not touch stdin either.
    let child = sommelier(&bus, &["send", "CLAIM", "--topic", "issue/1"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let (ok, out, err) = finish(child, 10).await;
    assert!(ok, "{err}");
    assert_eq!(out.trim(), "#1");

    // `-` reads stdin, lines joined.
    let mut child = sommelier(&bus, &["send", "RUN_RESULT", "-", "--topic", "run/1"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"\xEF\xBB\xBFpass 12\n\nfail 0\n")
        .unwrap();
    let (ok, out, err) = finish(child, 10).await;
    assert!(ok, "{err}");
    assert_eq!(out.trim(), "#2");

    let child = sommelier(&bus, &["log", "--last", "1"]).spawn().unwrap();
    let (ok, out, _) = finish(child, 10).await;
    assert!(ok);
    assert!(
        out.trim_end()
            .ends_with("alice RUN_RESULT run/1 : pass 12 | fail 0"),
        "{out}"
    );
}

#[tokio::test]
async fn tail_once_exits() {
    let bus = start("once").await;
    for body in ["a", "b", "c"] {
        let child = sommelier(&bus, &["send", "CHAT", body]).spawn().unwrap();
        assert!(finish(child, 10).await.0);
    }
    let child = sommelier(&bus, &["tail", "--last", "2", "--once"])
        .spawn()
        .unwrap();
    let (ok, out, err) = finish(child, 10).await;
    assert!(ok, "{err}");
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(
        lines[0].ends_with("CHAT - : b") && lines[1].ends_with("CHAT - : c"),
        "{out}"
    );
}

#[tokio::test]
async fn token_commands_use_the_shared_data_folder() {
    let dir = std::env::temp_dir().join(format!("sommelier-cli-tok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_sommelier"))
            .args(args)
            .env("SOMMELIER_DATA", &dir)
            .output()
            .unwrap()
    };
    let out = run(&["token", "add", "Alice"]);
    assert!(out.status.success());
    let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert!(token.starts_with("sb_"), "{token}");
    let file = std::fs::read_to_string(dir.join("tokens.json")).unwrap();
    assert!(
        file.contains("\"alice\"") && !file.contains(&token),
        "{file}"
    );

    let out = run(&["token", "add", "alice"]);
    assert!(!out.status.success(), "a second add must fail");
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already has a token"));

    let out = run(&["token", "rotate", "ALICE"]);
    assert!(out.status.success());
    let new = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert!(new.starts_with("sb_") && new != token);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn status_sync_and_tail_without_status() {
    let bus = start("status").await;
    let tsv = bus.dir.join("agents.tsv");
    std::fs::write(&tsv, "# agent\tstate\tmodel\tbranch\ttext\np1\trunning\tsonnet\tb1\tshaders\np2\tidle\t\t\twaiting\n").unwrap();
    let file = tsv.to_str().unwrap();
    let (ok, out, err) = finish(
        sommelier(&bus, &["status", "sync", "--file", file])
            .spawn()
            .unwrap(),
        10,
    )
    .await;
    assert!(ok, "{err}");
    assert_eq!(out.trim(), "sync: 2 updated, 0 cleared, 0 unchanged");

    // Same file again: nothing to send. Then p2 leaves and p1 changes.
    let (_, out, _) = finish(
        sommelier(&bus, &["status", "sync", "--file", file])
            .spawn()
            .unwrap(),
        10,
    )
    .await;
    assert_eq!(out.trim(), "sync: 0 updated, 0 cleared, 2 unchanged");
    std::fs::write(&tsv, "p1\tblocked\tsonnet\tb1\tshaders, device lost\n").unwrap();
    let (_, out, _) = finish(
        sommelier(&bus, &["status", "sync", "--file", file])
            .spawn()
            .unwrap(),
        10,
    )
    .await;
    assert_eq!(out.trim(), "sync: 1 updated, 1 cleared, 0 unchanged");

    let (ok, out, _) = finish(sommelier(&bus, &["status", "list"]).spawn().unwrap(), 10).await;
    assert!(ok);
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 1, "{out}");
    assert!(
        lines[0].starts_with("alice/p1 blocked sonnet b1 #3 ")
            && lines[0].ends_with(" : shaders, device lost"),
        "{out}"
    );

    assert!(
        finish(
            sommelier(&bus, &["send", "CHAT", "hello"]).spawn().unwrap(),
            10
        )
        .await
        .0
    );
    let (_, out, _) = finish(
        sommelier(&bus, &["tail", "--since", "1", "--once"])
            .spawn()
            .unwrap(),
        10,
    )
    .await;
    assert_eq!(out.lines().count(), 1, "STATUS left out by default: {out}");
    let (_, out, _) = finish(
        sommelier(&bus, &["tail", "--since", "1", "--once", "--all"])
            .spawn()
            .unwrap(),
        10,
    )
    .await;
    assert_eq!(out.lines().count(), 5, "{out}");
    let (_, out, _) = finish(
        sommelier(&bus, &["log", "--kind", "STATUS"])
            .spawn()
            .unwrap(),
        10,
    )
    .await;
    assert_eq!(out.lines().count(), 4, "{out}");
}

#[tokio::test]
async fn to_worker_and_worker_filter_and_size_refusal() {
    let bus = start("worker").await;
    for args in [
        ["send", "CHAT", "one", "--to", "alice/w1"],
        ["send", "CHAT", "two", "--to", "alice/w2"],
        ["send", "CHAT", "three", "--to", "alice"],
    ] {
        let child = sommelier(&bus, &args).spawn().unwrap();
        let (ok, _, err) = finish(child, 10).await;
        assert!(ok, "{err}");
    }
    let child = sommelier(&bus, &["send", "CHAT", "four"]).spawn().unwrap();
    assert!(finish(child, 10).await.0);

    let child = sommelier(&bus, &["log", "--worker", "w1"]).spawn().unwrap();
    let (ok, out, err) = finish(child, 10).await;
    assert!(ok, "{err}");
    assert!(out.contains("CHAT @alice/w1 - : one"), "{out}");
    assert!(!out.contains(": two"), "{out}");
    assert!(out.contains(": three") && out.contains(": four"), "{out}");

    let child = sommelier(&bus, &["log", "--to", "me"]).spawn().unwrap();
    let (_, out, _) = finish(child, 10).await;
    assert!(out.contains(": one") && out.contains(": two"), "{out}");

    let long = "x".repeat(201);
    let child = sommelier(&bus, &["send", "ACCEPT", &long]).spawn().unwrap();
    let (ok, _, err) = finish(child, 10).await;
    assert!(!ok);
    assert!(
        err.contains("201 characters") && err.contains("limit for ACCEPT is 200"),
        "{err}"
    );
}

#[tokio::test]
async fn policy_cli_syncs_and_publish_rejects_network_options() {
    let bus = start("policy").await;
    let child = sommelier(&bus, &["policy", "show", "--json"])
        .spawn()
        .unwrap();
    let (ok, out, err) = finish(child, 10).await;
    assert!(ok, "{err}");
    let summary: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(summary["version"], 1);
    assert_eq!(summary["mode"], "off");

    let child = sommelier(&bus, &["sync"]).spawn().unwrap();
    let (ok, _, err) = finish(child, 10).await;
    assert!(ok, "{err}");
    assert_eq!(
        sommelier::policy::Snapshot::cached_hash(&bus.dir)
            .unwrap()
            .as_deref(),
        summary["hash"].as_str()
    );

    for args in [
        [
            "policy",
            "publish",
            "candidate.toml",
            "--url",
            "ws://127.0.0.1:1",
        ],
        ["policy", "rollback", "1", "--url", "ws://127.0.0.1:1"],
    ] {
        let child = sommelier(&bus, &args).spawn().unwrap();
        let (ok, _, err) = finish(child, 10).await;
        assert!(!ok);
        assert!(
            err.contains("unknown flag") && err.contains("--url"),
            "{err}"
        );
    }
}

#[tokio::test]
async fn sync_reports_old_clients_according_to_mode() {
    use sommelier::policy::{DEFAULT_POLICY, Snapshot};

    for (mode, success) in [("shadow", true), ("enforce", false)] {
        let dir = tempfile::tempdir().unwrap();
        let policy_dir = dir.path().join("policy");
        std::fs::create_dir(&policy_dir).unwrap();
        let source = DEFAULT_POLICY
            .replace("min_client = \"0.1.0\"", "min_client = \"9.0.0\"")
            .replace("\"off\"", &format!("\"{mode}\""));
        std::fs::write(policy_dir.join("swarm.policy.toml"), &source).unwrap();
        let bus = start_in(dir.path().to_owned()).await;
        let child = sommelier(&bus, &["sync"]).spawn().unwrap();
        let (ok, _, err) = finish(child, 10).await;
        assert_eq!(ok, success, "{err}");
        assert!(err.contains("older than policy minimum 9.0.0"), "{err}");
        assert_eq!(err.contains("warning:"), success, "{err}");
        assert_eq!(
            Snapshot::cached_hash(&bus.dir).unwrap(),
            Some(Snapshot::parse(&source).unwrap().hash)
        );

        if mode == "enforce" {
            let child = sommelier(&bus, &["send", "CLAIM", "--topic", "queue/1"])
                .spawn()
                .unwrap();
            let (ok, _, err) = finish(child, 10).await;
            assert!(ok, "{err}");
        }
    }
}

#[tokio::test]
async fn idle_cli_json_matches_fixture() {
    let dir = std::env::temp_dir().join(format!("sommelier-cli-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    let alice = t.add("alice", Role::Member).unwrap();
    let bob = t.add("bob", Role::Member).unwrap();
    t.save(&dir.join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(&dir);
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(hub::run(cfg, listener));

    let bus_alice = Bus {
        url: url.clone(),
        dir: dir.clone(),
        token: alice,
    };
    let bus_bob = Bus {
        url: url.clone(),
        dir: dir.clone(),
        token: bob,
    };
    let bus_orch = Bus {
        url,
        dir: dir.clone(),
        token: orch,
    };

    assert!(
        finish(
            sommelier(
                &bus_alice,
                &["send", "HELLO", "os=windows class=codex:2 mode=auto"]
            )
            .spawn()
            .unwrap(),
            10
        )
        .await
        .0
    );
    assert!(
        finish(
            sommelier(
                &bus_alice,
                &[
                    "status",
                    "set",
                    "w1",
                    "--state",
                    "running",
                    "--model",
                    "codex",
                    "--branch",
                    "codex/t1",
                    "compiling"
                ]
            )
            .spawn()
            .unwrap(),
            10
        )
        .await
        .0
    );
    assert!(
        finish(
            sommelier(
                &bus_bob,
                &[
                    "send",
                    "HELLO",
                    "os=linux class=gemini:1 mode=auto accepts=review"
                ]
            )
            .spawn()
            .unwrap(),
            10
        )
        .await
        .0
    );
    assert!(
        finish(
            sommelier(
                &bus_orch,
                &["send", "TASK", "scope: fix views", "--topic", "issue/50"]
            )
            .spawn()
            .unwrap(),
            10
        )
        .await
        .0
    );

    let (ok, out, err) = finish(
        sommelier(&bus_orch, &["idle", "--json"]).spawn().unwrap(),
        10,
    )
    .await;
    assert!(ok, "{err}");
    let mut got: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(got["as_of"].as_u64().unwrap_or(0) > 0);
    // Normalize dynamic age/idle seconds and as_of for deterministic fixture comparison
    got.as_object_mut().unwrap().remove("as_of");
    for arr_key in ["idle", "slots"] {
        for item in got[arr_key].as_array_mut().unwrap() {
            item["age_secs"] = serde_json::json!(0);
            item["idle_secs"] = serde_json::json!(0);
        }
    }
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/idle.json")).unwrap();
    assert_eq!(got, fixture);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn alerts_open_wait_and_alert_ack_with_epoch_fencing() {
    let dir = std::env::temp_dir().join(format!("sommelier-cli-alerts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut t = TokenFile::default();
    let orch = t.add("orch", Role::Orchestrator).unwrap();
    t.save(&dir.join("tokens.json")).unwrap();
    let mut cfg = HubConfig::new(&dir);
    cfg.verbose = false;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(hub::run(cfg, listener));

    let bus = Bus {
        url,
        dir: dir.clone(),
        token: orch,
    };
    let run_cli = |args: &[&str]| finish(sommelier(&bus, args).spawn().unwrap(), 10);

    // 1. When no alerts are open, `sommelier alerts --open --wait 0` exits with code 6 (NO_WORK).
    let (ok, out, err) = run_cli(&["alerts", "--open", "--wait", "0", "--json"]).await;
    assert!(!ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["code"], 6);

    // 2. Seed two open alerts and a coordinator role at epoch 2.
    for (code, key, opened_at, text) in [
        ("STARVED", "slot-b", 2000u64, "slot-b starved"),
        ("IDLE", "slot-a", 1000u64, "slot-a idle"),
    ] {
        let body = serde_json::json!({
            "v": 1,
            "code": code,
            "key": key,
            "severity": "warn",
            "to": "coordinator",
            "state": "open",
            "opened_at": opened_at,
            "last_at": opened_at,
            "count": 1,
            "text": text,
            "data": {},
        })
        .to_string();
        let topic = format!("alert/{code}/{key}");
        assert!(
            run_cli(&["send", "ALERT", &body, "--topic", &topic])
                .await
                .0
        );
    }

    let role_fact = serde_json::json!({
        "v": 1,
        "role": "coordinator",
        "scope": "global",
        "holder": "orch",
        "lease_scope": "full",
        "epoch": 2,
        "state": "held",
        "granted_at": 1000,
        "heartbeat_at": 1000,
        "min_hold_until": 1900,
        "last_reason": "elected",
        "changes_last_hour": 1,
        "frozen": false,
    })
    .to_string();
    assert!(
        run_cli(&["send", "FACT", &role_fact, "--topic", "role/coordinator"])
            .await
            .0
    );

    // 3. `sommelier alerts --open --wait 5 --json` returns immediately with code 0, oldest first.
    let (ok, out, err) = run_cli(&["alerts", "--open", "--wait", "5", "--json"]).await;
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true);
    let arr = v["alerts"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["key"], "slot-a");
    assert_eq!(arr[1]["key"], "slot-b");

    // 4. Filter by `--code STARVED`.
    let (ok, out, err) = run_cli(&["alerts", "--code", "STARVED", "--json"]).await;
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v["alerts"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["code"], "STARVED");

    // 5. `sommelier alert ack slot-a --epoch 1` (stale epoch) is refused with exit code 3.
    let (ok, out, _) = run_cli(&["alert", "ack", "slot-a", "--epoch", "1", "--json"]).await;
    assert!(!ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["code"], 3);
    assert!(
        v["reason"]
            .as_str()
            .unwrap_or("")
            .contains("you are no longer coordinator (your epoch 1, current 2), stop")
    );

    // 6. `sommelier alert ack slot-a --epoch 2` succeeds and transitions slot-a to `acked`.
    let (ok, out, err) = run_cli(&["alert", "ack", "slot-a", "--epoch", "2", "--json"]).await;
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["state"], "acked");
    assert_eq!(v["key"], "slot-a");

    // 7. Now `sommelier alerts --open --json` shows only `slot-b` open.
    let (ok, out, err) = run_cli(&["alerts", "--open", "--json"]).await;
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v["alerts"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["key"], "slot-b");

    // 8. Acking `STARVED/slot-b` without `--epoch` also succeeds.
    let (ok, _, err) = run_cli(&["alert", "ack", "STARVED/slot-b", "--json"]).await;
    assert!(ok, "{err}");

    let (ok, out, _) = run_cli(&["alerts", "--open", "--wait", "0", "--json"]).await;
    assert!(!ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["code"], 6);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn help_documents_all_v2_commands_and_subcommand_help() {
    let bin = env!("CARGO_BIN_EXE_sommelier");
    let out = Command::new(bin).arg("help").output().unwrap();
    assert!(out.status.success());
    let main_help = String::from_utf8_lossy(&out.stdout);

    let v2_cmds = [
        "guide", "doctor", "handover", "alerts", "alert", "task", "land", "quota", "plan", "agent",
        "standby", "promote", "drill",
    ];
    for cmd in v2_cmds {
        assert!(
            main_help.contains(&format!("sommelier {cmd}")),
            "main `sommelier help` is missing `sommelier {cmd}`:\n{main_help}"
        );

        let sub = Command::new(bin).args(["help", cmd]).output().unwrap();
        assert!(
            sub.status.success(),
            "`sommelier help {cmd}` failed: {}",
            String::from_utf8_lossy(&sub.stderr)
        );
        let sub_text = String::from_utf8_lossy(&sub.stdout);
        assert!(
            sub_text.contains("usage:"),
            "`sommelier help {cmd}` missing usage header:\n{sub_text}"
        );

        let flag = Command::new(bin).args([cmd, "--help"]).output().unwrap();
        assert!(
            flag.status.success(),
            "`sommelier {cmd} --help` failed: {}",
            String::from_utf8_lossy(&flag.stderr)
        );
    }
}
