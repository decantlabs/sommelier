//! Key identity of public members: key pairs, member ids, signed requests and the KEY table.
//!
//! Part of the public factory, see `docs/factory.md` sections 4 and 12. A person with no
//! token runs `sommelier join`, gets a member id made from a key that never leaves the
//! machine and connects as a tier 0 member that may do very little.
//!
//! * [`crypto`]: Ed25519 through `ring`, the member id, canonical signing, the proof of work.
//! * [`book`]: the key facts of the log, the hub's in memory book and the KEY table.
//! * [`join`]: the join rules, the nonces and the checks of a join request.
//! * [`ops`]: the requests `key.rotate` and `key.revoke`.
//! * [`client`]: the key file, joining and signing in from a client.

pub mod book;
pub mod client;
pub mod crypto;
pub mod join;
pub mod ops;

pub use book::{KeyBook, KeyModule, KeyState, Standing};
pub use crypto::{MemberKey, member_id, sign_request, verify_request};
pub use join::{Challenges, JoinRules};
pub use ops::on_op;

/// What a tier 0 member may use, in ONE place. Operations are lower case request names, message
/// kinds are upper case. Later phases extend this list.
pub const TIER0_ALLOWED: [&str; 11] = [
    "whoami",
    "heartbeat",
    "log",
    "roster",
    "who",
    "claims",
    "key.rotate",
    "key.revoke",
    "submit.status",
    "report.send",
    "test.next",
];

/// The requests a worker needs to take and deliver an order. A tier 0 member gets them only when the
/// hub runs with `tier0_coding` on. Tier 1 and above always have them.
pub const CODING_OPS: [&str; 7] = [
    "task.next",
    "task.work",
    "task.checkpoint",
    "task.done",
    "task.stuck",
    "task.decline",
    "task.heartbeat",
];

/// What a tier 1 member may use on top of the tier 0 list and [`CODING_OPS`]: the free text message
/// kinds, HELLO, and the two requests of assigned review.
pub const TIER1_EXTRA: [&str; 8] = [
    "CHAT",
    "FINDING",
    "FIX",
    "READY",
    "STUCK",
    "HELLO",
    "review.next",
    "review.verdict",
];

/// True when a tier 0 member may use the request `op` (such as `whoami` or `task.add`) or send
/// the message kind (such as `CHAT`).
pub fn tier0_allows(op_or_kind: &str) -> bool {
    TIER0_ALLOWED.contains(&op_or_kind)
}

/// What a member of `tier` may use, in ONE place. `what` is a request name or a message kind.
/// Tier -1 (no tier at all) may only ask `whoami`. Tier 0 has [`TIER0_ALLOWED`] and, with
/// `tier0_coding`, [`CODING_OPS`]. Tier 1 adds [`TIER1_EXTRA`]. Tier 2 and above may do what a
/// member with a token may do.
pub fn tier_allows(tier: i64, what: &str, tier0_coding: bool) -> bool {
    match tier {
        t if t < 0 => what == "whoami",
        0 => tier0_allows(what) || (tier0_coding && CODING_OPS.contains(&what)),
        1 => tier0_allows(what) || CODING_OPS.contains(&what) || TIER1_EXTRA.contains(&what),
        _ => true,
    }
}

/// The refusal for a member of `tier`: one line that names the tier and what it may use.
pub fn tier_refusal(tier: i64, what: &str, tier0_coding: bool) -> String {
    match tier {
        t if t < 0 => format!(
            "no tier: this key has no standing (revoked, or a finding of bad intent), it may not use {what}"
        ),
        0 if !tier0_coding => tier0_refusal(what),
        0 => format!(
            "tier 0 may not use {what}: a new key may only use {}, {}",
            TIER0_ALLOWED.join(", "),
            CODING_OPS.join(", ")
        ),
        _ => format!(
            "tier {tier} may not use {what}: a contributor may use the tier 0 requests, {}, {}",
            CODING_OPS.join(", "),
            TIER1_EXTRA.join(", ")
        ),
    }
}

/// The refusal for a tier 0 member, one line with the tier in it.
pub fn tier0_refusal(what: &str) -> String {
    format!(
        "tier 0 may not use {what}: a new key may only use {}",
        TIER0_ALLOWED.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier0_list_is_the_one_place() {
        for ok in ["whoami", "heartbeat", "log", "roster", "who", "claims"] {
            assert!(tier0_allows(ok), "{ok}");
        }
        assert!(tier0_allows("key.rotate") && tier0_allows("key.revoke"));
        for no in [
            "CHAT", "READY", "CLAIM", "TASK", "task.add", "idle", "send", "hello", "HELLO",
        ] {
            assert!(!tier0_allows(no), "{no}");
        }
        assert!(tier0_refusal("CHAT").contains("tier 0"));
    }

    #[test]
    fn tier_allows_table() {
        let coding = ["task.next", "task.work", "task.checkpoint", "task.done"];
        let more = ["task.stuck", "task.decline", "task.heartbeat"];
        let free = ["CHAT", "FINDING", "FIX", "READY", "STUCK", "HELLO"];
        let review = ["review.next", "review.verdict"];
        let other = ["task.add", "CLAIM", "TASK", "FACT", "idle", "quota.report"];
        // No tier: whoami only, with every switch.
        for sw in [false, true] {
            assert!(tier_allows(-1, "whoami", sw));
            for w in TIER0_ALLOWED.iter().filter(|w| **w != "whoami") {
                assert!(!tier_allows(-1, w, sw), "{w}");
            }
            for w in coding.iter().chain(&free).chain(&review) {
                assert!(!tier_allows(-1, w, sw), "{w}");
            }
        }
        // Tier 0: today's list, the order requests only with the switch.
        for w in TIER0_ALLOWED {
            assert!(tier_allows(0, w, false) && tier_allows(0, w, true), "{w}");
        }
        for w in coding.iter().chain(&more) {
            assert!(!tier_allows(0, w, false), "{w}");
            assert!(tier_allows(0, w, true), "{w}");
        }
        for w in free.iter().chain(&review).chain(&other) {
            assert!(!tier_allows(0, w, false) && !tier_allows(0, w, true), "{w}");
        }
        // Tier 1: all of that, the switch makes no difference.
        for sw in [false, true] {
            for w in TIER0_ALLOWED
                .iter()
                .chain(&coding)
                .chain(&more)
                .chain(&free)
                .chain(&review)
            {
                assert!(tier_allows(1, w, sw), "{w}");
            }
            for w in &other {
                assert!(!tier_allows(1, w, sw), "{w}");
            }
        }
        // Tier 2 and 3: everything a token member may do.
        for t in [2, 3] {
            for w in TIER0_ALLOWED
                .iter()
                .chain(&coding)
                .chain(&free)
                .chain(&other)
            {
                assert!(tier_allows(t, w, false), "{w}");
            }
        }
        assert!(tier_refusal(0, "CHAT", false).contains("tier 0"));
        assert!(tier_refusal(1, "CLAIM", false).contains("tier 1"));
        assert!(tier_refusal(-1, "CHAT", false).contains("no tier"));
    }
}
