//! Child process tracking, cross-platform liveness via `sysinfo`, and heartbeats (step 7).
//!
//! Tracks running supervised workers on the local machine in `<data_dir>/agent/children/<name>.json`.
//! Uses `sysinfo` on both Windows and Linux to verify process liveness (`pid_alive`) and terminate
//! children on `sommelier agent stop [NAME]`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};

/// Persistent record of one supervised worker child process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildRecord {
    pub name: String,
    pub class: String,
    pub model: String,
    pub branch: String,
    pub task_id: Option<String>,
    pub pid: u32,
    pub started_at_sec: u64,
    pub heartbeat_at_sec: u64,
    pub out_dir: Option<PathBuf>,
    pub worktree: Option<PathBuf>,
    pub state: String,
    pub text: String,
}

/// Live status of a tracked child process after probing the OS process table with `sysinfo`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildStatus {
    pub record: ChildRecord,
    pub pid_alive: bool,
}

/// Checks whether a process with `pid` is currently alive using the `sysinfo` crate.
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let sys_pid = Pid::from_u32(pid);
    let Ok(mut sys) = System::new() else {
        return false;
    };
    sys.refresh_processes(ProcessesToUpdate::Some(&[sys_pid]), true);
    sys.process(sys_pid).is_some()
}

/// Terminates a process with `pid` using `sysinfo` (works on Windows and Linux).
/// Returns `true` if the process was found and signalled, or was already gone.
pub fn kill_pid(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let sys_pid = Pid::from_u32(pid);
    let Ok(mut sys) = System::new() else {
        return false;
    };
    sys.refresh_processes(ProcessesToUpdate::Some(&[sys_pid]), true);
    match sys.process(sys_pid) {
        Some(proc_info) => proc_info.kill(),
        None => true,
    }
}

/// Directory store for local child process records (`<data_dir>/agent/children`).
#[derive(Clone, Debug)]
pub struct ChildrenStore {
    dir: PathBuf,
}

impl ChildrenStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("agent").join("children"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn record_path(&self, name: &str) -> PathBuf {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    fn stop_marker_path(&self, name: &str) -> PathBuf {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join(format!("{safe}.stop"))
    }

    /// Writes or updates a child record atomically.
    pub fn save(&self, record: &ChildRecord) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.record_path(&record.name);
        let mut temp = tempfile::NamedTempFile::new_in(&self.dir)?;
        let json = serde_json::to_string_pretty(record).map_err(io::Error::other)?;
        use std::io::Write;
        temp.write_all(json.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Loads a child record by worker name.
    pub fn get(&self, name: &str) -> io::Result<Option<ChildRecord>> {
        let path = self.record_path(name);
        match fs::read_to_string(path) {
            Ok(s) => {
                let rec = serde_json::from_str::<ChildRecord>(&s).map_err(io::Error::other)?;
                Ok(Some(rec))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Removes a child record and any stop marker for `name`.
    pub fn remove(&self, name: &str) -> io::Result<()> {
        let _ = fs::remove_file(self.stop_marker_path(name));
        match fs::remove_file(self.record_path(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Marks a worker as requested to stop by `sommelier agent stop` so `agent exec` classifies it as `killed`.
    pub fn mark_stopped(&self, name: &str) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        fs::write(self.stop_marker_path(name), b"stop\n")
    }

    /// Checks whether `sommelier agent stop` marked this worker for termination.
    pub fn is_marked_stopped(&self, name: &str) -> bool {
        self.stop_marker_path(name).is_file()
    }

    /// Lists all tracked child records and probes `sysinfo` for each PID's liveness.
    pub fn list_statuses(&self) -> io::Result<Vec<ChildStatus>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(s) = fs::read_to_string(&path)
                && let Ok(rec) = serde_json::from_str::<ChildRecord>(&s)
            {
                records.push(rec);
            }
        }
        records.sort_by(|a, b| a.name.cmp(&b.name));

        let pids: Vec<Pid> = records
            .iter()
            .filter(|r| r.pid > 0)
            .map(|r| Pid::from_u32(r.pid))
            .collect();
        let mut sys = System::new().ok();
        if let Some(s) = sys.as_mut()
            && !pids.is_empty()
        {
            s.refresh_processes(ProcessesToUpdate::Some(&pids), true);
        }

        let statuses = records
            .into_iter()
            .map(|record| {
                let pid_alive = record.pid > 0
                    && sys
                        .as_ref()
                        .is_some_and(|s| s.process(Pid::from_u32(record.pid)).is_some());
                ChildStatus { record, pid_alive }
            })
            .collect();
        Ok(statuses)
    }

    /// Stops one worker by name (or all workers when `name` is `None`).
    /// Marks each stopped worker so its wrapper records `ExitReason::Killed` and kills its OS process.
    pub fn stop(&self, name: Option<&str>) -> io::Result<Vec<String>> {
        let statuses = self.list_statuses()?;
        let mut stopped = Vec::new();
        for st in statuses {
            if let Some(target) = name
                && st.record.name != target
            {
                continue;
            }
            self.mark_stopped(&st.record.name)?;
            if st.pid_alive {
                let _ = kill_pid(st.record.pid);
            }
            stopped.push(st.record.name);
        }
        if let Some(target) = name
            && stopped.is_empty()
        {
            // Even if not currently in the table, write the stop marker if requested explicitly.
            self.mark_stopped(target)?;
        }
        Ok(stopped)
    }
}
