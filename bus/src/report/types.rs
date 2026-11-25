//! The test report. `tester` builds it, the report module checks and projects it.
//!
//! Shared type file of the spine. Its owner is the report module. Other modules use it as it is.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The hardware and system class of a test machine. Free text fields are lower case and trimmed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineClass {
    /// `windows` or `linux`.
    pub os: String,
    pub os_build: String,
    /// GPU model as the driver names it, for example `rtx 4080`.
    pub gpu: String,
    pub gpu_memory_mb: u32,
    pub driver: String,
    pub memory_mb: u32,
}

/// What a run showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Every milestone of the scenario was reached.
    Works,
    /// A milestone was missed or the title ended early.
    Fails,
    /// The run says nothing about the build (machine paused, title missing, harness error).
    Inconclusive,
}

/// One signed test report. It never holds title data, a path with a user folder or an account name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Format version, 1.
    pub v: u32,
    /// Id of the run order this report answers.
    pub run: String,
    /// Member id of the tester.
    pub member: String,
    /// Commit of the build that ran.
    pub build: String,
    /// `main` or `candidate`.
    pub channel: String,
    /// Id of the title profile.
    pub profile: String,
    pub scenario: String,
    pub machine: MachineClass,
    pub outcome: Outcome,
    /// Short class of the first error, for example `device_removed`. None when the run works.
    pub first_error: Option<String>,
    /// Metric name to value, for example `fps_median`.
    pub metrics: BTreeMap<String, f64>,
    pub secs_alive: u32,
    /// Scrubbed tail of the log, at most 16 KiB.
    pub log_excerpt: String,
    /// UTC epoch seconds when the run ended.
    pub at: u64,
    /// Signature of the member over the canonical form of this report without this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}
