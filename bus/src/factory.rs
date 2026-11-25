//! The settings of the public factory on a real hub: `sommelier serve --factory FILE`.
//!
//! The file is local to the hub. It holds paths and addresses and is never published. Each table
//! switches one part on and gives its local values. The numbers that are policy (tier rules, the review
//! rule, the audit share, verify limits, the quorum, the proof of work bits) come from the published
//! policy of the hub, see [`crate::policy::FactoryRules`]. A policy `[switch]` that says `off` vetoes a
//! part even when this file enables it. Without `--factory` nothing here runs and the hub is as before.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::hub::HubConfig;
use crate::policy::{FactoryRules, Mode, OtherFamily as PolicyFamily, Policy};
use crate::review::pick::{OtherFamily, ReviewRule};
use crate::stamp::TierRules;

/// The contents of the factory file. Every table and every key is optional, an unknown key is refused.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FactoryFile {
    pub join: JoinFile,
    pub intake: IntakeFile,
    pub verify: VerifyFile,
    pub review: ReviewFile,
    pub stamp: StampFile,
    pub test: TestFile,
    pub door: DoorFile,
}

/// `[join]`: whether new keys may join. The proof of work comes from the policy.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JoinFile {
    pub enabled: bool,
}

/// `[intake]`: bundles from members.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntakeFile {
    pub enabled: bool,
    /// A local mirror of the project repository that the gates read the base commit from.
    pub reference_repo: Option<PathBuf>,
    /// Where bundles and diffs are stored. Default `<data>/bundles`.
    pub bundle_store: Option<PathBuf>,
}

/// `[verify]`: the clean build on GitHub.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyFile {
    pub enabled: bool,
    /// The GitHub repository, `owner/name`.
    pub repository: Option<String>,
    /// The workflow file. Default: the name in the policy `[verify]`.
    pub workflow: Option<String>,
    /// The git ref the workflow runs from. Default `main`.
    pub git_ref: Option<String>,
    /// A file that holds the GitHub token. Read at the first run, never logged.
    pub token_file: Option<PathBuf>,
    /// The public address of the hub, `https://...`, which the runner fetches bundles from.
    pub public_address: Option<String>,
}

/// `[review]`: assigned review.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReviewFile {
    pub enabled: bool,
    pub answer_secs: Option<u64>,
    pub online_secs: Option<u64>,
}

/// `[stamp]`: standing, stamps and tiers.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StampFile {
    pub enabled: bool,
    /// Maintainers: names of tokens and ids of key members.
    pub maintainers: Vec<String>,
    pub confirm_days: Option<u64>,
    /// Whether a tier 0 member may take and deliver orders. The policy switch can still veto it.
    pub tier0_coding: bool,
}

/// `[test]`: the test network.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestFile {
    pub enabled: bool,
    pub max_handed_per_member: Option<usize>,
    pub hand_hold_secs: Option<u64>,
    pub order_ttl_secs: Option<u64>,
    pub max_open_orders: Option<usize>,
}

/// `[door]`: the front door. No `enabled`: every key left out keeps the default of the door.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DoorFile {
    pub trust_header: Option<String>,
    pub joins_per_block_per_hour: Option<u32>,
    pub conns_per_member: Option<u32>,
    pub max_conns: Option<u32>,
    pub reserved_share: Option<f64>,
    pub public_read: Option<bool>,
    pub public_reads_per_block_per_min: Option<u32>,
    pub keep_days: Option<u32>,
    pub join_flood_refusals: Option<u32>,
    pub join_flood_window_secs: Option<u64>,
}

impl DoorFile {
    fn is_set(&self) -> bool {
        self.trust_header.is_some()
            || self.joins_per_block_per_hour.is_some()
            || self.conns_per_member.is_some()
            || self.max_conns.is_some()
            || self.reserved_share.is_some()
            || self.public_read.is_some()
            || self.public_reads_per_block_per_min.is_some()
            || self.keep_days.is_some()
            || self.join_flood_refusals.is_some()
            || self.join_flood_window_secs.is_some()
    }
}

/// Reads and parses the factory file. The error is one line that names the file and the key.
pub fn load(path: &Path) -> Result<FactoryFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "--factory {}: cannot read the file: {}",
            path.display(),
            e.kind()
        )
    })?;
    parse(&text).map_err(|e| format!("--factory {}: {e}", path.display()))
}

/// Parses the text of a factory file.
pub fn parse(text: &str) -> Result<FactoryFile, String> {
    toml::from_str(text).map_err(|e| {
        // The full error quotes the file. The message and the line number are enough.
        match e.span() {
            Some(span) => {
                let line = text[..span.start.min(text.len())].matches('\n').count() + 1;
                format!("line {line}: {}", e.message())
            }
            None => e.message().to_owned(),
        }
    })
}

/// Which parts ended up on, and which a policy switch vetoed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub join: bool,
    pub intake: bool,
    pub verify: bool,
    pub review: bool,
    pub stamp: bool,
    pub test: bool,
    pub door: bool,
    /// Names of the parts that the file enables and a policy switch turned off.
    pub vetoed: Vec<&'static str>,
}

impl Applied {
    /// The line printed at start: `factory: join on, intake on, verify off, ...`.
    pub fn line(&self) -> String {
        let one = |name: &str, on: bool| {
            let state = if on { "on" } else { "off" };
            if self.vetoed.contains(&name) {
                format!("{name} off (policy switch)")
            } else {
                format!("{name} {state}")
            }
        };
        let parts = [
            one("join", self.join),
            one("intake", self.intake),
            one("verify", self.verify),
            one("review", self.review),
            one("stamp", self.stamp),
            one("test", self.test),
            one("door", self.door),
        ];
        format!("factory: {}", parts.join(", "))
    }
}

/// The tier rules of the stamp part from the policy tables.
fn tier_rules(rules: &FactoryRules) -> TierRules {
    let base = TierRules::default();
    TierRules {
        tier1_key_age_days: rules.tier1.reach.key_age_days,
        tier1_penalty_window_days: base.tier1_penalty_window_days,
        tier1_landed: rules.tier1.reach.landed,
        tier1_run_agreed: rules.tier1.reach.run_agreed,
        tier2_landed: rules.tier2.reach.landed,
        tier2_review_confirmed: rules.tier2.reach.review_confirmed,
    }
}

/// The review rule from the policy rule named `default`, or the defaults of the picker.
fn review_rule(rules: &FactoryRules) -> (ReviewRule, f64) {
    let mut rule = ReviewRule::default();
    let mut audit = crate::policy::ReviewRules::default().audit_share;
    if let Some(p) = rules.review.get("default") {
        let n = |v: u64| usize::try_from(v).unwrap_or(1).max(1);
        rule.reviewers[0] = n(p.tier0_author.reviewers);
        rule.reviewers[1] = n(p.tier1_author.reviewers);
        rule.min_tier[0] = u8::try_from(p.tier0_author.min_tier).unwrap_or(3);
        rule.min_tier[1] = u8::try_from(p.tier1_author.min_tier).unwrap_or(3);
        // The picker knows prefer and ignore. `require` is read as `prefer`.
        rule.other_family = match p.other_family {
            PolicyFamily::Ignore => OtherFamily::Ignore,
            PolicyFamily::Prefer | PolicyFamily::Require => OtherFamily::Prefer,
        };
        audit = p.audit_share;
    }
    (rule, audit)
}

/// Writes the settings of the file and of the policy into `cfg`. Flags of the command line are applied
/// by the caller afterwards. `switch_table` says whether the policy has a `[switch]` table: without one
/// nothing can veto.
pub fn apply(
    cfg: &mut HubConfig,
    file: &FactoryFile,
    rules: &FactoryRules,
    switch_table: bool,
) -> Result<Applied, String> {
    let mut out = Applied::default();
    let vetoed = |mode: Mode| switch_table && mode == Mode::Off;

    // join
    cfg.join.enabled = file.join.enabled;
    cfg.join.proof_of_work_bits = u32::try_from(rules.join.proof_of_work_bits).unwrap_or(32);
    out.join = cfg.join.enabled;

    // intake
    let mut intake_on = file.intake.enabled;
    if intake_on && vetoed(rules.switch.intake) {
        intake_on = false;
        out.vetoed.push("intake");
    }
    cfg.intake.enabled = intake_on;
    cfg.intake.reference_repo = file.intake.reference_repo.clone();
    cfg.intake.store_dir = file.intake.bundle_store.clone();
    cfg.intake.tier_bundle_bytes = [&rules.tier0, &rules.tier1, &rules.tier2, &rules.tier2]
        .iter()
        .map(|t| t.max_bundle_kib.saturating_mul(1024))
        .collect();
    out.intake = intake_on;

    // verify
    let v = &file.verify;
    cfg.verify.enabled = v.enabled;
    cfg.verify.repository = v.repository.clone().unwrap_or_default();
    cfg.verify.workflow_file = v
        .workflow
        .clone()
        .unwrap_or_else(|| rules.verify.workflow.clone());
    if let Some(r) = &v.git_ref {
        cfg.verify.git_ref = r.clone();
    }
    cfg.verify.token_file = v.token_file.clone();
    cfg.verify.public_address = v.public_address.clone().unwrap_or_default();
    cfg.verify.rules.max_concurrent = usize::try_from(rules.verify.max_parallel).unwrap_or(16);
    cfg.verify.rules.tier0_share_percent = (rules.tier0.verify_share * 100.0).round() as u32;
    if v.enabled && (cfg.verify.repository.is_empty() || cfg.verify.public_address.is_empty()) {
        return Err(
            "[verify] enabled needs `repository` (owner/name) and `public_address` (https://...)"
                .into(),
        );
    }
    out.verify = v.enabled;

    // review
    let (rule, audit) = review_rule(rules);
    cfg.review.enabled = file.review.enabled;
    cfg.review.rule = rule;
    cfg.review.audit_share = audit;
    if let Some(s) = file.review.answer_secs {
        cfg.review.answer_secs = s;
    }
    if let Some(s) = file.review.online_secs {
        cfg.review.online_secs = s;
    }
    out.review = file.review.enabled;

    // stamp
    cfg.stamp.enabled = file.stamp.enabled;
    cfg.stamp.rules = tier_rules(rules);
    cfg.stamp.maintainers = file.stamp.maintainers.clone();
    if let Some(d) = file.stamp.confirm_days {
        cfg.stamp.confirm_days = d;
    }
    cfg.stamp.quorum = usize::try_from(rules.test.quorum).unwrap_or(3).max(1);
    let mut coding = file.stamp.tier0_coding;
    if coding && vetoed(rules.switch.tier0_coding) {
        coding = false;
        out.vetoed.push("tier0 coding");
    }
    cfg.stamp.tier0_coding = coding;
    out.stamp = file.stamp.enabled;

    // test
    cfg.test.enabled = file.test.enabled;
    cfg.test.rules.quorum = cfg.stamp.quorum;
    if let Some(n) = file.test.max_handed_per_member {
        cfg.test.max_handed_per_member = n;
    }
    if let Some(n) = file.test.hand_hold_secs {
        cfg.test.hand_hold_secs = n;
    }
    if let Some(n) = file.test.order_ttl_secs {
        cfg.test.order_ttl_secs = n;
    }
    if let Some(n) = file.test.max_open_orders {
        cfg.test.max_open_orders = n;
    }
    out.test = file.test.enabled;

    // door
    let d = &file.door;
    let door = &mut cfg.door;
    door.trust_header = d.trust_header.clone();
    door.joins_per_block_per_hour = d
        .joins_per_block_per_hour
        .unwrap_or_else(|| u32::try_from(rules.join.per_block_per_hour).unwrap_or(u32::MAX));
    if let Some(n) = d.conns_per_member {
        door.conns_per_member = n;
    }
    if let Some(n) = d.max_conns {
        door.max_conns = n;
    }
    if let Some(x) = d.reserved_share {
        if !(0.0..=1.0).contains(&x) {
            return Err("[door] reserved_share must be between 0 and 1".into());
        }
        door.reserved_share = x;
    }
    if let Some(b) = d.public_read {
        door.public_read = b;
    }
    if let Some(n) = d.public_reads_per_block_per_min {
        door.public_reads_per_block_per_min = n;
    }
    if let Some(n) = d.keep_days {
        door.keep_days = n;
    }
    if let Some(n) = d.join_flood_refusals {
        door.join_flood_refusals = n;
    }
    if let Some(n) = d.join_flood_window_secs {
        door.join_flood_window_secs = n;
    }
    out.door = d.is_set();
    Ok(out)
}

/// Reads the factory numbers from the current policy of the data folder. Returns the rules and whether
/// the policy has a `[switch]` table.
pub fn policy_rules(
    data_dir: &Path,
    policy_path: Option<&Path>,
) -> Result<(FactoryRules, bool), String> {
    let store = crate::policy::Store::new(data_dir);
    store
        .initialize(policy_path)
        .map_err(|e| format!("cannot read the policy: {e}"))?;
    let snap = store
        .current()
        .map_err(|e| format!("cannot read the policy: {e}"))?;
    let policy: &Policy = &snap.policy;
    Ok((policy.factory(), policy.has_switch_table()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> FactoryRules {
        FactoryRules::default()
    }

    #[test]
    fn an_empty_file_keeps_everything_off() {
        let file = parse("").unwrap();
        let mut cfg = HubConfig::new("unused");
        let out = apply(&mut cfg, &file, &defaults(), false).unwrap();
        assert_eq!(
            out.line(),
            "factory: join off, intake off, verify off, review off, stamp off, test off, door off"
        );
        assert!(!cfg.intake.enabled && !cfg.verify.enabled && !cfg.review.enabled);
    }

    #[test]
    fn each_table_switches_its_part_on() {
        let file = parse(
            r#"
            [join]
            enabled = true
            [intake]
            enabled = true
            reference_repo = "ref"
            bundle_store = "store"
            [verify]
            enabled = true
            repository = "o/p"
            public_address = "https://hub.example"
            token_file = "tok"
            [review]
            enabled = true
            [stamp]
            enabled = true
            maintainers = ["boss"]
            [test]
            enabled = true
            [door]
            trust_header = "X-Real-IP"
            keep_days = 3
            "#,
        )
        .unwrap();
        let mut cfg = HubConfig::new("unused");
        let out = apply(&mut cfg, &file, &defaults(), false).unwrap();
        assert_eq!(
            out.line(),
            "factory: join on, intake on, verify on, review on, stamp on, test on, door on"
        );
        assert_eq!(cfg.intake.reference_repo, Some(PathBuf::from("ref")));
        assert_eq!(cfg.intake.store_dir, Some(PathBuf::from("store")));
        assert_eq!(cfg.verify.repository, "o/p");
        assert_eq!(cfg.stamp.maintainers, vec!["boss".to_string()]);
        assert_eq!(cfg.door.keep_days, 3);
        assert_eq!(cfg.door.trust_header.as_deref(), Some("X-Real-IP"));
        // policy numbers
        assert_eq!(cfg.join.proof_of_work_bits, 22);
        assert_eq!(cfg.verify.rules.max_concurrent, 16);
        assert_eq!(cfg.verify.rules.tier0_share_percent, 20);
        assert_eq!(cfg.stamp.rules.tier1_landed, 5);
        assert_eq!(cfg.stamp.quorum, 3);
        assert_eq!(cfg.intake.tier_bundle_bytes[0], 256 * 1024);
    }

    #[test]
    fn unknown_keys_and_tables_are_refused() {
        assert!(parse("[intake]\nenabeld = true\n").is_err());
        assert!(parse("[nothing]\nenabled = true\n").is_err());
        assert!(parse("unknown = 1\n").is_err());
    }

    #[test]
    fn verify_needs_its_address_when_on() {
        let file = parse("[verify]\nenabled = true\n").unwrap();
        let mut cfg = HubConfig::new("unused");
        let err = apply(&mut cfg, &file, &defaults(), false).unwrap_err();
        assert!(err.contains("public_address"), "{err}");
    }

    #[test]
    fn a_policy_switch_off_vetoes_the_part() {
        let file =
            parse("[intake]\nenabled = true\n[stamp]\nenabled = true\ntier0_coding = true\n")
                .unwrap();
        let mut rules = defaults();
        rules.switch.intake = Mode::Off;
        rules.switch.tier0_coding = Mode::Enforce;
        let mut cfg = HubConfig::new("unused");
        let out = apply(&mut cfg, &file, &rules, true).unwrap();
        assert!(!cfg.intake.enabled);
        assert!(cfg.stamp.tier0_coding);
        assert!(out.line().contains("intake off (policy switch)"));
        // Without a switch table in the policy the same defaults veto nothing.
        let mut cfg = HubConfig::new("unused");
        apply(&mut cfg, &file, &rules, false).unwrap();
        assert!(cfg.intake.enabled);
    }

    #[test]
    fn review_numbers_come_from_the_policy() {
        let mut rules = defaults();
        let mut rule = crate::policy::ReviewRules::default();
        rule.tier0_author.reviewers = 2;
        rule.audit_share = 0.5;
        rules.review.insert("default".into(), rule);
        let mut cfg = HubConfig::new("unused");
        apply(&mut cfg, &FactoryFile::default(), &rules, false).unwrap();
        assert_eq!(cfg.review.rule.reviewers[0], 2);
        assert_eq!(cfg.review.audit_share, 0.5);
    }
}
