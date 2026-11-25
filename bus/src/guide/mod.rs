//! Agent contract, role guides, loop recipes, and takeover pages (step 6).

pub mod doctor;
pub mod handover;
pub mod loops;
pub mod oneshot;
pub mod templates;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::policy::{DEFAULT_POLICY, Snapshot};

/// Maximum line count for any rendered role guide (section 8).
pub const MAX_GUIDE_LINES: usize = 40;

/// Maximum line count for the takeover page (section 13.4).
pub const MAX_HANDOVER_LINES: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuideRole {
    Worker,
    Lead,
    Coordinator,
    Brain,
    GuestLead,
}

impl GuideRole {
    pub const ALL: &[GuideRole] = &[
        GuideRole::Worker,
        GuideRole::Lead,
        GuideRole::Coordinator,
        GuideRole::Brain,
        GuideRole::GuestLead,
    ];

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "worker" => Ok(Self::Worker),
            "lead" => Ok(Self::Lead),
            "coordinator" => Ok(Self::Coordinator),
            "brain" => Ok(Self::Brain),
            "guest-lead" | "guest_lead" => Ok(Self::GuestLead),
            other => Err(format!(
                "unknown role `{other}`, expected one of: worker, lead, coordinator, brain, guest-lead"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Lead => "lead",
            Self::Coordinator => "coordinator",
            Self::Brain => "brain",
            Self::GuestLead => "guest-lead",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Claude,
    Codex,
    Gemini,
    Other,
}

impl Vendor {
    pub const ALL: &[Vendor] = &[Vendor::Claude, Vendor::Codex, Vendor::Gemini, Vendor::Other];

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "gemini" | "agy" => Ok(Self::Gemini),
            "other" => Ok(Self::Other),
            other => Err(format!(
                "unknown vendor `{other}`, expected one of: claude, codex, gemini, other"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Md,
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "md" | "markdown" => Ok(Self::Md),
            "json" => Ok(Self::Json),
            other => Err(format!("unknown format `{other}`, expected md or json")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Sh,
    Ps,
}

impl Shell {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sh" | "bash" => Ok(Self::Sh),
            "ps" | "powershell" | "pwsh" => Ok(Self::Ps),
            other => Err(format!("unknown shell `{other}`, expected sh or ps")),
        }
    }
}

/// Policy fields used by `guide` and `doctor`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyInfo {
    pub version: u64,
    pub hash: String,
    pub min_client: String,
    pub mode: String,
    pub lease_min: u64,
    pub offer_min: u64,
    pub checkpoint_min: u64,
    pub branch_pattern: String,
    pub guest_may_claim: Vec<String>,
    pub hold_paths: Vec<String>,
    pub role_rules: BTreeMap<String, Vec<String>>,
    pub classes: Vec<String>,
    pub disk_min_free_gb: u64,
    pub release_sha256: Option<String>,
}

impl PolicyInfo {
    pub fn short_hash(&self) -> &str {
        &self.hash[..8.min(self.hash.len())]
    }

    /// Loads policy from `--policy` if given, else `<data>/policy/swarm.policy.json`,
    /// else falls back to the compiled default policy.
    pub fn load(policy_path: Option<&Path>, data_dir: &Path) -> Result<Self, String> {
        if let Some(p) = policy_path {
            let text = fs::read_to_string(p)
                .map_err(|e| format!("cannot read policy file {}: {e}", p.display()))?;
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                return Self::from_json_str(&text);
            }
            return Self::from_toml_str(&text);
        }
        let cache = data_dir.join("policy").join("swarm.policy.json");
        match fs::read_to_string(&cache) {
            Ok(text) => Self::from_json_str(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Self::from_toml_str(DEFAULT_POLICY)
            }
            Err(e) => Err(format!(
                "cannot read cached policy {}: {e}",
                cache.display()
            )),
        }
    }

    pub fn from_toml_str(source: &str) -> Result<Self, String> {
        if let Ok(snap) = Snapshot::parse(source) {
            let resp = snap.response();
            return Ok(Self::from_canonical_value(&resp["policy"], snap.hash));
        }
        let val: Value = toml::from_str(source).map_err(|e| format!("invalid policy TOML: {e}"))?;
        let mut canonical = val;
        canonical.sort_all_objects();
        let hash = format!("{:x}", Sha256::digest(canonical.to_string().as_bytes()));
        Ok(Self::from_canonical_value(&canonical, hash))
    }

    pub fn from_json_str(source: &str) -> Result<Self, String> {
        let mut val: Value =
            serde_json::from_str(source).map_err(|e| format!("invalid policy JSON: {e}"))?;
        val.sort_all_objects();
        let hash = format!("{:x}", Sha256::digest(val.to_string().as_bytes()));
        Ok(Self::from_canonical_value(&val, hash))
    }

    fn from_canonical_value(v: &Value, hash: String) -> Self {
        let version = v["version"].as_u64().unwrap_or(1);
        let min_client = v["min_client"].as_str().unwrap_or("0.1.0").to_owned();
        let mode = v["mode"].as_str().unwrap_or("off").to_owned();
        let lease_min = v["timers"]["lease_min"].as_u64().unwrap_or(45);
        let offer_min = v["timers"]["offer_min"].as_u64().unwrap_or(5);
        let checkpoint_min = v["timers"]["checkpoint_min"].as_u64().unwrap_or(30);
        let branch_pattern = v["kind"]["fix"]["branch"]
            .as_str()
            .unwrap_or("{class}/{task}")
            .to_owned();
        let guest_may_claim = string_array(&v["trust"]["guest"]["may_claim"]);
        let hold_paths = string_array(&v["trust"]["guest"]["hold_paths"]);

        let mut role_rules = BTreeMap::new();
        if let Some(guide_map) = v["guide"].as_object() {
            for (role, entry) in guide_map {
                let rules = string_array(&entry["rules"]);
                if !rules.is_empty() {
                    role_rules.insert(role.clone(), rules);
                }
            }
        }

        let mut classes = Vec::new();
        if let Some(class_map) = v["class"].as_object() {
            for (name, cfg) in class_map {
                if cfg["spawn"].as_bool() != Some(false) {
                    classes.push(name.clone());
                }
            }
        }

        let disk_min_free_gb = v["disk"]["min_free_gb"].as_u64().unwrap_or(40);
        let os_key = if cfg!(windows) { "windows" } else { "linux" };
        let release_sha256 = v["release"]["sha256"][os_key].as_str().map(str::to_owned);

        Self {
            version,
            hash,
            min_client,
            mode,
            lease_min,
            offer_min,
            checkpoint_min,
            branch_pattern,
            guest_may_claim,
            hold_paths,
            role_rules,
            classes,
            disk_min_free_gb,
            release_sha256,
        }
    }
}

fn string_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
