//! Free disk space guard (sections 4.4, 5.3, 8.5, 9, and step 7).
//!
//! Checks available disk space before starting a worker, worktree, run or batch.
//! When free space is below `min_free_gb`, runs an optional cleanup hook and refuses
//! with exit code 3 (`REFUSED`) and a single-line reason and fix.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sysinfo::Disks;

/// Default minimum free disk space in GiB from `policy.disk.min_free_gb` (section 5.3).
pub const DEFAULT_MIN_FREE_GB: u64 = 40;
const GIB: u64 = 1024 * 1024 * 1024;

/// Configuration for the disk guard.
#[derive(Clone, Debug)]
pub struct DiskGuardConfig {
    /// Minimum free disk space in GiB required to proceed.
    pub min_free_gb: u64,
    /// Optional injected free bytes for deterministic testing without system calls.
    pub injected_free_bytes: Option<u64>,
    /// Optional cleanup hook script or executable to invoke when space is low.
    pub cleanup_hook: Option<PathBuf>,
}

impl Default for DiskGuardConfig {
    fn default() -> Self {
        Self {
            min_free_gb: DEFAULT_MIN_FREE_GB,
            injected_free_bytes: None,
            cleanup_hook: None,
        }
    }
}

impl DiskGuardConfig {
    pub fn with_min_free_gb(mut self, min_free_gb: u64) -> Self {
        self.min_free_gb = min_free_gb;
        self
    }

    pub fn with_injected_free_bytes(mut self, bytes: u64) -> Self {
        self.injected_free_bytes = Some(bytes);
        self
    }

    pub fn with_injected_free_gb(mut self, gb: u64) -> Self {
        self.injected_free_bytes = Some(gb.saturating_mul(GIB));
        self
    }

    pub fn with_cleanup_hook(mut self, hook: impl Into<PathBuf>) -> Self {
        self.cleanup_hook = Some(hook.into());
        self
    }
}

/// Outcome of a passing disk guard check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskStatus {
    pub free_bytes: u64,
    pub free_gb: u64,
    pub min_free_gb: u64,
    pub cleanup_ran: bool,
}

/// Refusal returned when free disk space is below the configured minimum.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskRefusal {
    pub code: u8,
    pub free_bytes: u64,
    pub free_gb: u64,
    pub min_free_gb: u64,
    pub cleanup_ran: bool,
    pub message: String,
}

impl std::fmt::Display for DiskRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Queries free disk bytes for the disk containing `path` using `sysinfo::Disks`.
pub fn system_free_bytes(path: &Path) -> u64 {
    let Ok(disks) = Disks::new_with_refreshed_list() else {
        return 0;
    };
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    let mut best_mount_len = 0usize;
    let mut best_avail: Option<u64> = None;

    for disk in &disks {
        let mount = disk.mount_point();
        let mount_canon = mount.canonicalize().unwrap_or_else(|_| mount.to_path_buf());
        if canonical.starts_with(&mount_canon) || path.starts_with(mount) {
            let len = mount.as_os_str().len();
            if len >= best_mount_len {
                best_mount_len = len;
                best_avail = Some(disk.available_space());
            }
        }
    }

    if let Some(avail) = best_avail {
        return avail;
    }

    disks
        .iter()
        .map(sysinfo::Disk::available_space)
        .max()
        .unwrap_or(0)
}

/// Resolves free bytes from config override, `SOMMELIER_INJECT_FREE_BYTES` / `SOMMELIER_INJECT_FREE_GB`,
/// or `sysinfo::Disks`.
pub fn resolve_free_bytes(path: &Path, cfg: &DiskGuardConfig) -> u64 {
    if let Some(b) = cfg.injected_free_bytes {
        return b;
    }
    if let Ok(s) = std::env::var("SOMMELIER_INJECT_FREE_BYTES")
        && let Ok(b) = s.trim().parse::<u64>()
    {
        return b;
    }
    if let Ok(s) = std::env::var("SOMMELIER_INJECT_FREE_GB")
        && let Ok(gb) = s.trim().parse::<u64>()
    {
        return gb.saturating_mul(GIB);
    }
    system_free_bytes(path)
}

/// Checks whether `path` has at least `cfg.min_free_gb` GiB free.
/// If below the threshold and `cleanup_hook` is set, runs the hook once and re-checks.
/// Returns `Err(DiskRefusal)` with `code == crate::exit::REFUSED` (3) when still below threshold.
pub fn check_disk(path: &Path, cfg: &DiskGuardConfig) -> Result<DiskStatus, DiskRefusal> {
    let min_bytes = cfg.min_free_gb.saturating_mul(GIB);
    let mut free_bytes = resolve_free_bytes(path, cfg);
    let mut cleanup_ran = false;

    if free_bytes < min_bytes
        && let Some(hook) = &cfg.cleanup_hook
        && hook.exists()
    {
        let _ = Command::new(hook).status();
        cleanup_ran = true;
        free_bytes = resolve_free_bytes(path, cfg);
    }

    let free_gb = free_bytes / GIB;
    if free_bytes < min_bytes {
        let message = format!(
            "DISK_LOW: disk free space is {free_gb} GiB, below minimum {} GiB, free disk space or run worktree sweep",
            cfg.min_free_gb
        );
        return Err(DiskRefusal {
            code: crate::exit::REFUSED,
            free_bytes,
            free_gb,
            min_free_gb: cfg.min_free_gb,
            cleanup_ran,
            message,
        });
    }

    Ok(DiskStatus {
        free_bytes,
        free_gb,
        min_free_gb: cfg.min_free_gb,
        cleanup_ran,
    })
}
