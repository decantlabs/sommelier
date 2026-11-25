//! Outside commands behind a small trait, so tests use a fake and the real system stays at the edge.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// What a finished command left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmdOutput {
    /// Exit code. None when a signal ended the process.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Why a command did not finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// The program does not exist on this machine.
    NotFound,
    /// The program could not be started or did not finish in time. The text says why.
    Failed(String),
}

/// Runs one outside program and returns what it printed.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CmdOutput, RunError>;
}

/// The real runner. It stops a command that runs longer than `timeout`.
#[derive(Clone, Debug)]
pub struct SystemRunner {
    pub timeout: Duration,
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
        }
    }
}

fn drain<R: Read + Send + 'static>(mut r: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<CmdOutput, RunError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => RunError::NotFound,
                _ => RunError::Failed(format!("cannot start {program}: {}", e.kind())),
            })?;
        let out = child.stdout.take().map(drain);
        let err = child.stderr.take().map(drain);
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if start.elapsed() < self.timeout => {
                    thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunError::Failed(format!(
                        "{program} did not finish within {} s",
                        self.timeout.as_secs()
                    )));
                }
                Err(e) => {
                    return Err(RunError::Failed(format!(
                        "cannot wait for {program}: {}",
                        e.kind()
                    )));
                }
            }
        };
        let join = |h: Option<thread::JoinHandle<String>>| {
            h.and_then(|h| h.join().ok()).unwrap_or_default()
        };
        Ok(CmdOutput {
            code: status.code(),
            stdout: join(out),
            stderr: join(err),
        })
    }
}
