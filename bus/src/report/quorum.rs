//! Machine classes, the quorum of a matrix cell, outlier marks and matrix regressions.
//!
//! One report is a hint. A cell is settled when `quorum` independent owners agree. Independent means
//! distinct members in distinct address blocks. Every function here is pure and does not depend on
//! the order of its input.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::types::{MachineClass, Outcome};

/// Words that name a vendor or a product line and not the card. They are dropped from the key.
const VENDOR_WORDS: &[&str] = &[
    "nvidia",
    "geforce",
    "amd",
    "radeon",
    "ati",
    "intel",
    "corporation",
    "graphics",
    "series",
    "inc",
];

/// The class a report counts under: operating system plus the normalised GPU model, for example
/// `windows/rtx 4080`. Two spellings of one card give one key, different cards stay apart.
pub fn machine_key(m: &MachineClass) -> String {
    format!("{}/{}", m.os.trim().to_lowercase(), normalise_gpu(&m.gpu))
}

fn normalise_gpu(gpu: &str) -> String {
    let mut s = gpu.to_lowercase();
    for mark in ["(r)", "(tm)", "(c)", "\u{ae}", "\u{2122}", "\u{a9}"] {
        s = s.replace(mark, " ");
    }
    // Tokens are runs of letters or runs of digits, so `rtx4080`, `rtx-4080` and `RTX 4080` agree.
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_digit = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            let d = ch.is_ascii_digit();
            if !cur.is_empty() && d != cur_digit {
                tokens.push(std::mem::take(&mut cur));
            }
            cur_digit = d;
            cur.push(ch);
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens.retain(|t| !VENDOR_WORDS.contains(&t.as_str()));
    if tokens.is_empty() {
        "unknown".to_string()
    } else {
        tokens.join(" ")
    }
}

/// One report as the quorum sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportView {
    pub member: String,
    /// Tier of the member. Carried for callers, the quorum does not weigh it.
    pub tier: u8,
    /// Opaque address block of the member, `None` when unknown.
    pub block: Option<String>,
    pub outcome: Outcome,
    /// UTC epoch seconds.
    pub at: u64,
}

/// Settings of the quorum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuorumRules {
    /// Distinct owners in distinct blocks that settle a cell. At least 1.
    pub quorum: usize,
}

impl Default for QuorumRules {
    fn default() -> Self {
        Self { quorum: 3 }
    }
}

/// State of a matrix cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellState {
    Unknown,
    HintWorks,
    HintFails,
    Works,
    Fails,
    /// Both sides reached the quorum.
    Mixed,
}

impl CellState {
    /// The name used in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            CellState::Unknown => "unknown",
            CellState::HintWorks => "hint_works",
            CellState::HintFails => "hint_fails",
            CellState::Works => "works",
            CellState::Fails => "fails",
            CellState::Mixed => "mixed",
        }
    }
}

/// Result of [`cell`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub state: CellState,
    /// Independent owners that saw it work.
    pub works: usize,
    /// Independent owners that saw it fail.
    pub fails: usize,
    /// How many more agreeing reports settle the cell. 0 when it is settled.
    pub need_more: usize,
}

/// Total order that picks the newest report and breaks ties the same way for every input order.
fn newer_key(r: &ReportView) -> (u64, u8, &str) {
    let rank = match r.outcome {
        Outcome::Fails => 2,
        Outcome::Works => 1,
        Outcome::Inconclusive => 0,
    };
    (r.at, rank, r.member.as_str())
}

/// The state of one cell from the reports of one profile, machine key and build.
///
/// Inconclusive reports are ignored. A member counts once with the newest report. Members of one
/// address block count once with the newest report of the block. A report without a block is its
/// own block.
pub fn cell(reports: &[ReportView], rules: &QuorumRules) -> Cell {
    let quorum = rules.quorum.max(1);

    let mut by_member: BTreeMap<&str, &ReportView> = BTreeMap::new();
    for r in reports
        .iter()
        .filter(|r| r.outcome != Outcome::Inconclusive)
    {
        let slot = by_member.entry(r.member.as_str()).or_insert(r);
        if newer_key(r) > newer_key(slot) {
            *slot = r;
        }
    }

    // Key of a block: its name, or the member for a report without a block. The two forms cannot
    // collide because of the prefix.
    let mut by_block: BTreeMap<String, &ReportView> = BTreeMap::new();
    for r in by_member.values() {
        let key = match &r.block {
            Some(b) => format!("b:{b}"),
            None => format!("m:{}", r.member),
        };
        let slot = by_block.entry(key).or_insert(r);
        if newer_key(r) > newer_key(slot) {
            *slot = r;
        }
    }

    let works = by_block
        .values()
        .filter(|r| r.outcome == Outcome::Works)
        .count();
    let fails = by_block
        .values()
        .filter(|r| r.outcome == Outcome::Fails)
        .count();

    let state = if works >= quorum && fails >= quorum {
        CellState::Mixed
    } else if works >= quorum {
        CellState::Works
    } else if fails >= quorum {
        CellState::Fails
    } else if works > fails {
        CellState::HintWorks
    } else if fails > works {
        CellState::HintFails
    } else {
        CellState::Unknown
    };
    let need_more = match state {
        CellState::Works | CellState::Fails | CellState::Mixed => 0,
        _ => quorum - works.max(fails),
    };
    Cell {
        state,
        works,
        fails,
        need_more,
    }
}

/// What a report says about a settled cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The report matches the settled cell. Stamp `run_agreed`.
    Agreed,
    /// The report contradicts the settled cell. Stamp `run_outlier`.
    Outlier,
    /// The cell is not settled, or the report is inconclusive.
    None,
}

/// Compares a report with a cell. Only a cell that is `works` or `fails` judges.
pub fn classify(report: &ReportView, cell: &Cell) -> Verdict {
    let settled = match cell.state {
        CellState::Works => Outcome::Works,
        CellState::Fails => Outcome::Fails,
        _ => return Verdict::None,
    };
    match report.outcome {
        Outcome::Inconclusive => Verdict::None,
        o if o == settled => Verdict::Agreed,
        _ => Verdict::Outlier,
    }
}

/// A cell that turned from `works` to `fails`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Regression {
    /// The last commit whose cell was `works` before the change.
    pub last_good: String,
    /// The first commit whose cell is `fails` after it.
    pub first_bad: String,
}

/// Regressions along a history. `commits` run oldest first, `cells` maps a commit to its cell. A
/// commit without a cell, and a cell that is unknown, a hint or mixed, does not break a range.
pub fn regressions(commits: &[String], cells: &BTreeMap<String, Cell>) -> Vec<Regression> {
    let mut out = Vec::new();
    let mut last_good: Option<&str> = None;
    for c in commits {
        let Some(cell) = cells.get(c) else {
            continue;
        };
        match cell.state {
            CellState::Works => last_good = Some(c),
            CellState::Fails => {
                if let Some(good) = last_good.take() {
                    out.push(Regression {
                        last_good: good.to_string(),
                        first_bad: c.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}
