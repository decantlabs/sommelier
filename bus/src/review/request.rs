//! The signed form of `review.verdict`. Pure.

use serde_json::{Value, json};

/// The object a key member signs with `sign_request`. `verdict` is the text of the answer as sent.
pub fn verdict_request(task: &str, n: u64, verdict_text: &str) -> Value {
    json!({"op": "review.verdict", "task": task, "n": n, "verdict": verdict_text})
}
