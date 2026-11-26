//! The state of a feature, derived at read time from the cells of its proofs. Never stored.
//!
//! A proof names a profile and a scenario (and a milestone, which the scenario reaches when its run
//! works). The cell of a proof for a machine key is the cell of the newest build that has reports of
//! that profile, scenario and machine key, as the matrix computes it. The state of a feature for a
//! machine key is `works` when every proof works, `fails` when any proof fails and `unknown`
//! otherwise (a missing cell, a hint, a mixed cell). A feature without proof has no state.

use std::collections::{BTreeMap, BTreeSet};

use super::Proof;
use crate::report::ReportRow;
use crate::report::quorum::{Cell, CellState, QuorumRules, ReportView, cell};
use crate::report::types::Outcome;

/// The state of a feature for one machine key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureState {
    Works,
    Fails,
    Unknown,
}

impl FeatureState {
    /// The name used in JSON and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            FeatureState::Works => "works",
            FeatureState::Fails => "fails",
            FeatureState::Unknown => "unknown",
        }
    }
}

/// The cell of a proof, by (profile, scenario, machine key).
pub type ProofCells = BTreeMap<(String, String, String), CellState>;

fn outcome_of(s: &str) -> Option<Outcome> {
    match s {
        "works" => Some(Outcome::Works),
        "fails" => Some(Outcome::Fails),
        "inconclusive" => Some(Outcome::Inconclusive),
        _ => None,
    }
}

/// The cells of every (profile, scenario, machine key) at its newest build. `blocks` are the address
/// blocks of the side store by (run, member), as the matrix reads them. The newest build is the one
/// with the newest report.
pub fn proof_cells(
    rows: &[ReportRow],
    blocks: &BTreeMap<(String, String), String>,
    rules: &QuorumRules,
) -> ProofCells {
    type Key<'a> = (&'a str, &'a str, &'a str);
    // (profile, scenario, machine key) -> build -> (newest message id, views)
    let mut groups: BTreeMap<Key<'_>, BTreeMap<&str, (u64, Vec<ReportView>)>> = BTreeMap::new();
    for r in rows {
        let Some(outcome) = outcome_of(&r.outcome) else {
            continue;
        };
        let e = groups
            .entry((&r.profile, &r.scenario, &r.machine_key))
            .or_default()
            .entry(&r.build)
            .or_insert_with(|| (0, Vec::new()));
        e.0 = e.0.max(r.msg_id);
        e.1.push(ReportView {
            member: r.member.clone(),
            tier: 0,
            block: blocks.get(&(r.run.clone(), r.member.clone())).cloned(),
            outcome,
            at: r.at,
        });
    }
    let mut out = ProofCells::new();
    for ((p, s, k), builds) in groups {
        let newest = builds.iter().max_by(|a, b| (a.1.0, a.0).cmp(&(b.1.0, b.0)));
        if let Some((_, (_, views))) = newest {
            let c: Cell = cell(views, rules);
            out.insert((p.to_owned(), s.to_owned(), k.to_owned()), c.state);
        }
    }
    out
}

/// The state of a feature per machine key. The machine keys are those that have a cell for at least
/// one of its proofs. A feature without proof, or none of whose proofs ran anywhere, has no entry.
pub fn feature_states(proofs: &[Proof], cells: &ProofCells) -> BTreeMap<String, FeatureState> {
    let keys: BTreeSet<&str> = cells
        .keys()
        .filter(|(p, s, _)| proofs.iter().any(|x| &x.profile == p && &x.scenario == s))
        .map(|(_, _, k)| k.as_str())
        .collect();
    let mut out = BTreeMap::new();
    for k in keys {
        let states: Vec<CellState> = proofs
            .iter()
            .map(|x| {
                cells
                    .get(&(x.profile.clone(), x.scenario.clone(), k.to_owned()))
                    .copied()
                    .unwrap_or(CellState::Unknown)
            })
            .collect();
        let state = if states.contains(&CellState::Fails) {
            FeatureState::Fails
        } else if states.iter().all(|s| *s == CellState::Works) {
            FeatureState::Works
        } else {
            FeatureState::Unknown
        };
        out.insert(k.to_owned(), state);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof(p: &str, s: &str, m: &str) -> Proof {
        Proof {
            profile: p.into(),
            scenario: s.into(),
            milestone: m.into(),
        }
    }

    fn cells(list: &[(&str, &str, &str, CellState)]) -> ProofCells {
        list.iter()
            .map(|(p, s, k, c)| ((p.to_string(), s.to_string(), k.to_string()), *c))
            .collect()
    }

    #[test]
    fn every_proof_works_gives_works_and_one_failure_gives_fails() {
        let proofs = [proof("a", "boot", "m1"), proof("a", "play", "m2")];
        let c = cells(&[
            ("a", "boot", "k1", CellState::Works),
            ("a", "play", "k1", CellState::Works),
            ("a", "boot", "k2", CellState::Works),
            ("a", "play", "k2", CellState::Fails),
        ]);
        let st = feature_states(&proofs, &c);
        assert_eq!(st["k1"], FeatureState::Works);
        assert_eq!(st["k2"], FeatureState::Fails);
    }

    #[test]
    fn a_missing_hint_or_mixed_cell_is_unknown() {
        let proofs = [proof("a", "boot", "m1"), proof("a", "play", "m2")];
        let c = cells(&[
            ("a", "boot", "k1", CellState::Works),
            ("a", "boot", "k2", CellState::HintWorks),
            ("a", "play", "k2", CellState::Works),
            ("a", "boot", "k3", CellState::Mixed),
        ]);
        let st = feature_states(&proofs, &c);
        assert_eq!(st["k1"], FeatureState::Unknown);
        assert_eq!(st["k2"], FeatureState::Unknown);
        assert_eq!(st["k3"], FeatureState::Unknown);
    }

    #[test]
    fn a_failure_wins_over_a_missing_proof() {
        let proofs = [proof("a", "boot", "m1"), proof("a", "play", "m2")];
        let c = cells(&[("a", "boot", "k1", CellState::Fails)]);
        assert_eq!(feature_states(&proofs, &c)["k1"], FeatureState::Fails);
    }

    #[test]
    fn no_proof_or_no_run_gives_no_entry() {
        let c = cells(&[("a", "boot", "k1", CellState::Works)]);
        assert!(feature_states(&[], &c).is_empty());
        assert!(feature_states(&[proof("b", "boot", "m")], &c).is_empty());
    }
}
