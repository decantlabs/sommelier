//! Static gates for a submission: checks that read a patch and never build or run it.
//!
//! Part of the public factory, see `docs/factory.md` section 8.2. The gates are pure functions over a
//! parsed diff and an order. They take no clock, no random and no file access. Every string of the
//! diff is treated as hostile, so no gate indexes, unwraps or recurses on input.
//!
//! The entry point is [`check`]. Each gate is its own function in [`gates`], so a caller or a test can
//! run one alone. A finding never repeats a secret it found, it names the pattern only.

pub mod diff;
pub mod gates;
pub mod glob;
pub mod rules;

use diff::ParsedDiff;
use serde::{Deserialize, Serialize};

pub use rules::{GateRules, NamedPattern, TierCaps};

/// How much review a patch needs. The order is the order of the steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Normal,
    Guarded,
}

impl Risk {
    /// One step up. `Guarded` stays.
    pub fn raised(self) -> Risk {
        match self {
            Risk::Low => Risk::Normal,
            Risk::Normal | Risk::Guarded => Risk::Guarded,
        }
    }
}

/// The part of a work order the gates need.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderView {
    /// The base commit of the lease.
    pub base: String,
    /// Path globs the patch may touch.
    pub scope: Vec<String>,
    pub risk: Risk,
    /// Tier of the author, 0 to 3.
    pub tier: u8,
}

/// How serious a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The submission is returned.
    Block,
    /// The risk of the order goes up.
    Raise,
    /// Listed for the reviewer.
    Note,
}

/// One thing a gate found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub gate: String,
    pub severity: Severity,
    pub path: Option<String>,
    pub line: Option<u32>,
    /// One line. It never holds the text of a secret.
    pub text: String,
}

/// One hit of the capability or the test delta.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaEntry {
    /// Name of the pattern or of the kind of change.
    pub kind: String,
    pub path: String,
    /// Line number, 0 when the hit belongs to the whole file.
    pub line: u32,
}

/// The result of all gates. Its JSON form is stable for equal input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateReport {
    pub passed: bool,
    pub risk_after: Risk,
    pub findings: Vec<Finding>,
    pub capability_delta: Vec<DeltaEntry>,
    pub test_delta: Vec<DeltaEntry>,
}

/// Runs all thirteen gates in the order of the design and sums them up.
pub fn check(diff: &ParsedDiff, order: &OrderView, rules: &GateRules) -> GateReport {
    let capability_delta = gates::capability_hits(diff, rules);
    let test_delta = gates::test_hits(diff, rules);
    let runs: Vec<Vec<Finding>> = vec![
        gates::rules_problems(rules),
        gates::base(diff, order),
        gates::history(diff),
        gates::authors(diff, rules),
        gates::paths(diff),
        gates::scope(diff, order),
        gates::hold_paths(diff, order, rules),
        gates::entry_kinds(diff),
        gates::size(diff, order, rules),
        gates::manifests(diff, order, rules),
        gates::capability(&capability_delta),
        gates::tests(&test_delta),
        gates::licence(diff, rules),
        gates::secrets(diff, rules),
    ];
    let mut passed = true;
    let mut risk_after = order.risk;
    for f in runs.iter().flatten() {
        if f.severity == Severity::Block {
            passed = false;
        }
        if f.gate == "manifests" {
            risk_after = risk_after.max(Risk::Guarded);
        } else if f.severity == Severity::Raise {
            risk_after = risk_after.max(order.risk.raised());
        }
    }
    let mut findings = Vec::new();
    for run in runs {
        findings.extend(gates::limit(run, rules.max_findings_per_gate));
    }
    GateReport {
        passed,
        risk_after,
        findings,
        capability_delta: cap_list(capability_delta, rules.max_findings_per_gate),
        test_delta: cap_list(test_delta, rules.max_findings_per_gate),
    }
}

fn cap_list(mut list: Vec<DeltaEntry>, max: usize) -> Vec<DeltaEntry> {
    list.truncate(max);
    list
}
