//! Settings of the intake. They live in `HubConfig.intake` and keep everything off by default.
//! Reading them from the policy file is a later wave.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::bundle::{BundleError, Limits};
use crate::gate::diff::ParsedDiff;
use crate::gate::{GateReport, GateRules};

/// The shape of `bundle::inspect`: the bundle file, the reference repository, the expected base and the limits.
pub type InspectCall =
    dyn Fn(&Path, &Path, &str, &Limits) -> Result<ParsedDiff, BundleError> + Send + Sync;

/// Replaces `bundle::inspect`, for example by a slow one in a test.
#[derive(Clone)]
pub struct InspectFn(pub Arc<InspectCall>);

impl fmt::Debug for InspectFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InspectFn")
    }
}

/// What a finished gate run hands to the next tick.
#[derive(Debug)]
pub(crate) struct GateDone {
    pub sha256: String,
    pub head: String,
    pub report: GateReport,
}

/// State that lives as long as the hub and is not part of the log.
#[derive(Debug, Default)]
pub(crate) struct Runtime {
    /// Held while an upload decides whether a hash is new, so two equal uploads write once.
    pub upload: Mutex<()>,
    /// Submissions with a gate run under way.
    pub in_flight: Mutex<HashSet<String>>,
    /// Gate runs that ended and wait for the next tick to become FACTs.
    pub done: Mutex<Vec<GateDone>>,
}

/// Settings of the intake of submissions. Lives in `HubConfig.intake`.
#[derive(Clone, Debug)]
pub struct IntakeConfig {
    /// Master switch. Off: the hub refuses `PUT /api/submit/` and runs no gate.
    pub enabled: bool,
    /// A local mirror of the project repository. The gates read the base commit from it. None: no
    /// gate run starts and a submission stays `received`.
    pub reference_repo: Option<PathBuf>,
    /// Where bundles and diffs are stored. None: `<data>/bundles`.
    pub store_dir: Option<PathBuf>,
    /// Hard limits of one unpacking.
    pub limits: Limits,
    /// The static gate rules.
    pub gate_rules: GateRules,
    /// The largest bundle in bytes by tier, index 0 to 3. A higher tier uses the last entry.
    pub tier_bundle_bytes: Vec<u64>,
    /// A test passes its own inspection here. None: `bundle::inspect`.
    pub inspector: Option<InspectFn>,
    pub(crate) runtime: Arc<Runtime>,
}

impl Default for IntakeConfig {
    fn default() -> Self {
        let rules = GateRules::default();
        Self {
            enabled: false,
            reference_repo: None,
            store_dir: None,
            limits: Limits::default(),
            tier_bundle_bytes: rules.caps.iter().map(|c| c.bundle_bytes).collect(),
            gate_rules: rules,
            inspector: None,
            runtime: Arc::default(),
        }
    }
}

impl IntakeConfig {
    /// The folder of bundles and diffs.
    pub fn store_path(&self, data_dir: &Path) -> PathBuf {
        self.store_dir
            .clone()
            .unwrap_or_else(|| data_dir.join("bundles"))
    }

    /// The largest bundle a member of `tier` may send.
    pub fn cap_for_tier(&self, tier: i64) -> u64 {
        let list = &self.tier_bundle_bytes;
        let i = usize::try_from(tier.max(0)).unwrap_or(0);
        list.get(i).or_else(|| list.last()).copied().unwrap_or(0)
    }

    /// The most a request body for `/api/submit/` may have: the cap of the highest tier.
    pub fn max_body_bytes(&self) -> u64 {
        self.tier_bundle_bytes.iter().copied().max().unwrap_or(0)
    }
}
