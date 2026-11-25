//! Running git as a hardened child process with hard limits.
//!
//! Every git call of this module goes through [`git_command`]. The fixed argument list and the fixed
//! environment live here and nowhere else, so one test can prove them.
//!
//! What the limits cover. The child gets a time limit, an output cap and a disk cap for its working
//! folder. When one is hit the WHOLE process tree is ended, not only the direct child: on Unix the child
//! starts in its own process group and the group gets the signal, on Windows the system tool
//! `taskkill /T` ends the tree. On Unix the child also starts through a shell with a memory limit
//! (`ulimit -v`). The shell command line is fixed text. The limit is a number from [`crate::bundle::Limits`]
//! and nothing from the bundle ever reaches it. Git's own memory is bounded in [`git_args`] by the
//! window, delta cache and thread settings.
//!
//! What is NOT covered on Windows: there is no memory limit, and `taskkill /T` follows the parent links
//! of processes, so a grandchild that was left behind by an already ended parent escapes it. The hub
//! server of the project is Linux and this module is built for it. Windows only keeps developer
//! machines working.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The null device for config files.
pub fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Fixed environment pairs of every git call. `PATH`, `HOME` and the Windows system root are added by
/// [`git_command`]. Nothing else of the parent environment is passed on.
pub fn git_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", null_device()),
        ("GIT_CONFIG_SYSTEM", null_device()),
        ("GIT_CONFIG_COUNT", "0"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_PAGER", "cat"),
        ("PAGER", "cat"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
        ("LC_ALL", "C"),
    ]
}

/// Fixed arguments in front of every git subcommand. `git_dir` is the temporary bare repository and
/// `hooks_dir` an empty folder that stands in for the hooks folder.
pub fn git_args(git_dir: Option<&Path>, hooks_dir: Option<&Path>) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec!["--no-pager".into()];
    if let Some(d) = git_dir {
        a.push("--git-dir".into());
        a.push(d.as_os_str().to_owned());
    }
    let mut cfg: Vec<String> = vec![
        "core.fsmonitor=false".into(),
        "core.quotepath=false".into(),
        "core.protectNTFS=true".into(),
        "core.protectHFS=true".into(),
        format!("core.attributesFile={}", null_device()),
        "color.ui=false".into(),
        "gc.auto=0".into(),
        "maintenance.auto=false".into(),
        "fetch.fsckObjects=true".into(),
        "transfer.fsckObjects=true".into(),
        "receive.fsckObjects=true".into(),
        "protocol.ext.allow=never".into(),
        "credential.helper=".into(),
        // Bounds for git's own memory. One thread, small windows and caches, and big blobs are
        // streamed instead of held in memory. `Sandbox` raises the threshold to the blob limit.
        "pack.threads=1".into(),
        "pack.windowMemory=64m".into(),
        "pack.deltaCacheSize=32m".into(),
        "core.deltaBaseCacheLimit=32m".into(),
        "core.packedGitWindowSize=16m".into(),
        "core.packedGitLimit=128m".into(),
        "core.bigFileThreshold=2m".into(),
        "index.threads=1".into(),
        "fetch.unpackLimit=1".into(),
        "transfer.unpackLimit=1".into(),
    ];
    if let Some(h) = hooks_dir {
        cfg.push(format!("core.hooksPath={}", h.display()));
    }
    for c in cfg {
        a.push("-c".into());
        a.push(c.into());
    }
    a
}

/// Flags that every diff and log call carries: no external diff program, no text conversion, no colour.
pub fn diff_flags() -> [&'static str; 4] {
    [
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-abbrev",
    ]
}

/// A git command with the fixed arguments and the cleared environment. `home` is an empty folder.
pub fn git_command(git_dir: Option<&Path>, hooks_dir: Option<&Path>, home: &Path) -> Command {
    git_command_capped(git_dir, hooks_dir, home, 0)
}

/// Like [`git_command`] with a memory cap of `mem_mb` megabytes for the whole process (address space).
/// 0 means no cap. The cap works on Unix only. There the command is `sh -c <fixed text> git ARGS...`
/// and the fixed text sets the limit and then replaces the shell with git. The arguments reach git as
/// `"$@"`, they are never part of the shell text.
pub fn git_command_capped(
    git_dir: Option<&Path>,
    hooks_dir: Option<&Path>,
    home: &Path,
    mem_mb: u64,
) -> Command {
    let mut cmd = program(mem_mb);
    cmd.args(git_args(git_dir, hooks_dir));
    cmd.env_clear();
    for (k, v) in git_env() {
        cmd.env(k, v);
    }
    for k in ["PATH", "SystemRoot", "SYSTEMROOT"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    cmd.env("HOME", home);
    cmd.env("USERPROFILE", home);
    cmd.env("XDG_CONFIG_HOME", home);
    cmd
}

#[cfg(unix)]
fn program(mem_mb: u64) -> Command {
    if mem_mb == 0 {
        return Command::new("git");
    }
    // A limit that cannot be set means the hard limit is lower already, which is stricter. So go on.
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(format!(
            "ulimit -v {} 2>/dev/null; exec git \"$@\"",
            mem_mb.saturating_mul(1024)
        ))
        .arg("git");
    c
}

#[cfg(not(unix))]
fn program(_mem_mb: u64) -> Command {
    Command::new("git")
}

/// What a finished child produced.
#[derive(Debug)]
pub struct Run {
    /// True when the exit status was success.
    pub ok: bool,
    pub stdout: Vec<u8>,
    /// The first part of stderr, lossy.
    pub stderr: String,
}

/// Why a child did not finish normally.
#[derive(Debug, PartialEq, Eq)]
pub enum RunError {
    /// The program could not be started.
    Spawn(String),
    /// The time limit passed and the process tree was killed.
    TimedOut,
    /// The output grew past the cap and the process tree was killed.
    OutputTooLarge,
    /// The watched folder grew past its cap and the process tree was killed.
    DiskTooLarge,
}

/// The limits of one child.
#[derive(Clone, Debug)]
pub struct RunLimits {
    pub secs: u64,
    pub max_out: usize,
    /// A folder whose total size is watched while the child runs.
    pub watch_dir: Option<PathBuf>,
    /// Bytes the watched folder may hold.
    pub max_disk: u64,
}

const STDERR_CAP: usize = 16 * 1024;
const POLL: Duration = Duration::from_millis(5);
const DISK_EVERY: u32 = 40;

fn drain(mut r: impl Read, cap: usize, over: Arc<AtomicBool>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() + n > cap {
                    out.extend_from_slice(&buf[..cap.saturating_sub(out.len())]);
                    over.store(true, Ordering::SeqCst);
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
        }
    }
    out
}

/// True when the files under `root` hold more than `cap` bytes. Links are not followed.
fn folder_over(root: &Path, cap: u64) -> bool {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total = total.saturating_add(m.len());
                if total > cap {
                    return true;
                }
            }
        }
    }
    false
}

/// End the child with the given process id and everything below it. Best effort: the caller still
/// kills the direct child by handle.
fn kill_tree(pid: u32) {
    let quiet = |mut c: Command| {
        let _ = c
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    if cfg!(unix) {
        // The child leads its own process group, so the group id is its pid. Fixed text, one number.
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg("kill -s KILL -- \"-$0\" 2>/dev/null || /bin/kill -s KILL -- \"-$0\"")
            .arg(pid.to_string());
        quiet(c);
    } else {
        let tool = std::env::var_os("SystemRoot")
            .map(|r| PathBuf::from(r).join("System32").join("taskkill.exe"))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("taskkill"));
        let mut c = Command::new(tool);
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        quiet(c);
    }
}

/// Wait for a reader thread until `deadline`. `None` when it did not finish.
fn join_by(h: Option<JoinHandle<Vec<u8>>>, deadline: Instant) -> Option<Vec<u8>> {
    let Some(h) = h else {
        return Some(Vec::new());
    };
    while !h.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(POLL);
    }
    h.join().ok()
}

/// Run `cmd` with optional stdin bytes. The process tree is killed after `secs` seconds or when stdout
/// passes `max_out` bytes.
pub fn run_limited(
    cmd: Command,
    stdin: Option<Vec<u8>>,
    secs: u64,
    max_out: usize,
) -> Result<Run, RunError> {
    run_guarded(
        cmd,
        stdin,
        &RunLimits {
            secs,
            max_out,
            watch_dir: None,
            max_disk: 0,
        },
    )
}

/// Run `cmd` under all limits of [`RunLimits`]. Every failure ends the whole process tree.
pub fn run_guarded(
    mut cmd: Command,
    stdin: Option<Vec<u8>>,
    lim: &RunLimits,
) -> Result<Run, RunError> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| RunError::Spawn(e.to_string()))?;
    let pid = child.id();
    let over = Arc::new(AtomicBool::new(false));
    let out_h = child.stdout.take().map(|s| {
        let o = over.clone();
        let cap = lim.max_out;
        thread::spawn(move || drain(s, cap, o))
    });
    let err_h = child.stderr.take().map(|s| {
        let o = Arc::new(AtomicBool::new(false));
        thread::spawn(move || drain(s, STDERR_CAP, o))
    });
    if let (Some(mut si), Some(data)) = (child.stdin.take(), stdin) {
        thread::spawn(move || {
            let _ = si.write_all(&data);
        });
    }
    let deadline = Instant::now() + Duration::from_secs(lim.secs);
    let mut failure = None;
    let mut tick = 0u32;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(e) => {
                kill_tree(pid);
                let _ = child.kill();
                return Err(RunError::Spawn(e.to_string()));
            }
        }
        if over.load(Ordering::SeqCst) {
            failure = Some(RunError::OutputTooLarge);
        } else if Instant::now() >= deadline {
            failure = Some(RunError::TimedOut);
        } else if let Some(dir) = &lim.watch_dir {
            tick += 1;
            if tick.is_multiple_of(DISK_EVERY) && folder_over(dir, lim.max_disk) {
                failure = Some(RunError::DiskTooLarge);
            }
        }
        if failure.is_some() {
            kill_tree(pid);
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(POLL);
    };
    if let Some(f) = failure {
        // The reader threads end by themselves when the pipes close.
        return Err(f);
    }
    // The child is gone. A grandchild may still hold the pipes: wait for the readers until the
    // deadline, then end what is left of the group.
    let (Some(stdout), Some(stderr)) = (join_by(out_h, deadline), join_by(err_h, deadline)) else {
        kill_tree(pid);
        return Err(RunError::TimedOut);
    };
    if over.load(Ordering::SeqCst) {
        return Err(RunError::OutputTooLarge);
    }
    Ok(Run {
        ok: status.is_some_and(|s| s.success()),
        stdout,
        stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
    })
}
