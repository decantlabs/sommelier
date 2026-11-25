//! The first lock before landing (docs/factory.md 8.5): a `bundle:<sha256>` entry may enter the queue only when
//! its submission is approved, its gate report passed, its clean build passed and it holds the passing reviews its
//! rule asked for. This is an invariant of the queue. All of it lives in [`evaluate`].

use rusqlite::Connection;

use crate::review::data::{self, Review};

/// The prefix of a ref that names a submission instead of a branch.
pub const BUNDLE_PREFIX: &str = "bundle:";

/// Everything the lock looks at, read from the tables.
#[derive(Clone, Debug, Default)]
pub struct LockInputs {
    /// State of the submission, `None` when the hub does not know it.
    pub state: Option<String>,
    /// `Some(passed)` when a gate report exists.
    pub gate: Option<bool>,
    /// The conclusion of the clean build, `None` while it has none.
    pub verify: Option<String>,
    pub reviews: Vec<Review>,
}

/// Reads the inputs for one submission.
pub fn read_inputs(conn: &Connection, sha: &str) -> Result<LockInputs, String> {
    Ok(LockInputs {
        state: data::query_submission(conn, sha)?.map(|s| s.state),
        gate: data::gate_of(conn, sha)?.map(|g| g.0),
        verify: data::verify_conclusion(conn, sha)?,
        reviews: data::reviews_of(conn, sha)?,
    })
}

/// The decision. `require_approved` is false only when the review part asks whether it may approve.
/// The error is one line that names the part that is missing.
pub fn evaluate(sha: &str, i: &LockInputs, require_approved: bool) -> Result<(), String> {
    let refuse = |why: String| Err(format!("refused: bundle {sha} cannot be queued, {why}"));
    let Some(state) = &i.state else {
        return refuse("the hub does not know this submission".into());
    };
    if require_approved && state != "approved" {
        return refuse(format!("its state is `{state}`, it must be `approved`"));
    }
    match i.gate {
        None => return refuse("it has no gate report".into()),
        Some(false) => return refuse("its gate report did not pass".into()),
        Some(true) => {}
    }
    match i.verify.as_deref() {
        None => return refuse("its clean build has no conclusion yet".into()),
        Some("passed") => {}
        Some(other) => return refuse(format!("its clean build ended `{other}`")),
    }
    let required: Vec<&Review> = i.reviews.iter().filter(|r| !r.is_audit()).collect();
    if required.is_empty() {
        return refuse("no review was assigned".into());
    }
    for r in &required {
        if r.row.state == "answered" && r.row.verdict != "pass" {
            return refuse(format!(
                "review {} said `{}`",
                r.row.n,
                if r.row.verdict.is_empty() {
                    "nothing"
                } else {
                    &r.row.verdict
                }
            ));
        }
    }
    let need = required.iter().map(|r| r.meta.required).max().unwrap_or(1) as usize;
    // Distinct reviewers: one member who holds two rows is one pass, not two.
    let passed = required
        .iter()
        .filter(|r| r.row.state == "answered" && r.row.verdict == "pass")
        .map(|r| r.row.reviewer.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    if passed < need {
        return refuse(format!("{passed} of {need} required reviews have passed"));
    }
    if required.iter().any(|r| r.row.audited) {
        let audit = i.reviews.iter().find(|r| r.is_audit());
        match audit {
            None => return refuse("its audit review is not assigned".into()),
            Some(a) if a.row.state != "answered" => {
                return refuse("its audit review has no answer".into());
            }
            Some(a) if a.row.verdict != "pass" => {
                return refuse(format!("its audit review said `{}`", a.row.verdict));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// The check of `land.enqueue`. A ref that is not a `bundle:` ref passes untouched.
pub fn check_enqueue(conn: &Connection, ref_name: &str) -> Result<(), String> {
    let Some(sha) = ref_name.strip_prefix(BUNDLE_PREFIX) else {
        return Ok(());
    };
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "refused: `{ref_name}` is not `bundle:` and 64 hex characters"
        ));
    }
    evaluate(sha, &read_inputs(conn, sha)?, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::ReviewRow;
    use crate::review::data::Meta;

    fn review(n: u64, state: &str, verdict: &str, role: &str, audited: bool) -> Review {
        Review {
            row: ReviewRow {
                task: "t".into(),
                n,
                submission: "s".into(),
                reviewer: "r".into(),
                picked_by: "rule".into(),
                state: state.into(),
                verdict: verdict.into(),
                findings: "[]".into(),
                signature: String::new(),
                audited,
                msg_id: 1,
            },
            meta: Meta {
                role: role.into(),
                required: 1,
                at: 0,
                due: 0,
                prev: vec![],
            },
        }
    }

    fn good() -> LockInputs {
        LockInputs {
            state: Some("approved".into()),
            gate: Some(true),
            verify: Some("passed".into()),
            reviews: vec![review(1, "answered", "pass", "required", false)],
        }
    }

    fn why(i: &LockInputs) -> String {
        evaluate("abc", i, true).unwrap_err()
    }

    #[test]
    fn passes_only_when_every_part_holds() {
        assert!(evaluate("abc", &good(), true).is_ok());
    }

    #[test]
    fn each_missing_part_is_named() {
        let mut i = good();
        i.state = None;
        assert!(why(&i).contains("does not know"));
        let mut i = good();
        i.state = Some("reviewing".into());
        assert!(why(&i).contains("`reviewing`"));
        assert!(evaluate("abc", &i, false).is_ok());
        let mut i = good();
        i.gate = None;
        assert!(why(&i).contains("no gate report"));
        i.gate = Some(false);
        assert!(why(&i).contains("gate report did not pass"));
        let mut i = good();
        i.verify = None;
        assert!(why(&i).contains("no conclusion"));
        i.verify = Some("failed".into());
        assert!(why(&i).contains("ended `failed`"));
        let mut i = good();
        i.reviews.clear();
        assert!(why(&i).contains("no review"));
        i.reviews = vec![review(1, "assigned", "", "required", false)];
        assert!(why(&i).contains("0 of 1"));
        i.reviews = vec![review(1, "answered", "fail", "required", false)];
        assert!(why(&i).contains("said `fail`"));
        i.reviews = vec![review(1, "answered", "escalate", "required", false)];
        assert!(why(&i).contains("said `escalate`"));
    }

    fn by(mut r: Review, who: &str) -> Review {
        r.row.reviewer = who.into();
        r.meta.required = 2;
        r
    }

    #[test]
    fn two_required_reviews_need_two_passes() {
        let a = by(review(1, "answered", "pass", "required", false), "r1");
        let b = by(review(2, "assigned", "", "required", false), "r2");
        let mut i = good();
        i.reviews = vec![a.clone(), b];
        assert!(why(&i).contains("1 of 2"));
        let c = by(review(2, "answered", "pass", "required", false), "r2");
        i.reviews = vec![a, c];
        assert!(evaluate("abc", &i, true).is_ok());
    }

    #[test]
    fn two_passes_of_one_reviewer_count_once() {
        let a = by(review(1, "answered", "pass", "required", false), "same");
        let b = by(review(2, "answered", "pass", "required", false), "same");
        let mut i = good();
        i.reviews = vec![a, b];
        assert!(why(&i).contains("1 of 2"), "{}", why(&i));
    }

    #[test]
    fn an_audited_review_needs_a_passing_audit() {
        let mut i = good();
        i.reviews = vec![review(1, "answered", "pass", "required", true)];
        assert!(why(&i).contains("audit review is not assigned"));
        i.reviews.push(review(2, "assigned", "", "audit", false));
        assert!(why(&i).contains("no answer"));
        i.reviews[1] = review(2, "answered", "fail", "audit", false);
        assert!(why(&i).contains("audit review said `fail`"));
        i.reviews[1] = review(2, "answered", "pass", "audit", false);
        assert!(evaluate("abc", &i, true).is_ok());
    }

    #[test]
    fn other_refs_pass_and_a_bad_bundle_ref_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(check_enqueue(&conn, "fx/some-branch").is_ok());
        assert!(
            check_enqueue(&conn, "bundle:xyz")
                .unwrap_err()
                .contains("64 hex")
        );
    }
}
