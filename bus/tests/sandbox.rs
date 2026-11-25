//! The worker sandbox: `wrap`, `detect`, the launcher hook choice and the doctor line.
//! No test starts a container, uses the network or a vendor login.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use sommelier::agent::launch::{HookEnv, plain_hook_argv, plan_hook_command};
use sommelier::agent::sandbox::{
    self, ContractEnv, Mount, SandboxLevel, SandboxProbe, SandboxSpec, detect, doctor_line,
    parse_mounts, wrap,
};

fn spec() -> SandboxSpec {
    SandboxSpec {
        runtime: "podman".into(),
        image: "sommelier-worker:latest".into(),
        worktree: PathBuf::from("/srv/wt one/task-1"),
        out_dir: PathBuf::from("/srv/out/task-1"),
        prompt_file: PathBuf::from("/srv/wt one/task-1/.sommelier/prompt.md"),
        hooks_dir: PathBuf::from("/srv/hooks"),
        git_dir: Some(PathBuf::from("/srv/clone/.git")),
        cache_dirs: vec![Mount {
            host: PathBuf::from("/srv/cache/cargo"),
            container: "/home/worker/.cargo/registry".into(),
            readonly: false,
        }],
        vendor_dirs: vec![Mount {
            host: PathBuf::from("/srv/vendor/claude"),
            container: "/home/worker/.claude".into(),
            readonly: false,
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
            "/srv/clone/.git",
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
    let m = parse_mounts("/a/b=/home/worker/.claude; /c=/cache:ro").unwrap();
    assert_eq!(m.len(), 2);
    assert!(!m[0].readonly && m[1].readonly);
    assert_eq!(m[1].container, "/cache");
    assert!(parse_mounts("no-equals").is_err());
    assert!(parse_mounts("/a=relative").is_err());
    assert!(parse_mounts("").unwrap().is_empty());
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
            .any(|a| a == "type=bind,src=/srv/vendor/claude,dst=/home/worker/.claude")
    );
    assert!(
        argv.iter()
            .any(|a| a == "type=bind,src=/srv/hooks,dst=/hooks,readonly")
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
