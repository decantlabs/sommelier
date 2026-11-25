//! Running git as a hardened child process with a hard time limit.
//!
//! Every git call of this module goes through [`git_command`]. The fixed argument list and the fixed
//! environment live here and nowhere else, so one test can prove them.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
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
    let mut cmd = Command::new("git");
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
    /// The time limit passed and the child was killed.
    TimedOut,
    /// The output grew past the cap and the child was killed.
    OutputTooLarge,
}

const STDERR_CAP: usize = 16 * 1024;

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

/// Run `cmd` with optional stdin bytes. The child is killed after `secs` seconds or when stdout passes
/// `max_out` bytes.
pub fn run_limited(
    mut cmd: Command,
    stdin: Option<Vec<u8>>,
    secs: u64,
    max_out: usize,
) -> Result<Run, RunError> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| RunError::Spawn(e.to_string()))?;
    let over = Arc::new(AtomicBool::new(false));
    let out_h = child.stdout.take().map(|s| {
        let o = over.clone();
        thread::spawn(move || drain(s, max_out, o))
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
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut failure = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(e) => return Err(RunError::Spawn(e.to_string())),
        }
        if over.load(Ordering::SeqCst) {
            failure = Some(RunError::OutputTooLarge);
        } else if Instant::now() >= deadline {
            failure = Some(RunError::TimedOut);
        }
        if failure.is_some() {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(5));
    };
    if let Some(f) = failure {
        // The reader threads end by themselves when the pipes close.
        return Err(f);
    }
    let stdout = out_h.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_h.and_then(|h| h.join().ok()).unwrap_or_default();
    if over.load(Ordering::SeqCst) {
        return Err(RunError::OutputTooLarge);
    }
    Ok(Run {
        ok: status.is_some_and(|s| s.success()),
        stdout,
        stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
    })
}
