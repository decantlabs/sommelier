use std::collections::BTreeMap;
use std::fs::{self, File, FileTimes};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::msg::Role;
use crate::tokens::TokenFile;

pub const DEFAULT_POLICY: &str = include_str!("../policy/swarm.policy.toml");
pub const STALE: &str = "stale policy, run sommelier sync";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Off,
    Shadow,
    Enforce,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u64,
    pub min_client: Version,
    pub grace_min: u64,
    pub mode: Mode,
    timers: Timers,
    class: BTreeMap<String, Class>,
    trust: TrustTable,
    kind: BTreeMap<String, Kind>,
    priority: Priority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    join: Option<JoinRules>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    review: BTreeMap<String, ReviewRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verify: Option<VerifyRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    test: Option<TestRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    switch: Option<Switches>,
}

impl Policy {
    /// The settings of the public factory as one typed, read only view.
    /// Sections that the file does not carry give their defaults, which mean "off" or the outline numbers.
    pub fn factory(&self) -> FactoryRules {
        let tier = |n: usize| {
            self.trust.tiers[n]
                .clone()
                .unwrap_or_else(|| TierRules::for_tier(n))
        };
        FactoryRules {
            join: self.join.clone().unwrap_or_default(),
            tier0: tier(0),
            tier1: tier(1),
            tier2: tier(2),
            review: self.review.clone(),
            verify: self.verify.clone().unwrap_or_default(),
            test: self.test.clone().unwrap_or_default(),
            switch: self.switch.clone().unwrap_or_default(),
        }
    }

    fn validate(&self) -> io::Result<()> {
        let share = |field: &str, value: f64| {
            if (0.0..=1.0).contains(&value) {
                Ok(())
            } else {
                Err(invalid(format!("policy {field} must be between 0 and 1")))
            }
        };
        for (n, tier) in self.trust.tiers.iter().enumerate() {
            if let Some(tier) = tier {
                share(&format!("trust.tier{n}.verify_share"), tier.verify_share)?;
                share(&format!("trust.tier{n}.review_share"), tier.review_share)?;
            }
        }
        if let Some(join) = &self.join
            && join.proof_of_work_bits > 32
        {
            return Err(invalid(
                "policy join.proof_of_work_bits must be between 0 and 32",
            ));
        }
        for (name, rule) in &self.review {
            share(&format!("review.{name}.audit_share"), rule.audit_share)?;
            for (author, side) in [
                ("tier0_author", &rule.tier0_author),
                ("tier1_author", &rule.tier1_author),
            ] {
                if side.reviewers < 1 {
                    return Err(invalid(format!(
                        "policy review.{name}.{author}.reviewers must be at least 1"
                    )));
                }
                if side.min_tier > 3 {
                    return Err(invalid(format!(
                        "policy review.{name}.{author}.min_tier must be between 0 and 3"
                    )));
                }
            }
        }
        if !self.review.is_empty() && !self.review.contains_key("default") {
            return Err(invalid(
                "policy review needs a rule named default when any review rule is given",
            ));
        }
        if let Some(test) = &self.test {
            if test.quorum < 1 {
                return Err(invalid("policy test.quorum must be at least 1"));
            }
            if test.candidate_min_tier > 3 {
                return Err(invalid(
                    "policy test.candidate_min_tier must be between 0 and 3",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Timers {
    lease_min: u64,
    poll_min: u64,
    gone_min: u64,
    offer_min: u64,
    status_fresh_min: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Class {
    #[serde(skip_serializing_if = "Option::is_none")]
    cap_per_owner: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cap_per_family: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cap_per_machine: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    spawn: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<Budget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_pause_min: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lock: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Budget {
    meter: String,
    stop_at: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Trust {
    may_claim: Vec<String>,
    hold_paths: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Kind {
    required: Vec<String>,
    branch: String,
    min_tier: String,
    checks: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    clean_room: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Priority {
    levels: BTreeMap<String, i64>,
    aging_per_10min: u64,
    aging_cap: u64,
}

/// The tier numbers a trust table can name. Tier 3 is named in the policy by the owner and has no table.
const TIER_NAMES: [&str; 3] = ["tier0", "tier1", "tier2"];

/// The `trust` table. Role tables (`may_claim`, `hold_paths`) and tier tables live side by side.
/// Tier tables are only written back when the file carried them, so old files keep their hash.
#[derive(Debug, Default)]
struct TrustTable {
    guests: BTreeMap<String, Trust>,
    tiers: [Option<TierRules>; 3],
}

impl<'de> Deserialize<'de> for TrustTable {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = BTreeMap::<String, Value>::deserialize(deserializer)?;
        let mut table = Self::default();
        for (name, value) in raw {
            if let Some(n) = TIER_NAMES.iter().position(|t| *t == name) {
                let patch: TierPatch = serde_json::from_value(value)
                    .map_err(|e| D::Error::custom(format!("trust.{name}: {e}")))?;
                table.tiers[n] = Some(patch.apply(TierRules::for_tier(n)));
            } else {
                let trust: Trust = serde_json::from_value(value)
                    .map_err(|e| D::Error::custom(format!("trust.{name}: {e}")))?;
                table.guests.insert(name, trust);
            }
        }
        Ok(table)
    }
}

impl Serialize for TrustTable {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        for (name, trust) in &self.guests {
            map.serialize_entry(name, trust)?;
        }
        for (name, tier) in TIER_NAMES.iter().zip(&self.tiers) {
            if let Some(tier) = tier {
                map.serialize_entry(name, tier)?;
            }
        }
        map.end()
    }
}

/// What a tier needs to have earned: counts of stamps, the age of the key, the most reverts allowed.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reach {
    pub landed: u64,
    pub run_agreed: u64,
    pub review_confirmed: u64,
    pub key_age_days: u64,
    pub reverted_for_cause: u64,
}

/// Limits and rights of one tier (factory.md 5.1).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TierRules {
    pub leases: u64,
    pub submissions_per_day: u64,
    pub max_bundle_kib: u64,
    /// Share of verifier capacity for all keys of this tier together.
    pub verify_share: f64,
    /// Share of trusted review capacity for all keys of this tier together.
    pub review_share: f64,
    pub kinds: Vec<String>,
    pub free_text: bool,
    pub reach: Reach,
}

impl TierRules {
    /// The defaults of tier `n`. Tier 0 and the reach of tier 1 and 2 are the numbers of the outline.
    pub fn for_tier(n: usize) -> Self {
        let kinds = |k: &[&str]| k.iter().map(|s| (*s).to_owned()).collect();
        match n {
            0 => Self {
                leases: 1,
                submissions_per_day: 4,
                max_bundle_kib: 256,
                verify_share: 0.20,
                review_share: 0.20,
                kinds: kinds(&["run", "fix", "port"]),
                free_text: false,
                reach: Reach::default(),
            },
            1 => Self {
                leases: 3,
                submissions_per_day: 20,
                max_bundle_kib: 1024,
                verify_share: 1.0,
                review_share: 1.0,
                kinds: kinds(&["run", "fix", "port", "review", "scout"]),
                free_text: true,
                reach: Reach {
                    landed: 5,
                    run_agreed: 20,
                    review_confirmed: 0,
                    key_age_days: 7,
                    reverted_for_cause: 0,
                },
            },
            _ => Self {
                leases: 5,
                submissions_per_day: 50,
                max_bundle_kib: 4096,
                verify_share: 1.0,
                review_share: 1.0,
                kinds: kinds(&["run", "fix", "port", "review", "scout", "draft"]),
                free_text: true,
                reach: Reach {
                    landed: 25,
                    run_agreed: 0,
                    review_confirmed: 20,
                    key_age_days: 7,
                    reverted_for_cause: 0,
                },
            },
        }
    }
}

impl Default for TierRules {
    fn default() -> Self {
        Self::for_tier(0)
    }
}

/// A tier table as written in a file. Missing keys keep the default of the tier.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TierPatch {
    leases: Option<u64>,
    submissions_per_day: Option<u64>,
    max_bundle_kib: Option<u64>,
    verify_share: Option<f64>,
    review_share: Option<f64>,
    kinds: Option<Vec<String>>,
    free_text: Option<bool>,
    reach: Option<ReachPatch>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReachPatch {
    landed: Option<u64>,
    run_agreed: Option<u64>,
    review_confirmed: Option<u64>,
    key_age_days: Option<u64>,
    reverted_for_cause: Option<u64>,
}

impl TierPatch {
    fn apply(self, mut base: TierRules) -> TierRules {
        base.leases = self.leases.unwrap_or(base.leases);
        base.submissions_per_day = self.submissions_per_day.unwrap_or(base.submissions_per_day);
        base.max_bundle_kib = self.max_bundle_kib.unwrap_or(base.max_bundle_kib);
        base.verify_share = self.verify_share.unwrap_or(base.verify_share);
        base.review_share = self.review_share.unwrap_or(base.review_share);
        base.kinds = self.kinds.unwrap_or(base.kinds);
        base.free_text = self.free_text.unwrap_or(base.free_text);
        if let Some(r) = self.reach {
            base.reach = Reach {
                landed: r.landed.unwrap_or(base.reach.landed),
                run_agreed: r.run_agreed.unwrap_or(base.reach.run_agreed),
                review_confirmed: r.review_confirmed.unwrap_or(base.reach.review_confirmed),
                key_age_days: r.key_age_days.unwrap_or(base.reach.key_age_days),
                reverted_for_cause: r
                    .reverted_for_cause
                    .unwrap_or(base.reach.reverted_for_cause),
            };
        }
        base
    }
}

/// The front door for new keys (`[join]`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct JoinRules {
    pub enabled: bool,
    pub proof_of_work_bits: u64,
    pub per_block_per_hour: u64,
}

impl Default for JoinRules {
    fn default() -> Self {
        Self {
            enabled: false,
            proof_of_work_bits: 22,
            per_block_per_hour: 5,
        }
    }
}

/// Who reviews the work of an author of one tier.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorRule {
    pub reviewers: u64,
    pub min_tier: u64,
}

/// Whether a reviewer must come from another model family than the author.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OtherFamily {
    Ignore,
    Prefer,
    Require,
}

/// One review rule (`[review.<name>]`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewRules {
    #[serde(default = "tier0_author")]
    pub tier0_author: AuthorRule,
    #[serde(default = "tier1_author")]
    pub tier1_author: AuthorRule,
    #[serde(default = "audit_share")]
    pub audit_share: f64,
    #[serde(default = "other_family")]
    pub other_family: OtherFamily,
}

fn tier0_author() -> AuthorRule {
    AuthorRule {
        reviewers: 1,
        min_tier: 2,
    }
}

fn tier1_author() -> AuthorRule {
    AuthorRule {
        reviewers: 1,
        min_tier: 1,
    }
}

fn audit_share() -> f64 {
    0.10
}

fn other_family() -> OtherFamily {
    OtherFamily::Prefer
}

impl Default for ReviewRules {
    fn default() -> Self {
        Self {
            tier0_author: tier0_author(),
            tier1_author: tier1_author(),
            audit_share: audit_share(),
            other_family: other_family(),
        }
    }
}

/// The verifier workflow (`[verify]`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyRules {
    pub workflow: String,
    pub max_parallel: u64,
}

impl Default for VerifyRules {
    fn default() -> Self {
        Self {
            workflow: "factory-verify.yml".to_owned(),
            max_parallel: 16,
        }
    }
}

/// The test network (`[test]`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TestRules {
    pub quorum: u64,
    pub candidate_min_tier: u64,
}

impl Default for TestRules {
    fn default() -> Self {
        Self {
            quorum: 3,
            candidate_min_tier: 2,
        }
    }
}

/// The kill switches of the factory (`[switch]`), each off, shadow or enforce.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Switches {
    pub intake: Mode,
    pub tier0_coding: Mode,
    pub candidate_builds: Mode,
}

impl Default for Switches {
    fn default() -> Self {
        Self {
            intake: Mode::Off,
            tier0_coding: Mode::Off,
            candidate_builds: Mode::Off,
        }
    }
}

/// Every setting of the public factory, read only. `Default` is a file without factory sections.
#[derive(Clone, Debug, PartialEq)]
pub struct FactoryRules {
    pub join: JoinRules,
    pub tier0: TierRules,
    pub tier1: TierRules,
    pub tier2: TierRules,
    /// Review rules by name. Empty, or it holds `default`.
    pub review: BTreeMap<String, ReviewRules>,
    pub verify: VerifyRules,
    pub test: TestRules,
    pub switch: Switches,
}

impl Default for FactoryRules {
    fn default() -> Self {
        Self {
            join: JoinRules::default(),
            tier0: TierRules::for_tier(0),
            tier1: TierRules::for_tier(1),
            tier2: TierRules::for_tier(2),
            review: BTreeMap::new(),
            verify: VerifyRules::default(),
            test: TestRules::default(),
            switch: Switches::default(),
        }
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub policy: Policy,
    canonical: Value,
    pub hash: String,
    pub published: SystemTime,
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

impl Snapshot {
    pub fn parse(source: &str) -> io::Result<Self> {
        Self::new(toml::from_str(source).map_err(invalid)?)
    }

    fn new(policy: Policy) -> io::Result<Self> {
        policy.validate()?;
        let mut canonical = serde_json::to_value(&policy)?;
        canonical.sort_all_objects();
        let hash = format!("{:x}", Sha256::digest(canonical.to_string().as_bytes()));
        Ok(Self {
            policy,
            canonical,
            hash,
            published: SystemTime::UNIX_EPOCH,
        })
    }

    pub fn summary(&self) -> Value {
        json!({"version": self.policy.version, "mode": self.policy.mode, "hash": self.hash})
    }

    pub fn response(&self) -> Value {
        json!({"version": self.policy.version, "hash": self.hash, "policy": self.canonical})
    }

    pub fn verify(response: Value) -> io::Result<Self> {
        let policy: Policy = serde_json::from_value(response["policy"].clone())?;
        let snapshot = Self::new(policy)?;
        if response["hash"].as_str() != Some(&snapshot.hash)
            || response["version"].as_u64() != Some(snapshot.policy.version)
            || response["policy"] != snapshot.canonical
        {
            return Err(invalid("policy hash or version mismatch"));
        }
        Ok(snapshot)
    }

    pub fn check_hash(
        &self,
        sent: Option<&str>,
        now: SystemTime,
    ) -> Result<Option<&'static str>, &'static str> {
        if self.policy.mode == Mode::Off || sent == Some(&self.hash) {
            return Ok(None);
        }
        let grace = self.policy.grace_min.saturating_mul(60);
        let elapsed = now.duration_since(self.published).unwrap_or_default();
        if self.published != SystemTime::UNIX_EPOCH && elapsed.as_secs() < grace {
            Ok(Some(STALE))
        } else {
            Err(STALE)
        }
    }

    pub fn cached_hash(data: &Path) -> io::Result<Option<String>> {
        let path = data.join("policy/swarm.policy.json");
        match fs::read(path) {
            Ok(bytes) => Ok(Some(Self::new(serde_json::from_slice(&bytes)?)?.hash)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn write_cache(&self, data: &Path) -> io::Result<()> {
        let dir = data.join("policy");
        fs::create_dir_all(&dir)?;
        let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
        write!(temp, "{}", self.canonical)?;
        temp.as_file().sync_all()?;
        temp.persist(dir.join("swarm.policy.json"))?;
        Ok(())
    }
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(data: &Path) -> Self {
        Self {
            root: data.join("policy"),
        }
    }

    fn archive(&self, version: u64) -> PathBuf {
        self.root.join("versions").join(format!("{version}.toml"))
    }

    fn lock(&self) -> io::Result<File> {
        fs::create_dir_all(self.root.join("versions"))?;
        let file = File::options()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("publish.lock"))?;
        file.lock()?;
        Ok(file)
    }

    pub fn versions_after(&self, version: Option<u64>) -> io::Result<Vec<u64>> {
        let mut versions = Vec::new();
        for entry in fs::read_dir(self.root.join("versions"))? {
            let name = entry?.file_name();
            if let Some(n) = name
                .to_str()
                .and_then(|s| s.strip_suffix(".toml"))
                .and_then(|s| s.parse::<u64>().ok())
                && name == format!("{n}.toml").as_str()
                && version.is_none_or(|old| n > old)
            {
                versions.push(n);
            }
        }
        versions.sort_unstable();
        Ok(versions)
    }

    pub fn read_version(&self, version: u64) -> io::Result<Snapshot> {
        let path = self.archive(version);
        let mut snapshot = Snapshot::parse(&fs::read_to_string(&path)?)?;
        if snapshot.policy.version != version {
            return Err(invalid("policy version does not match its archive name"));
        }
        snapshot.published = fs::metadata(path)?.modified()?;
        Ok(snapshot)
    }

    fn write_version(&self, snapshot: &Snapshot, source: &str, baseline: bool) -> io::Result<()> {
        let path = self.archive(snapshot.policy.version);
        let mut file = tempfile::NamedTempFile::new_in(self.root.join("versions"))?;
        file.write_all(source.as_bytes())?;
        if baseline {
            file.as_file()
                .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))?;
        }
        file.as_file().sync_all()?;
        file.persist_noclobber(path)?;
        Ok(())
    }

    pub fn initialize(&self, source: Option<&Path>) -> io::Result<()> {
        let _lock = self.lock()?;
        let contents = match source {
            Some(path) => fs::read_to_string(path)?,
            None => match fs::read_to_string(self.root.join("swarm.policy.toml")) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => DEFAULT_POLICY.to_owned(),
                result => result?,
            },
        };
        let initial = Snapshot::parse(&contents)?;
        if self.versions_after(None)?.is_empty() {
            self.write_version(&initial, &contents, true)?;
        }
        Ok(())
    }

    pub fn current(&self) -> io::Result<Snapshot> {
        let version = self
            .versions_after(None)?
            .pop()
            .ok_or_else(|| invalid("hub policy is not initialized"))?;
        self.read_version(version)
    }

    /// Commit a local publication after checking the owner against the local token file.
    pub fn publish(data: &Path, token: &str, change: Change<'_>) -> io::Result<Snapshot> {
        require_local(data)?;
        if let Change::Publish(path) = change {
            require_local(path)?;
        }
        let data = data.canonicalize()?;
        require_local(&data)?;
        let store = Self::new(&data);
        require_local(&store.root.canonicalize()?)?;
        require_local(&store.root.join("versions").canonicalize()?)?;
        let tokens_path = data.join("tokens.json").canonicalize()?;
        require_local(&tokens_path)?;
        let _lock = store.lock()?;
        let tokens = TokenFile::load(&tokens_path)?;
        if !tokens
            .lookup(token)
            .is_some_and(|member| member.role == Role::Orchestrator)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "policy publication requires an orchestrator token",
            ));
        }
        let current = store.current()?.policy.version;
        let source = match change {
            Change::Publish(path) => {
                let path = path.canonicalize()?;
                require_local(&path)?;
                fs::read_to_string(path)?
            }
            Change::Rollback(version) => {
                let mut policy = store.read_version(version)?.policy;
                policy.version = current
                    .checked_add(1)
                    .ok_or_else(|| invalid("policy version overflow"))?;
                toml::to_string(&policy).map_err(invalid)?
            }
        };
        let snapshot = Snapshot::parse(&source)?;
        if snapshot.policy.version <= current {
            return Err(invalid(format!(
                "policy version must be higher than {current}"
            )));
        }
        store.write_version(&snapshot, &source, false)?;
        Ok(snapshot)
    }
}

pub enum Change<'a> {
    Publish(&'a Path),
    Rollback(u64),
}

pub fn require_local(path: &Path) -> io::Result<()> {
    let raw = path.as_os_str().to_string_lossy();
    if raw.contains("://")
        || raw.starts_with("//")
        || (raw.starts_with(r"\\") && !raw.starts_with(r"\\?\"))
    {
        return Err(invalid("policy publication requires local paths"));
    }
    #[cfg(windows)]
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next()
        && !matches!(
            prefix.kind(),
            std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
        )
    {
        return Err(invalid("policy publication requires local paths"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn schema_and_loader_agree() {
        let schema = serde_json::from_str(include_str!("../policy/schema.json")).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let snapshot = Snapshot::parse(DEFAULT_POLICY).unwrap();
        assert!(validator.is_valid(&snapshot.canonical));
        for (old, new) in [
            ("spawn = false", "unexpected = false"),
            ("mode = \"off\"", "mode = \"audit\""),
            ("min_client = \"0.1.0\"", "min_client = \"0.1\""),
        ] {
            let source = DEFAULT_POLICY.replace(old, new);
            let untyped: toml::Table = toml::from_str(&source).unwrap();
            assert!(!validator.is_valid(&serde_json::to_value(untyped).unwrap()));
            assert!(Snapshot::parse(&source).is_err());
        }
    }

    #[test]
    fn canonical_hash_is_stable_and_verified() {
        let original = Snapshot::parse(DEFAULT_POLICY).unwrap();
        let reordered = DEFAULT_POLICY
            .replace(
                "version = 1\nmin_client = \"0.1.0\"",
                "min_client = \"0.1.0\"\nversion = 1",
            )
            .replace(
                "meter = \"claude_week\", stop_at = 80",
                "stop_at = 80, meter = \"claude_week\"",
            );
        let reordered = Snapshot::parse(&reordered).unwrap();
        assert_eq!(original.hash, reordered.hash);
        assert_eq!(original.hash.len(), 64);
        let mut response = original.response();
        assert_eq!(
            Snapshot::verify(response.clone()).unwrap().hash,
            original.hash
        );
        response["policy"]["timers"]["lease_min"] = json!(25);
        assert!(Snapshot::verify(response).is_err());

        let client = tempfile::tempdir().unwrap();
        assert!(Snapshot::cached_hash(client.path()).unwrap().is_none());
        original.write_cache(client.path()).unwrap();
        assert_eq!(
            Snapshot::cached_hash(client.path()).unwrap(),
            Some(original.hash)
        );
    }

    #[test]
    fn stale_policy_warns_only_during_grace() {
        let source = DEFAULT_POLICY.replace("\"off\"", "\"shadow\"");
        let mut snapshot = Snapshot::parse(&source).unwrap();
        let now = SystemTime::now();
        assert_eq!(snapshot.check_hash(None, now), Err(STALE));
        snapshot.published = now;
        assert_eq!(
            snapshot.check_hash(Some("old"), now + Duration::from_secs(899)),
            Ok(Some(STALE))
        );
        assert_eq!(
            snapshot.check_hash(None, now + Duration::from_secs(900)),
            Err(STALE)
        );
        assert_eq!(snapshot.check_hash(Some(&snapshot.hash), now), Ok(None));
        assert_eq!(
            Snapshot::parse(DEFAULT_POLICY)
                .unwrap()
                .check_hash(None, now),
            Ok(None)
        );
    }

    #[test]
    fn local_publication_is_monotonic_and_rollback_keeps_history() {
        let data = tempfile::tempdir().unwrap();
        let store = Store::new(data.path());
        let source = data.path().join("candidate.toml");
        let mut tokens = TokenFile::default();
        let owner = tokens.add("owner", Role::Orchestrator).unwrap();
        let member = tokens.add("member", Role::Member).unwrap();
        tokens.save(&data.path().join("tokens.json")).unwrap();

        fs::write(
            &source,
            DEFAULT_POLICY.replace("version = 1", "version = 7"),
        )
        .unwrap();
        store.initialize(Some(&source)).unwrap();
        assert_eq!(
            store.current().unwrap().hash,
            Snapshot::parse(&fs::read_to_string(&source).unwrap())
                .unwrap()
                .hash
        );

        let publish = |token| Store::publish(data.path(), token, Change::Publish(&source));
        assert_eq!(
            publish(&member).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(publish(&owner).is_err());
        fs::write(&source, DEFAULT_POLICY).unwrap();
        assert!(publish(&owner).is_err());
        let next = DEFAULT_POLICY
            .replace("version = 1", "version = 9")
            .replace("\"off\"", "\"shadow\"");
        fs::write(&source, &next).unwrap();
        assert_eq!(publish(&owner).unwrap().policy.version, 9);
        assert_eq!(store.current().unwrap().policy.mode, Mode::Shadow);
        let rollback = Store::publish(data.path(), &owner, Change::Rollback(7)).unwrap();
        assert_eq!(rollback.policy.version, 10);
        assert_eq!(rollback.policy.mode, Mode::Off);
        assert_eq!(store.versions_after(None).unwrap(), [7, 9, 10]);
        assert_eq!(fs::read_to_string(store.archive(9)).unwrap(), next);

        for path in [
            "https://example.invalid/policy.toml",
            r"\\server\share",
            r"\\?\UNC\server\share",
        ] {
            let result = Store::publish(Path::new(path), &owner, Change::Rollback(7));
            assert!(
                result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("requires local paths")
            );
        }
    }
}
