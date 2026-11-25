//! The worker sandbox (factory design 7.5): the level a machine runs at, the command line that runs one
//! launcher hook inside a rootless container, and the doctor line.
//!
//! `wrap` and `detect` are pure. The real system sits behind `SandboxProbe`, so tests use a fake.
//!
//! What a container protects: the worker sees its worktree, its toolchain cache, its vendor tool login and
//! nothing else of the member's machine. What it does NOT limit: the network. A plain rootless runtime cannot
//! restrict egress to a list of hosts, so the worker can reach any address the machine can reach. Read
//! `swarm/sandbox/README.md` for the full list of limits.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where the pieces appear inside the container.
pub const WORK_DIR: &str = "/work";
pub const OUT_DIR: &str = "/out";
pub const PROMPT_FILE: &str = "/prompt/prompt.md";
pub const HOOKS_DIR: &str = "/hooks";
/// The home folder of the worker user inside the image. A memory only folder, vendor logins mount below it.
pub const HOME_DIR: &str = "/home/worker";
/// Image used when the member names none.
pub const DEFAULT_IMAGE: &str = "sommelier-worker:latest";

/// The three levels of design 7.5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SandboxLevel {
    Container,
    User,
    None,
}

impl SandboxLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxLevel::Container => "container",
            SandboxLevel::User => "user",
            SandboxLevel::None => "none",
        }
    }

    /// Parses `container`, `user` or `none`. Anything else is one clear line.
    pub fn parse(text: &str) -> Result<SandboxLevel, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "container" => Ok(SandboxLevel::Container),
            "user" => Ok(SandboxLevel::User),
            "none" => Ok(SandboxLevel::None),
            other => Err(format!(
                "unknown sandbox level `{other}`: set SOMMELIER_SANDBOX to container, user or none"
            )),
        }
    }
}

/// The level asked for by the environment variable `SOMMELIER_SANDBOX`. Unset or empty means `none`.
pub fn level_from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<SandboxLevel, String> {
    match get("SOMMELIER_SANDBOX")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
    {
        Some(v) => SandboxLevel::parse(&v),
        None => Ok(SandboxLevel::None),
    }
}

/// What the machine tells a program. The real implementation is `SystemProbe`.
pub trait SandboxProbe {
    /// The raw value of `SOMMELIER_SANDBOX`, if set.
    fn requested(&self) -> Option<String>;
    /// Is a program of this name found on PATH.
    fn has_command(&self, name: &str) -> bool;
    /// Does the runtime say it runs rootless. `None` when it could not be asked.
    fn runtime_rootless(&self, runtime: &str) -> Option<bool>;
    /// Does this process run inside a container.
    fn inside_container(&self) -> bool;
    /// Does this process run with administrator or root rights. `None` when it could not be asked.
    fn is_elevated(&self) -> Option<bool>;
    /// Names of folders in the home of this account that belong to a member's own life (SSH keys, browser
    /// profiles, cloud logins). A program can see these. It cannot see what other accounts may read.
    fn home_secret_folders(&self) -> Vec<String>;
}

/// The result of `detect`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxReport {
    /// The level the hooks of this machine actually run at.
    pub in_force: SandboxLevel,
    /// The level the member asked for, if any.
    pub requested: Option<SandboxLevel>,
    /// The levels this machine can offer, best first. `none` is always offered.
    pub offers: Vec<SandboxLevel>,
    /// The rootless runtime that would be used for `container`.
    pub runtime: Option<String>,
    pub inside_container: bool,
    /// What a program cannot observe, said plainly.
    pub notes: Vec<String>,
    /// What to do, when a requested level is not available.
    pub fix: Option<String>,
}

/// The rootless runtime to use: podman first, docker second. Returns the name and what the probe said.
pub fn pick_runtime(probe: &dyn SandboxProbe) -> Option<String> {
    ["podman", "docker"]
        .into_iter()
        .find(|name| probe.has_command(name) && probe.runtime_rootless(name) == Some(true))
        .map(str::to_owned)
}

/// Says which level this machine can offer and which one is in force.
pub fn detect(probe: &dyn SandboxProbe) -> Result<SandboxReport, String> {
    let requested = match probe
        .requested()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
    {
        Some(v) => Some(SandboxLevel::parse(&v)?),
        None => None,
    };
    let runtime = pick_runtime(probe);
    let inside = probe.inside_container();
    let mut notes = Vec::new();

    let mut offers = Vec::new();
    if let Some(name) = &runtime {
        offers.push(SandboxLevel::Container);
        notes.push(format!(
            "container uses {name}: the network is not limited to a list of hosts"
        ));
    } else if probe.has_command("podman") || probe.has_command("docker") {
        notes.push(
            "a container runtime is installed but does not report rootless mode, so it is not offered"
                .to_owned(),
        );
    }

    let secrets = probe.home_secret_folders();
    let user_ok = probe.is_elevated() == Some(false) && secrets.is_empty();
    if user_ok {
        offers.push(SandboxLevel::User);
    }
    if probe.is_elevated().is_none() {
        notes.push("could not tell whether this process is elevated".to_owned());
    }
    if !secrets.is_empty() {
        notes.push(format!(
            "the home folder of this account holds: {}",
            secrets.join(", ")
        ));
    }
    notes.push(
        "level user is a claim by the member: a program cannot see whether the account is separate from the member's own, or what the other accounts allow it to read"
            .to_owned(),
    );
    offers.push(SandboxLevel::None);

    let mut fix = None;
    let in_force = match requested {
        Some(SandboxLevel::Container) => {
            if runtime.is_some() {
                SandboxLevel::Container
            } else {
                fix = Some(
                    "install rootless podman (or rootless docker) and put it on PATH, or set SOMMELIER_SANDBOX=none"
                        .to_owned(),
                );
                SandboxLevel::None
            }
        }
        Some(SandboxLevel::User) => {
            if user_ok {
                SandboxLevel::User
            } else {
                fix = Some(
                    "run the supervisor as a separate standard user whose home holds nothing else (swarm/sandbox/README.md), or set SOMMELIER_SANDBOX=none"
                        .to_owned(),
                );
                SandboxLevel::None
            }
        }
        Some(SandboxLevel::None) | None => {
            if inside {
                SandboxLevel::Container
            } else {
                SandboxLevel::None
            }
        }
    };
    if inside {
        notes.push(
            "this process already runs inside a container, what that container mounts is not visible from here"
                .to_owned(),
        );
    }

    Ok(SandboxReport {
        in_force,
        requested,
        offers,
        runtime,
        inside_container: inside,
        notes,
        fix,
    })
}

/// The line `sommelier doctor` prints: `sandbox: <level in force> (<what it can offer>)`, plus the fix line.
pub fn doctor_line(report: &SandboxReport) -> String {
    let mut offers: Vec<String> = Vec::new();
    for level in &report.offers {
        match (level, &report.runtime) {
            (SandboxLevel::Container, Some(rt)) => offers.push(format!("container via {rt}")),
            _ => offers.push(level.as_str().to_owned()),
        }
    }
    let mut line = format!(
        "sandbox: {} (can offer: {})",
        report.in_force.as_str(),
        offers.join(", ")
    );
    if let Some(fix) = &report.fix {
        line.push_str(&format!(" | fix: {fix}"));
    }
    line
}

/// A mount of a host folder into the container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    /// An absolute path inside the container.
    pub container: String,
    pub readonly: bool,
}

/// The values of the hook contract (CP 5.4) that come from the task. The three paths and the build jobs are
/// set by `wrap` itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractEnv {
    pub task: String,
    pub attempt: u32,
    pub branch: String,
    pub model: String,
    pub effort: String,
    pub cargo_build_jobs: String,
}

/// Everything `wrap` needs. No clock, no environment, no file access.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxSpec {
    /// `podman` or `docker`.
    pub runtime: String,
    pub image: String,
    pub worktree: PathBuf,
    pub out_dir: PathBuf,
    pub prompt_file: PathBuf,
    /// The folder that holds the hook script. Mounted read only.
    pub hooks_dir: PathBuf,
    /// The common git folder of the clone that owns the worktree. A worktree has only a pointer to it.
    /// Mounted at the same path, so the pointer works. `None` when the worktree is self contained.
    pub git_dir: Option<PathBuf>,
    /// Toolchain cache folders (cargo registry and so on).
    pub cache_dirs: Vec<Mount>,
    /// The folders with the vendor tools' own logins, listed by the member.
    pub vendor_dirs: Vec<Mount>,
    pub env: ContractEnv,
}

const ENV_NAMES: [&str; 9] = [
    "SOMMELIER_TASK",
    "SOMMELIER_ATTEMPT",
    "SOMMELIER_BRANCH",
    "SOMMELIER_WORKTREE",
    "SOMMELIER_PROMPT",
    "SOMMELIER_MODEL",
    "SOMMELIER_EFFORT",
    "SOMMELIER_OUT",
    "CARGO_BUILD_JOBS",
];

/// The names of the environment the container gets, in order. Exactly the hook contract and nothing else.
pub fn contract_env_names() -> &'static [&'static str] {
    &ENV_NAMES
}

fn check_host_path(label: &str, p: &Path) -> Result<String, String> {
    let s = p.to_string_lossy().into_owned();
    if s.is_empty() {
        return Err(format!("sandbox: {label} is empty"));
    }
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        return Err(format!(
            "sandbox: {label} `{s}` contains a comma, a quote or a line break, which the mount syntax cannot carry: rename the folder"
        ));
    }
    Ok(s)
}

fn check_container_path(label: &str, p: &str) -> Result<(), String> {
    if !p.starts_with('/') || p.contains(',') || p.contains('"') || p.contains('\n') {
        return Err(format!(
            "sandbox: {label} `{p}` must be an absolute path inside the container without comma or quote"
        ));
    }
    if p == "/" || p.split('/').any(|c| c == "..") {
        return Err(format!("sandbox: {label} `{p}` is not a usable target"));
    }
    Ok(())
}

fn bind(src: &str, dst: &str, readonly: bool) -> Vec<String> {
    let mut v = format!("type=bind,src={src},dst={dst}");
    if readonly {
        v.push_str(",readonly");
    }
    vec!["--mount".to_owned(), v]
}

/// Builds the full command line that runs one launcher hook inside the container. `hook_command` is the
/// command as the container sees it, for example `["sh", "/hooks/hook-claude.sh"]`.
///
/// Mounted: the worktree (read write), the output folder (read write), the prompt file and the hooks folder
/// (read only), the git folder when given, the cache folders and the vendor login folders. Nothing else.
/// The container gets no home folder of the member, no SSH agent socket, no other repository, drops all
/// capabilities, may not gain new privileges and has a read only root with memory only `/tmp` and home.
/// The environment is exactly the hook contract. Tokens and the member key are never passed.
/// Network: left at the runtime default, so egress is NOT limited to a list of hosts.
pub fn wrap(hook_command: &[String], spec: &SandboxSpec) -> Result<Vec<String>, String> {
    if hook_command.is_empty() {
        return Err("sandbox: the hook command is empty".to_owned());
    }
    if spec.runtime != "podman" && spec.runtime != "docker" {
        return Err(format!(
            "sandbox: runtime `{}` is not supported: use podman or docker",
            spec.runtime
        ));
    }
    if spec.image.trim().is_empty() || spec.image.starts_with('-') || spec.image.contains(' ') {
        return Err(format!(
            "sandbox: image name `{}` is not usable: set SOMMELIER_SANDBOX_IMAGE",
            spec.image
        ));
    }

    let mut a: Vec<String> = vec![
        spec.runtime.clone(),
        "run".into(),
        "--rm".into(),
        "--read-only".into(),
        "--cap-drop=ALL".into(),
        "--security-opt=no-new-privileges".into(),
        "--pids-limit=4096".into(),
        "--pull=never".into(),
    ];
    if spec.runtime == "podman" {
        // The worker user owns the mounted worktree as the member does.
        a.push("--userns=keep-id".into());
    }
    a.push("--tmpfs".into());
    a.push("/tmp:rw,size=1g".into());
    a.push("--tmpfs".into());
    a.push(format!("{HOME_DIR}:rw,size=256m"));
    a.push("--workdir".into());
    a.push(WORK_DIR.into());

    a.extend(bind(
        &check_host_path("worktree", &spec.worktree)?,
        WORK_DIR,
        false,
    ));
    a.extend(bind(
        &check_host_path("output folder", &spec.out_dir)?,
        OUT_DIR,
        false,
    ));
    a.extend(bind(
        &check_host_path("prompt file", &spec.prompt_file)?,
        PROMPT_FILE,
        true,
    ));
    a.extend(bind(
        &check_host_path("hooks folder", &spec.hooks_dir)?,
        HOOKS_DIR,
        true,
    ));
    if let Some(git) = &spec.git_dir {
        let s = check_host_path("git folder", git)?;
        // Same path inside: the worktree's pointer file names the host path.
        a.extend(bind(&s, &s, false));
    }
    for m in &spec.cache_dirs {
        check_container_path("cache target", &m.container)?;
        a.extend(bind(
            &check_host_path("cache folder", &m.host)?,
            &m.container,
            m.readonly,
        ));
    }
    for m in &spec.vendor_dirs {
        check_container_path("vendor login target", &m.container)?;
        a.extend(bind(
            &check_host_path("vendor login folder", &m.host)?,
            &m.container,
            m.readonly,
        ));
    }

    let e = &spec.env;
    let values: BTreeMap<&str, String> = BTreeMap::from([
        ("SOMMELIER_TASK", e.task.clone()),
        ("SOMMELIER_ATTEMPT", e.attempt.to_string()),
        ("SOMMELIER_BRANCH", e.branch.clone()),
        ("SOMMELIER_WORKTREE", WORK_DIR.to_owned()),
        ("SOMMELIER_PROMPT", PROMPT_FILE.to_owned()),
        ("SOMMELIER_MODEL", e.model.clone()),
        ("SOMMELIER_EFFORT", e.effort.clone()),
        ("SOMMELIER_OUT", OUT_DIR.to_owned()),
        ("CARGO_BUILD_JOBS", e.cargo_build_jobs.clone()),
    ]);
    for name in ENV_NAMES {
        a.push("-e".into());
        a.push(format!("{name}={}", values[name]));
    }

    a.push(spec.image.clone());
    a.extend(hook_command.iter().cloned());
    Ok(a)
}

/// Reads the mounts a member lists in one variable. Entries are `HOST=CONTAINER` or `HOST=CONTAINER:ro`,
/// separated by `;`.
pub fn parse_mounts(text: &str) -> Result<Vec<Mount>, String> {
    let mut out = Vec::new();
    for entry in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (host, rest) = entry.split_once('=').ok_or_else(|| {
            format!("sandbox mount `{entry}` must look like HOST=/path/in/container")
        })?;
        let (target, readonly) = match rest.strip_suffix(":ro") {
            Some(t) => (t, true),
            None => (rest, false),
        };
        check_container_path("mount target", target)?;
        if host.trim().is_empty() {
            return Err(format!("sandbox mount `{entry}` has no host folder"));
        }
        out.push(Mount {
            host: PathBuf::from(host.trim()),
            container: target.to_owned(),
            readonly,
        });
    }
    Ok(out)
}

/// The real system. Slow calls have a short deadline.
pub struct SystemProbe;

fn run_with_deadline(program: &str, args: &[&str], limit: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Finds a program on PATH (with the usual Windows extensions).
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let cand = dir.join(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

impl SandboxProbe for SystemProbe {
    fn requested(&self) -> Option<String> {
        std::env::var("SOMMELIER_SANDBOX").ok()
    }

    fn has_command(&self, name: &str) -> bool {
        find_on_path(name).is_some()
    }

    fn runtime_rootless(&self, runtime: &str) -> Option<bool> {
        let limit = Duration::from_secs(8);
        match runtime {
            "podman" => run_with_deadline(
                "podman",
                &["info", "--format", "{{.Host.Security.Rootless}}"],
                limit,
            )
            .map(|s| s.eq_ignore_ascii_case("true")),
            "docker" => run_with_deadline(
                "docker",
                &["info", "--format", "{{.SecurityOptions}}"],
                limit,
            )
            .map(|s| s.contains("rootless")),
            _ => None,
        }
    }

    fn inside_container(&self) -> bool {
        Path::new("/.dockerenv").exists()
            || Path::new("/run/.containerenv").exists()
            || std::env::var_os("container").is_some()
    }

    fn is_elevated(&self) -> Option<bool> {
        if cfg!(windows) {
            // The high mandatory level SID is present in the token of an elevated process.
            let groups = run_with_deadline("whoami", &["/groups"], Duration::from_secs(8))?;
            Some(groups.contains("S-1-16-12288"))
        } else {
            let uid = run_with_deadline("id", &["-u"], Duration::from_secs(4))?;
            Some(uid == "0")
        }
    }

    fn home_secret_folders(&self) -> Vec<String> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let Some(home) = home else {
            return vec!["(home folder unknown)".to_owned()];
        };
        let candidates = [
            ".ssh",
            ".gnupg",
            ".aws",
            ".azure",
            ".kube",
            ".mozilla",
            ".config/google-chrome",
            ".config/chromium",
            "AppData/Local/Google/Chrome",
            "AppData/Local/Microsoft/Edge",
            "AppData/Roaming/Mozilla",
        ];
        candidates
            .iter()
            .filter(|c| home.join(c).exists())
            .map(|c| (*c).to_owned())
            .collect()
    }
}
