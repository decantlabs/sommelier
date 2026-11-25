//! Static gates for a submission: checks that read a patch and never build or run it.
//!
//! Part of the public factory, see `docs/factory.md` section 8.2. The gates are pure functions over a
//! parsed diff and an order. They take no clock, no random and no file access. Every string of the
//! diff is treated as hostile, so no gate indexes, unwraps or recurses on input.
//!
//! What the gates see. The gates read the diff of the two end trees and, for the gates of paths, scope,
//! hold paths and entry kinds, the path list of every commit between base and head. They do not diff
//! the middle commits line by line. That is safe only because a submission lands as ONE squashed commit
//! made from the gated end tree, so no middle commit ever reaches main. The lander does the squash.
//! A submission whose middle commits were pushed as they are would carry content the gates never read.
//!
//! What a line pattern cannot see, and this module does not claim to: a call split over two lines, an
//! alias such as `use ... as C`, a macro of an existing crate that expands to a capability, and secrets
//! that are encoded. The stages that must catch these are the clean build without secrets (S2) and the
//! assigned review (S3).
//!
//! The entry point is [`check`]. Each gate is its own function in [`gates`], so a caller or a test can
//! run one alone. A finding never repeats a secret it found, it names the pattern only.

pub mod diff;
pub mod gates;
pub mod glob;
pub mod rules;

use diff::ParsedDiff;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// The first hits, cut so that every kind keeps its first entries. See `capability_counts`.
    pub capability_delta: Vec<DeltaEntry>,
    pub test_delta: Vec<DeltaEntry>,
    /// How many hits there were per kind before the cut.
    #[serde(default)]
    pub capability_counts: BTreeMap<String, u64>,
    #[serde(default)]
    pub test_counts: BTreeMap<String, u64>,
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
        gates::paths_with(diff, rules),
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
    let max = rules.max_findings_per_gate;
    let (capability_counts, test_counts) =
        (kind_counts(&capability_delta), kind_counts(&test_delta));
    GateReport {
        passed,
        risk_after,
        findings,
        capability_delta: cut_delta(capability_delta, max),
        test_delta: cut_delta(test_delta, max),
        capability_counts,
        test_counts,
    }
}

/// How many entries each kind has.
fn kind_counts(list: &[DeltaEntry]) -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    for e in list {
        *m.entry(e.kind.clone()).or_insert(0u64) += 1;
    }
    m
}

/// The first entries of every kind that always survive a cut.
const KEEP_PER_KIND: usize = 3;

/// Which items a cut to `max` keeps: the first [`KEEP_PER_KIND`] of every kind, then the rest in order
/// until `max` is reached. Padding with one kind cannot push another kind out. The list may be longer
/// than `max` by the number of kinds, never by more.
pub(crate) fn keep_flags<T>(list: &[T], max: usize, kind: impl Fn(&T) -> String) -> Vec<bool> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut keep = vec![false; list.len()];
    let mut kept = 0usize;
    for (i, item) in list.iter().enumerate() {
        let n = seen.entry(kind(item)).or_insert(0);
        if *n < KEEP_PER_KIND {
            keep[i] = true;
            kept += 1;
        }
        *n += 1;
    }
    for flag in keep.iter_mut() {
        if kept >= max {
            break;
        }
        if !*flag {
            *flag = true;
            kept += 1;
        }
    }
    keep
}

fn cut_delta(list: Vec<DeltaEntry>, max: usize) -> Vec<DeltaEntry> {
    if list.len() <= max {
        return list;
    }
    let keep = keep_flags(&list, max, |e| e.kind.clone());
    list.into_iter()
        .zip(keep)
        .filter_map(|(e, k)| k.then_some(e))
        .collect()
}
