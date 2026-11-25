//! The decisions and the messages of the review flow. Pure: no clock, no file, no database.

use serde_json::{Value, json};

use super::data::{Review, SubRow};
use super::verdict::{Verdict, VerdictWord};
use crate::msg::{Kind, Message, Role};
use crate::time::rfc3339;

/// Longest body of a FACT the hub writes, below the limit of a message.
const MAX_FACT_BYTES: usize = 3800;
/// Longest resume note that goes back to the task.
const MAX_NOTE_CHARS: usize = 1500;
/// Longest signed text the hub keeps with an answer. A longer signed answer is refused.
pub const MAX_SIGNED_BYTES: usize = 2400;

/// What the answers so far mean for a submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Not enough answers yet.
    Wait,
    /// A required review said fail. `n` is its number.
    Failed { n: u64 },
    /// A review said escalate.
    Escalate { n: u64 },
    /// The audit says something else than the first reviews.
    Disagree { n: u64 },
    /// All required reviews pass and the audit review is not assigned yet.
    NeedAudit,
    /// Everything the rule asks for passed.
    Approve,
}

fn word(r: &Review) -> &str {
    if r.row.state == "answered" {
        r.row.verdict.as_str()
    } else {
        ""
    }
}

/// How many DISTINCT reviewers passed. One member who holds two rows of a submission counts once.
pub fn passed_reviewers(required: &[&Review]) -> usize {
    required
        .iter()
        .filter(|r| word(r) == "pass")
        .map(|r| r.row.reviewer.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// Decides from the reviews of one submission. The first answer that needs action wins, a fail before an escalate.
pub fn decide(reviews: &[Review]) -> Decision {
    let required: Vec<&Review> = reviews.iter().filter(|r| !r.is_audit()).collect();
    if required.is_empty() {
        return Decision::Wait;
    }
    if let Some(r) = required.iter().find(|r| word(r) == "fail") {
        return Decision::Failed { n: r.row.n };
    }
    if let Some(r) = required.iter().find(|r| word(r) == "escalate") {
        return Decision::Escalate { n: r.row.n };
    }
    let need = required.iter().map(|r| r.meta.required).max().unwrap_or(1) as usize;
    if passed_reviewers(&required) < need {
        return Decision::Wait;
    }
    match reviews.iter().find(|r| r.is_audit()) {
        Some(a) => match word(a) {
            "pass" => Decision::Approve,
            "escalate" => Decision::Escalate { n: a.row.n },
            "" => Decision::Wait,
            _ => Decision::Disagree { n: a.row.n },
        },
        None if required.iter().any(|r| r.row.audited) => Decision::NeedAudit,
        None => Decision::Approve,
    }
}

fn hub_message(kind: Kind, topic: String, body: Value) -> Message {
    Message {
        id: 0,
        ts: String::new(),
        from: "hub".into(),
        role: Role::Hub,
        kind,
        topic,
        body: body.to_string(),
        refs: Vec::new(),
        to: Vec::new(),
    }
}

/// A control character becomes a space, a message is one line.
pub fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The FACT `review/<task>/<n>` that assigns a review.
#[allow(clippy::too_many_arguments)]
pub fn assignment_fact(
    task: &str,
    n: u64,
    submission: &str,
    reviewer: &str,
    role: &str,
    required: u32,
    audited: bool,
    prev: &[String],
    now_sec: u64,
    due: u64,
) -> Message {
    hub_message(
        Kind::Fact,
        format!("review/{task}/{n}"),
        json!({
            "v": 1,
            "state": "assigned",
            "submission": submission,
            "reviewer": reviewer,
            "picked_by": if role == "audit" { "audit" } else { "rule" },
            "role": role,
            "required": required,
            "audited": audited,
            "prev": prev,
            "at": now_sec,
            "due": due,
        }),
    )
}

/// The FACT that records an answer. It stays under the size of a message: findings are dropped from the end.
/// `signed` is the exact text the key member signed, kept so the signature can be checked again from the
/// log alone. It is never shortened, a caller refuses a signed answer that is longer than [`MAX_SIGNED_BYTES`].
pub fn answer_fact(
    review: &Review,
    verdict: &Verdict,
    signature: &str,
    signed: Option<&str>,
    now_sec: u64,
) -> Message {
    let mut findings: Vec<Value> = verdict
        .findings
        .iter()
        .map(|f| json!({"path": clean(&f.path), "line": f.line, "text": clean(&f.text)}))
        .collect();
    let mut summary = clean(&verdict.summary);
    let mut dropped = 0usize;
    loop {
        let body = json!({
            "v": 1,
            "state": "answered",
            "submission": review.row.submission,
            "reviewer": review.row.reviewer,
            "verdict": verdict.verdict.as_str(),
            "findings": findings,
            "findings_dropped": dropped,
            "summary": summary,
            "signature": signature,
            "audited": review.row.audited,
            "at": now_sec,
        });
        let mut body = body;
        if let Some(text) = signed {
            body["signed"] = json!(text);
        }
        if body.to_string().len() <= MAX_FACT_BYTES || (findings.is_empty() && summary.is_empty()) {
            return hub_message(
                Kind::Fact,
                format!("review/{}/{}", review.row.task, review.row.n),
                body,
            );
        }
        if findings.pop().is_some() {
            dropped += 1;
        } else {
            summary = summary.chars().take(summary.chars().count() / 2).collect();
        }
    }
}

/// The FACT that marks the answered required reviews of a submission as audited. The hub writes it once the
/// required passes are in, so nobody learns of the audit while the required reviews are open.
pub fn audit_mark_fact(review: &Review, now_sec: u64) -> Message {
    hub_message(
        Kind::Fact,
        format!("review/{}/{}", review.row.task, review.row.n),
        json!({"v": 1, "state": "answered", "audited": true, "at": now_sec}),
    )
}

/// The FACT that a maintainer writes to settle an escalated review or a disagreeing audit. It changes the
/// verdict of an answered review to `pass` or `fail` and names who decided and why. The projection accepts a
/// change of an answered row only for a FACT that carries `resolved_by`.
pub fn resolution_fact(
    review: &Review,
    decision: &str,
    by: &str,
    reason: &str,
    now_sec: u64,
) -> Message {
    hub_message(
        Kind::Fact,
        format!("review/{}/{}", review.row.task, review.row.n),
        json!({
            "v": 1,
            "state": "answered",
            "submission": review.row.submission,
            "verdict": decision,
            "was": review.row.verdict,
            "resolved_by": clean(by),
            "reason": clean(reason),
            "at": now_sec,
        }),
    )
}

/// The FACT `submission/<sha256>` with a new state. Every other field stays as the row had it.
pub fn submission_fact(sub: &SubRow, state: &str, now_sec: u64) -> Message {
    hub_message(
        Kind::Fact,
        format!("submission/{}", sub.sha256),
        json!({
            "v": 1,
            "submission": sub.sha256,
            "task": sub.task,
            "attempt": sub.attempt,
            "member": sub.member,
            "base": sub.base,
            "head": sub.head,
            "bytes": sub.bytes,
            "state": state,
            "risk": sub.risk,
            "tier": sub.tier,
            "at": now_sec,
        }),
    )
}

/// An ALERT. `to_label` is the group in the text (`coordinator`, `maintainers`), `to_names` the token members
/// that get it addressed. A key member cannot be addressed, so callers leave it out of `to_names`.
pub fn alert(
    code: &str,
    key: &str,
    text: &str,
    to_label: &str,
    to_names: &[String],
    now_ms: u64,
) -> Message {
    let mut m = hub_message(
        Kind::Alert,
        format!("alert/{code}/{key}"),
        json!({
            "v": 1,
            "code": code,
            "key": key,
            "severity": "warn",
            "to": to_label,
            "text": clean(text),
        }),
    );
    m.ts = rfc3339(now_ms);
    m.refs = vec![format!("alert:{code}"), format!("key:{key}")];
    m.to = to_names.to_vec();
    m
}

/// The resume note of a task after a failed review: the summary and the findings, short.
pub fn resume_note(verdict: &Verdict) -> String {
    let mut note = format!("review failed: {}", clean(&verdict.summary));
    for f in &verdict.findings {
        note.push_str(&format!(
            " | {}:{} {}",
            clean(&f.path),
            f.line,
            clean(&f.text)
        ));
        if note.chars().count() > MAX_NOTE_CHARS {
            break;
        }
    }
    note.chars().take(MAX_NOTE_CHARS).collect()
}

/// The TASK_UPDATE that puts a task back in the queue with a resume note.
pub fn requeue(task: &str, prev_state: &str, attempt: u32, note: &str) -> Message {
    let mut m = hub_message(
        Kind::TaskUpdate,
        format!("task/{task}"),
        json!({
            "v": 1,
            "task": task,
            "prev": prev_state,
            "state": "ready",
            "attempt": attempt.saturating_add(1),
            "by": "hub",
            "reason": "review failed",
            "resume_note": note,
        }),
    );
    m.refs = vec!["state:ready".into()];
    m
}

/// True for the word that lets a submission go on.
pub fn is_pass(v: &Verdict) -> bool {
    v.verdict == VerdictWord::Pass
}
