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

/// True when a tier 0 member may use the request `op` (such as `whoami` or `task.add`) or send
/// the message kind (such as `CHAT`).
pub fn tier0_allows(op_or_kind: &str) -> bool {
    TIER0_ALLOWED.contains(&op_or_kind)
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
}
