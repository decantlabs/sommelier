//! The fixed form of a review answer.

use serde::{Deserialize, Serialize};

/// Most findings in one answer.
pub const MAX_FINDINGS: usize = 50;
/// Longest text of one finding in characters.
pub const MAX_FINDING_CHARS: usize = 500;
/// Longest summary in characters.
pub const MAX_SUMMARY_CHARS: usize = 1000;
/// Longest path of a finding in characters.
pub const MAX_PATH_CHARS: usize = 300;

/// The three possible answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictWord {
    Pass,
    Fail,
    Escalate,
}

impl VerdictWord {
    pub fn as_str(self) -> &'static str {
        match self {
            VerdictWord::Pass => "pass",
            VerdictWord::Fail => "fail",
            VerdictWord::Escalate => "escalate",
        }
    }
}

/// One finding by file and line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub path: String,
    pub line: u32,
    pub text: String,
}

/// A parsed review answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub verdict: VerdictWord,
    pub findings: Vec<Finding>,
    pub summary: String,
}

/// Parses one JSON object. Unknown fields, bad words and over long text are refused.
pub fn parse_verdict(text: &str) -> Result<Verdict, String> {
    let v: Verdict = serde_json::from_str(text.trim()).map_err(|e| {
        format!(
            "answer is not the review form (one JSON object with verdict, findings, summary): {e}"
        )
    })?;
    if v.findings.len() > MAX_FINDINGS {
        return Err(format!(
            "too many findings: {} (at most {MAX_FINDINGS}), keep the most important",
            v.findings.len()
        ));
    }
    for (i, f) in v.findings.iter().enumerate() {
        if f.path.is_empty() || f.path.chars().count() > MAX_PATH_CHARS {
            return Err(format!(
                "finding {i}: path must be 1 to {MAX_PATH_CHARS} characters"
            ));
        }
        if f.text.chars().count() > MAX_FINDING_CHARS {
            return Err(format!(
                "finding {i}: text is longer than {MAX_FINDING_CHARS} characters"
            ));
        }
    }
    if v.summary.chars().count() > MAX_SUMMARY_CHARS {
        return Err(format!(
            "summary is longer than {MAX_SUMMARY_CHARS} characters"
        ));
    }
    Ok(v)
}
