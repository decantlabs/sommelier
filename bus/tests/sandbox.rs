//! The worker sandbox: `wrap`, `detect`, the launcher hook choice and the doctor line.
//! No test starts a container, uses the network or a vendor login.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use sommelier::agent::launch::{
    HookEnv, container_clone_dir, fetch_container_result, plain_hook_argv, plan_hook_command,
    plan_hook_command_with, prepare_container_clone,
};
use sommelier::agent::sandbox::{
    self, ContractEnv, Mount, SandboxLevel, SandboxProbe, SandboxSpec, detect, doctor_line,
    mount_warnings, parse_mounts, runtime_warning, wrap,
};
use sommelier::cli::doctor::sandbox_warnings;

fn spec() -> SandboxSpec {
    SandboxSpec {
        runtime: "podman".into(),
        image: "sommelier-worker:latest".into(),
        worktree: PathBuf::from("/srv/wt one/task-1"),
        out_dir: PathBuf::from("/srv/out/task-1"),
        prompt_file: PathBuf::from("/srv/wt one/task-1/.sommelier/prompt.md"),
        hooks_dir: PathBuf::from("/srv/hooks"),
        cache_dirs: vec![Mount {
            host: PathBuf::from("/srv/cache/cargo"),
            container: "/home/worker/.cargo/registry".into(),
            readonly: true,
        }],
        vendor_dirs: vec![Mount {
            host: PathBuf::from("/srv/vendor/claude"),
            container: "/home/worker/.claude".into(),
            readonly: true,
        }],
        env: ContractEnv {
            task: "t-1".into(),
            attempt: 2,
            branch: "fx/x".into(),
            model: "claude".into(),
            effort: "medium".into(),
            cargo_build_jobs: "4".into(),
        },
    }
}

fn hook() -> Vec<String> {
    vec!["sh".into(), "/hooks/hook-claude.sh".into()]
}

fn mounts_of(argv: &[String]) -> Vec<String> {
    argv.windows(2)
        .filter(|w| w[0] == "--mount")
        .map(|w| w[1].clone())
        .collect()
}

#[test]
fn wrap_mounts_only_the_listed_folders() {
    let argv = wrap(&hook(), &spec()).unwrap();
    let dsts: Vec<String> = mounts_of(&argv)
        .iter()
        .map(|m| {
            m.split(',')
                .find_map(|p| p.strip_prefix("dst="))
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        dsts,
        vec![
            "/work",
            "/out",
            "/prompt/prompt.md",
            "/hooks",
            "/home/worker/.cargo/registry",
            "/home/worker/.claude"
        ]
    );
    assert!(mounts_of(&argv).iter().all(|m| m.starts_with("type=bind,")));
    assert!(!argv.iter().any(|a| a == "-v" || a == "--volume"));
    // hooks and prompt are read only, worktree is not
    let m = mounts_of(&argv);
    assert!(m[2].ends_with(",readonly") && m[3].ends_with(",readonly"));
    assert!(!m[0].ends_with(",readonly"));
    // vendor and cache mounts are read only unless the member wrote :rw
    assert!(m[4].ends_with(",readonly") && m[5].ends_with(",readonly"));
}

#[test]
fn wrap_never_mounts_a_git_folder() {
    // The container works in its own plain clone. Nothing named .git is mounted, whatever the paths are.
    let argv = wrap(&hook(), &spec()).unwrap();
    for m in mounts_of(&argv) {
        assert!(!m.contains(".git"), "a git folder is mounted: {m}");
    }
    // The spec has no field for a git folder any more.
    let text =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/agent/sandbox.rs"))
            .unwrap();
    assert!(!text.contains("git_dir"));
}

#[test]
fn wrap_keeps_a_path_with_spaces_as_one_argument() {
    let argv = wrap(&hook(), &spec()).unwrap();
    assert!(argv.contains(&"type=bind,src=/srv/wt one/task-1,dst=/work".to_owned()));
}

#[test]
fn wrap_never_names_the_members_home_or_secrets() {
    let argv = wrap(&hook(), &spec()).unwrap().join("\n");
    for bad in [
        "/home/member",
        "\\Users\\",
        ".ssh",
        "SSH_AUTH_SOCK",
        "TOKEN",
        "token",
    ] {
        assert!(!argv.contains(bad), "found {bad}");
    }
}

#[test]
fn wrap_environment_is_exactly_the_hook_contract() {
    let argv = wrap(&hook(), &spec()).unwrap();
    let env: BTreeMap<String, String> = argv
        .windows(2)
        .filter(|w| w[0] == "-e")
        .map(|w| {
            let (k, v) = w[1].split_once('=').unwrap();
            (k.to_owned(), v.to_owned())
        })
        .collect();
    let names: Vec<&str> = env.keys().map(String::as_str).collect();
    let mut want: Vec<&str> = sandbox::contract_env_names().to_vec();
    want.sort();
    assert_eq!(names, want);
    assert_eq!(env["SOMMELIER_WORKTREE"], "/work");
    assert_eq!(env["SOMMELIER_OUT"], "/out");
    assert_eq!(env["SOMMELIER_PROMPT"], "/prompt/prompt.md");
    assert_eq!(env["SOMMELIER_ATTEMPT"], "2");
    assert!(!argv.iter().any(|a| a == "--env-host" || a == "--env-file"));
}

#[test]
fn wrap_has_no_privileged_flag_and_hardens() {
    let argv = wrap(&hook(), &spec()).unwrap();
    for bad in [
        "--privileged",
        "--cap-add",
        "--device",
        "--pid=host",
        "--network=host",
        "--net=host",
        "--ipc=host",
        "--userns=host",
        "--security-opt=seccomp=unconfined",
    ] {
        assert!(!argv.iter().any(|a| a.starts_with(bad)), "found {bad}");
    }
    for good in [
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--rm",
    ] {
        assert!(argv.iter().any(|a| a == good), "missing {good}");
    }
    // the image and then the hook command come last
    let n = argv.len();
    assert_eq!(argv[n - 3], "sommelier-worker:latest");
    assert_eq!(&argv[n - 2..], hook().as_slice());
    // docker has no keep-id
    let mut s = spec();
    s.runtime = "docker".into();
    assert!(
        !wrap(&hook(), &s)
            .unwrap()
            .iter()
            .any(|a| a.starts_with("--userns"))
    );
}

#[test]
fn wrap_refuses_what_the_mount_syntax_cannot_carry() {
    let mut s = spec();
    s.worktree = PathBuf::from("/srv/a,b");
    assert!(wrap(&hook(), &s).unwrap_err().contains("comma"));
    let mut s = spec();
    s.runtime = "lxc".into();
    assert!(wrap(&hook(), &s).is_err());
    let mut s = spec();
    s.vendor_dirs[0].container = "relative".into();
    assert!(wrap(&hook(), &s).is_err());
    assert!(wrap(&[], &spec()).is_err());
}

#[test]
fn parse_mounts_reads_the_list() {
    let m = parse_mounts("/a/b=/home/worker/.claude; /c=/cache:ro; /d=/rw:rw").unwrap();
    assert_eq!(m.len(), 3);
    // read only is the default, `:rw` is the only way to a writable mount
    assert!(m[0].readonly && m[1].readonly && !m[2].readonly);
    assert_eq!(m[1].container, "/cache");
    assert_eq!(m[2].container, "/rw");
    assert!(parse_mounts("no-equals").is_err());
    assert!(parse_mounts("/a=relative").is_err());
    assert!(parse_mounts("").unwrap().is_empty());
}

#[test]
fn a_vendor_mount_is_read_only_unless_rw_is_written() {
    let mut s = spec();
    s.vendor_dirs = parse_mounts("/srv/v=/home/worker/.claude").unwrap();
    let argv = wrap(&hook(), &s).unwrap();
    assert!(
        argv.iter()
            .any(|a| a == "type=bind,src=/srv/v,dst=/home/worker/.claude,readonly")
    );
    s.vendor_dirs = parse_mounts("/srv/v=/home/worker/.claude:rw").unwrap();
    let argv = wrap(&hook(), &s).unwrap();
    assert!(
        argv.iter()
            .any(|a| a == "type=bind,src=/srv/v,dst=/home/worker/.claude")
    );
}

#[test]
fn doctor_warns_when_a_mounted_vendor_folder_is_the_one_the_member_uses() {
    let home = Path::new("/home/member");
    let own = parse_mounts("/home/member/.claude=/home/worker/.claude").unwrap();
    let w = mount_warnings(&own, &[], home);
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("the one your own tools use") && w[0].contains("read only"));
    let own_rw = parse_mounts("/home/member/.claude=/home/worker/.claude:rw").unwrap();
    let w = mount_warnings(&own_rw, &[], home);
    assert!(w[0].contains("plant a command"));
    // a folder below it, and the home folder itself
    let below = parse_mounts("/home/member/.codex/auth=/home/worker/.codex").unwrap();
    assert_eq!(mount_warnings(&below, &[], home).len(), 1);
    let whole = parse_mounts("/home/member=/home/worker/h").unwrap();
    assert!(mount_warnings(&whole, &[], home)[0].contains("whole home folder"));
    // a dedicated folder is quiet, and so is a trailing slash on a dedicated one
    let own_dedicated = parse_mounts("/srv/worker-login/claude=/home/worker/.claude").unwrap();
    assert!(mount_warnings(&own_dedicated, &[], home).is_empty());
    // the sibling folder with the same prefix is not the same folder
    let sibling = parse_mounts("/home/member/.claude-worker=/home/worker/.claude").unwrap();
    assert!(mount_warnings(&sibling, &[], home).is_empty());
    // the host registry, writable, is flagged. Read only is not.
    let registry_rw =
        parse_mounts("/home/member/.cargo/registry=/home/worker/.cargo/registry:rw").unwrap();
    assert_eq!(mount_warnings(&[], &registry_rw, home).len(), 1);
    let registry_ro =
        parse_mounts("/home/member/.cargo/registry=/home/worker/.cargo/registry").unwrap();
    assert!(mount_warnings(&[], &registry_ro, home).is_empty());
    // doctor reads the lists from its settings
    let get = |k: &str| match k {
        "SOMMELIER_SANDBOX_VENDOR_DIRS" => {
            Some("/home/member/.claude=/home/worker/.claude".to_owned())
        }
        _ => None,
    };
    let w = sandbox_warnings(&get, home, &Fake::default());
    assert_eq!(w.len(), 1);
}

#[test]
fn doctor_warns_about_a_named_runtime_that_is_not_rootless() {
    let rootful = Fake {
        commands: vec!["docker"],
        ..Fake::default()
    };
    assert!(
        runtime_warning(&rootful, "docker")
            .unwrap()
            .contains("rootless")
    );
    let ok = Fake {
        commands: vec!["docker"],
        rootless: vec!["docker"],
        ..Fake::default()
    };
    assert!(runtime_warning(&ok, "docker").is_none());
    let get = |k: &str| (k == "SOMMELIER_SANDBOX_RUNTIME").then(|| "docker".to_owned());
    assert_eq!(
        sandbox_warnings(&get, Path::new("/home/m"), &rootful).len(),
        1
    );
}

#[derive(Default)]
struct Fake {
    requested: Option<&'static str>,
    commands: Vec<&'static str>,
    rootless: Vec<&'static str>,
    inside: bool,
    elevated: Option<bool>,
    secrets: Vec<&'static str>,
}

impl SandboxProbe for Fake {
    fn requested(&self) -> Option<String> {
        self.requested.map(str::to_owned)
    }
    fn has_command(&self, name: &str) -> bool {
        self.commands.contains(&name)
    }
    fn runtime_rootless(&self, runtime: &str) -> Option<bool> {
        Some(self.rootless.contains(&runtime))
    }
    fn inside_container(&self) -> bool {
        self.inside
    }
    fn is_elevated(&self) -> Option<bool> {
        self.elevated
    }
    fn home_secret_folders(&self) -> Vec<String> {
        self.secrets.iter().map(|s| (*s).to_owned()).collect()
    }
}

#[test]
fn detect_prefers_podman_then_docker() {
    let both = Fake {
        commands: vec!["docker", "podman"],
        rootless: vec!["docker", "podman"],
        elevated: Some(false),
        secrets: vec![".ssh"],
        ..Fake::default()
    };
    assert_eq!(detect(&both).unwrap().runtime.as_deref(), Some("podman"));
    let docker = Fake {
        commands: vec!["docker"],
        rootless: vec!["docker"],
        ..Fake::default()
    };
    assert_eq!(detect(&docker).unwrap().runtime.as_deref(), Some("docker"));
}

#[test]
fn detect_runtime_present_or_not() {
    let none = Fake {
        elevated: Some(false),
        secrets: vec![".ssh"],
        ..Fake::default()
    };
    let r = detect(&none).unwrap();
    assert_eq!(r.in_force, SandboxLevel::None);
    assert_eq!(r.offers, vec![SandboxLevel::None]);

    let present = Fake {
        commands: vec!["podman"],
        rootless: vec!["podman"],
        elevated: Some(false),
        secrets: vec![".ssh"],
        ..Fake::default()
    };
    let r = detect(&present).unwrap();
    assert_eq!(r.offers, vec![SandboxLevel::Container, SandboxLevel::None]);
    // offered is not in force
    assert_eq!(r.in_force, SandboxLevel::None);

    let rootful = Fake {
        commands: vec!["podman"],
        elevated: Some(false),
        secrets: vec![".ssh"],
        ..Fake::default()
    };
    let r = detect(&rootful).unwrap();
    assert!(r.runtime.is_none());
    assert!(r.notes.iter().any(|n| n.contains("rootless")));
}

#[test]
fn detect_container_requested() {
    let ok = Fake {
        requested: Some("container"),
        commands: vec!["podman"],
        rootless: vec!["podman"],
        ..Fake::default()
    };
    let r = detect(&ok).unwrap();
    assert_eq!(r.in_force, SandboxLevel::Container);
    assert!(r.fix.is_none());
    assert!(r.notes.iter().any(|n| n.contains("network is not limited")));

    let missing = Fake {
        requested: Some("container"),
        ..Fake::default()
    };
    let r = detect(&missing).unwrap();
    assert_eq!(r.in_force, SandboxLevel::None);
    assert!(r.fix.unwrap().contains("rootless podman"));
}

#[test]
fn detect_inside_container_and_plain_user() {
    let inside = Fake {
        inside: true,
        ..Fake::default()
    };
    let r = detect(&inside).unwrap();
    assert_eq!(r.in_force, SandboxLevel::Container);
    assert!(r.inside_container);

    let user = Fake {
        requested: Some("user"),
        elevated: Some(false),
        ..Fake::default()
    };
    let r = detect(&user).unwrap();
    assert_eq!(r.in_force, SandboxLevel::User);
    assert!(r.offers.contains(&SandboxLevel::User));
    assert!(r.notes.iter().any(|n| n.contains("cannot see")));

    let dirty = Fake {
        requested: Some("user"),
        elevated: Some(false),
        secrets: vec![".ssh"],
        ..Fake::default()
    };
    let r = detect(&dirty).unwrap();
    assert_eq!(r.in_force, SandboxLevel::None);
    assert!(r.fix.is_some());

    let admin = Fake {
        requested: Some("user"),
        elevated: Some(true),
        ..Fake::default()
    };
    assert_eq!(detect(&admin).unwrap().in_force, SandboxLevel::None);
    let unknown = Fake {
        requested: Some("user"),
        elevated: None,
        ..Fake::default()
    };
    assert_eq!(detect(&unknown).unwrap().in_force, SandboxLevel::None);
}

#[test]
fn detect_refuses_an_unknown_request() {
    let bad = Fake {
        requested: Some("vm"),
        ..Fake::default()
    };
    let e = detect(&bad).unwrap_err();
    assert!(e.contains("SOMMELIER_SANDBOX") && !e.contains('\n'));
}

#[test]
fn doctor_line_for_each_level() {
    let container = detect(&Fake {
        requested: Some("container"),
        commands: vec!["podman"],
        rootless: vec!["podman"],
        elevated: Some(false),
        ..Fake::default()
    })
    .unwrap();
    assert_eq!(
        doctor_line(&container),
        "sandbox: container (can offer: container via podman, user, none)"
    );
    let user = detect(&Fake {
        requested: Some("user"),
        elevated: Some(false),
        ..Fake::default()
    })
    .unwrap();
    assert_eq!(doctor_line(&user), "sandbox: user (can offer: user, none)");
    let none = detect(&Fake {
        elevated: Some(true),
        ..Fake::default()
    })
    .unwrap();
    assert_eq!(doctor_line(&none), "sandbox: none (can offer: none)");
}

#[test]
fn doctor_line_has_the_fix_when_no_runtime() {
    let r = detect(&Fake {
        requested: Some("container"),
        elevated: Some(false),
        ..Fake::default()
    })
    .unwrap();
    let line = doctor_line(&r);
    assert!(line.starts_with("sandbox: none (can offer: user, none) | fix: "));
    assert!(line.contains("install rootless podman"));
    assert!(!line.contains('\n'));
}

fn hook_env() -> HookEnv {
    HookEnv {
        task_id: "t-1".into(),
        attempt: 1,
        branch: "fx/x".into(),
        worktree: PathBuf::from("/srv/wt/t-1"),
        prompt_file: PathBuf::from("/srv/wt/t-1/.sommelier/prompt.md"),
        model: "claude".into(),
        effort: "medium".into(),
        out_dir: PathBuf::from("/srv/out/t-1"),
    }
}

fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| (*v).to_owned())
    }
}

#[test]
fn launch_none_is_todays_command() {
    let script = Path::new("/srv/hooks/hook-claude.sh");
    let today = vec!["sh".to_owned(), "/srv/hooks/hook-claude.sh".to_owned()];
    assert_eq!(plain_hook_argv(script), today);
    // unset, empty and explicit none all give the same command
    assert_eq!(
        plan_hook_command(script, &hook_env(), &lookup(&[])).unwrap(),
        today
    );
    assert_eq!(
        plan_hook_command(script, &hook_env(), &lookup(&[("SOMMELIER_SANDBOX", " ")])).unwrap(),
        today
    );
    assert_eq!(
        plan_hook_command(
            script,
            &hook_env(),
            &lookup(&[("SOMMELIER_SANDBOX", "none")])
        )
        .unwrap(),
        today
    );
    // a binary hook is run directly, as before
    assert_eq!(
        plain_hook_argv(Path::new("/srv/hooks/run-it")),
        vec!["/srv/hooks/run-it".to_owned()]
    );
}

#[test]
fn launch_container_gives_the_wrapped_command() {
    let argv = plan_hook_command(
        Path::new("/srv/hooks/hook-claude.sh"),
        &hook_env(),
        &lookup(&[
            ("SOMMELIER_SANDBOX", "container"),
            ("SOMMELIER_SANDBOX_RUNTIME", "podman"),
            ("SOMMELIER_SANDBOX_IMAGE", "my-image:1"),
            (
                "SOMMELIER_SANDBOX_VENDOR_DIRS",
                "/srv/vendor/claude=/home/worker/.claude",
            ),
        ]),
    )
    .unwrap();
    assert_eq!(argv[0], "podman");
    assert_eq!(argv[1], "run");
    assert!(argv.contains(&"my-image:1".to_owned()));
    assert_eq!(
        &argv[argv.len() - 2..],
        ["sh".to_owned(), "/hooks/hook-claude.sh".to_owned()]
    );
    assert!(
        argv.iter()
            .any(|a| a == "type=bind,src=/srv/vendor/claude,dst=/home/worker/.claude,readonly")
    );
    assert!(
        argv.iter()
            .any(|a| a == "type=bind,src=/srv/hooks,dst=/hooks,readonly")
    );
}

#[test]
fn launch_container_never_picks_a_runtime_that_is_not_rootless() {
    let script = Path::new("/srv/hooks/hook-claude.sh");
    let get = lookup(&[("SOMMELIER_SANDBOX", "container")]);
    // docker is installed and answers, but does not report rootless mode
    let rootful = Fake {
        commands: vec!["docker"],
        ..Fake::default()
    };
    let e = plan_hook_command_with(script, &hook_env(), &get, &rootful).unwrap_err();
    assert!(e.contains("rootless") && !e.contains('\n'));
    // rootless docker is taken, and podman wins when both are rootless
    let docker = Fake {
        commands: vec!["docker"],
        rootless: vec!["docker"],
        ..Fake::default()
    };
    assert_eq!(
        plan_hook_command_with(script, &hook_env(), &get, &docker).unwrap()[0],
        "docker"
    );
    let both = Fake {
        commands: vec!["docker", "podman"],
        rootless: vec!["docker", "podman"],
        ..Fake::default()
    };
    assert_eq!(
        plan_hook_command_with(script, &hook_env(), &get, &both).unwrap()[0],
        "podman"
    );
}

#[test]
fn launch_refuses_an_unknown_value_in_one_line() {
    let e = plan_hook_command(
        Path::new("/srv/hooks/hook-claude.sh"),
        &hook_env(),
        &lookup(&[("SOMMELIER_SANDBOX", "vm")]),
    )
    .unwrap_err();
    assert!(e.contains("unknown sandbox level") && !e.contains('\n'));
    let e = plan_hook_command(
        Path::new("/srv/hooks/hook-claude.cmd"),
        &hook_env(),
        &lookup(&[
            ("SOMMELIER_SANDBOX", "container"),
            ("SOMMELIER_SANDBOX_RUNTIME", "podman"),
        ]),
    )
    .unwrap_err();
    assert!(e.contains("shell hooks only"));
}

#[test]
fn shell_scripts_pass_sh_n() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../swarm/sandbox");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) == Some("sh") {
            seen += 1;
            match Command::new("sh").arg("-n").arg(&p).output() {
                Ok(o) => assert!(
                    o.status.success(),
                    "sh -n failed for {}: {}",
                    p.display(),
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(_) => eprintln!("sh not found, skipped {}", p.display()),
            }
        }
    }
    assert!(seen >= 1);
}

// ---- the container's own clone ----

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A member's clone with a linked worktree on a task branch, and the hook env that points at it.
fn member_repo(root: &Path) -> (PathBuf, HookEnv) {
    let repo = root.join("member clone");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    let wt = root.join("wt-t1");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "fx/x", wt.to_str().unwrap()],
    );
    std::fs::create_dir_all(wt.join(".sommelier")).unwrap();
    std::fs::write(wt.join(".sommelier/prompt.md"), "prompt").unwrap();
    std::fs::write(wt.join(".sommelier/brief.md"), "brief").unwrap();
    let env = HookEnv {
        task_id: "t1".into(),
        attempt: 1,
        branch: "fx/x".into(),
        worktree: wt.clone(),
        prompt_file: wt.join(".sommelier/prompt.md"),
        model: "claude".into(),
        effort: "medium".into(),
        out_dir: root.join("data/agent/workers/t1-1"),
    };
    (repo, env)
}

fn temp_root(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("sommelier-sandbox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn the_container_works_in_its_own_plain_clone_and_never_sees_the_members_git_folder() {
    let root = temp_root("clone");
    let (repo, env) = member_repo(&root);
    let clone = prepare_container_clone(&env).unwrap();
    assert_eq!(clone, container_clone_dir(&env));
    // a plain clone: a real .git folder, no pointer file, no link back to the member's clone
    assert!(clone.join(".git").is_dir());
    assert_eq!(git(&clone, &["remote"]), "");
    assert!(clone.join("a.txt").is_file());
    assert_eq!(git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]), "fx/x");
    assert!(clone.join(".sommelier/brief.md").is_file());
    // the command of the container names the clone and no path of the member's repository
    let launch = HookEnv {
        worktree: clone.clone(),
        ..env.clone()
    };
    let argv = plan_hook_command(
        Path::new("/srv/hooks/hook-claude.sh"),
        &launch,
        &lookup(&[
            ("SOMMELIER_SANDBOX", "container"),
            ("SOMMELIER_SANDBOX_RUNTIME", "podman"),
        ]),
    )
    .unwrap();
    let all = argv.join("\n");
    assert!(all.contains(&format!("src={},dst=/work", clone.display())));
    assert!(!all.contains(&repo.display().to_string()));
    for m in mounts_of(&argv) {
        assert!(!m.contains(".git"), "{m}");
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_hostile_clone_changes_nothing_on_the_host_and_only_the_commit_comes_back() {
    let root = temp_root("hostile");
    let (repo, env) = member_repo(&root);
    let clone = prepare_container_clone(&env).unwrap();
    let marker = root.join("RAN");
    // What a hostile worker can write in its own folder: a hook, a file system monitor, a moved main,
    // a pointer in place of .git later, and one honest looking commit.
    let hook = clone.join(".git/hooks/post-checkout");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    let monitor = clone.join("monitor.sh");
    std::fs::write(
        &monitor,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    git(
        &clone,
        &["config", "core.fsmonitor", monitor.to_str().unwrap()],
    );
    git(
        &clone,
        &[
            "config",
            "core.hooksPath",
            &format!("{}/.git/hooks", clone.display()),
        ],
    );
    std::fs::write(clone.join("a.txt"), "two\n").unwrap();
    // `git add` here is the hostile clone's own git: ignore a failing monitor, never mind
    let _ = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["-c", "core.fsmonitor=false", "add", "a.txt"])
        .status();
    git(
        &clone,
        &[
            "-c",
            "core.fsmonitor=false",
            "commit",
            "-q",
            "--no-verify",
            "-m",
            "work",
        ],
    );
    let result = git(&clone, &["-c", "core.fsmonitor=false", "rev-parse", "HEAD"]);
    let main_before = git(&repo, &["rev-parse", "refs/heads/main"]);
    let hooks_before: Vec<_> = std::fs::read_dir(repo.join(".git/hooks"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    let config_before = std::fs::read_to_string(repo.join(".git/config")).unwrap();

    let got = fetch_container_result(&env).unwrap();
    assert_eq!(got.as_deref(), Some(result.as_str()));
    // the task branch of the worktree stands on the result, main did not move
    assert_eq!(git(&env.worktree, &["rev-parse", "HEAD"]), result);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/fx/x"]), result);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/main"]), main_before);
    assert_eq!(
        git(&repo, &["rev-parse", "refs/sommelier/result/t1-1"]),
        result
    );
    // nothing of the clone's hook or monitor ran, and the member's git folder is as it was
    assert!(!marker.exists(), "code of the clone ran on the host");
    let hooks_after: Vec<_> = std::fs::read_dir(repo.join(".git/hooks"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(hooks_before, hooks_after);
    assert_eq!(
        config_before,
        std::fs::read_to_string(repo.join(".git/config")).unwrap()
    );
    assert!(
        !std::fs::read_to_string(repo.join(".git/config"))
            .unwrap()
            .contains("fsmonitor")
    );
    // the next hook start builds a new clone from the worktree and ignores the old one
    let again = prepare_container_clone(&env).unwrap();
    assert_eq!(git(&again, &["rev-parse", "HEAD"]), result);
    assert!(!again.join(".git/hooks/post-checkout").exists());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_clone_whose_git_entry_was_replaced_by_a_pointer_is_refused() {
    let root = temp_root("pointer");
    let (_repo, env) = member_repo(&root);
    let clone = prepare_container_clone(&env).unwrap();
    // the second way of the finding: /work/.git rewritten to point at a folder of the member
    std::fs::remove_dir_all(clone.join(".git")).unwrap();
    std::fs::write(clone.join(".git"), "gitdir: /home/member/a/b\n").unwrap();
    let e = fetch_container_result(&env).unwrap_err();
    assert!(e.contains("not a plain folder"), "{e}");
    // and a pointer never reaches a mount: the path of the container clone comes from the task record
    assert_eq!(
        container_clone_dir(&env),
        root.join("data/agent/workers/t1-1.clone")
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn no_commit_in_the_clone_gives_no_result() {
    let root = temp_root("nocommit");
    let (repo, env) = member_repo(&root);
    prepare_container_clone(&env).unwrap();
    let before = git(&repo, &["rev-parse", "refs/heads/fx/x"]);
    assert_eq!(fetch_container_result(&env).unwrap(), None);
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/fx/x"]), before);
    std::fs::remove_dir_all(&root).unwrap();
}
