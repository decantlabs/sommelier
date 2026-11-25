//! The signed form of `review.verdict`. Pure.

use serde_json::{Value, json};

/// The object a key member signs with `sign_request`. `submission` is the SHA 256 of the bundle under
/// review, `hub_id` the id of the hub that assigned the review and `verdict_text` the text of the answer as
/// sent. Binding the submission and the hub means a signature cannot be moved to another patch or another
/// hub.
pub fn verdict_request(
    task: &str,
    n: u64,
    submission: &str,
    hub_id: &str,
    verdict_text: &str,
) -> Value {
    json!({"op": "review.verdict", "task": task, "n": n, "submission": submission,
           "hub": hub_id, "verdict": verdict_text})
}
