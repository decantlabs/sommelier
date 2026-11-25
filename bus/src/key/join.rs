//! The hub side of a join: the rules, the nonces and the checks of a join request.
//!
//! Nothing here reads a clock or a file. The hub passes `now` in.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use super::crypto::{pow_ok, unb64, valid_handle, verify_request};

/// Largest proof of work the hub accepts as a setting. 40 bits is hours of CPU.
pub const MAX_POW_BITS: u32 = 40;

/// How long a nonce lives, in milliseconds.
pub const NONCE_TTL_MS: u64 = 60_000;

/// Most nonces held at once, so a flood of challenges cannot fill the memory.
const MAX_OPEN_NONCES: usize = 10_000;

/// Whether the hub takes new members, and how hard it makes the join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinRules {
    pub enabled: bool,
    pub proof_of_work_bits: u32,
}

impl Default for JoinRules {
    fn default() -> Self {
        JoinRules {
            enabled: false,
            proof_of_work_bits: 20,
        }
    }
}

/// The nonces the hub handed out. A nonce lives a short time and is used once.
#[derive(Debug, Default)]
pub struct Challenges {
    open: Mutex<HashMap<String, u64>>,
}

impl Challenges {
    /// A new random nonce and the time it expires (epoch ms).
    pub fn issue(&self, now_ms: u64, ttl_ms: u64) -> Result<(String, u64), String> {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).map_err(|e| format!("no random numbers from the OS: {e}"))?;
        let nonce: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let expires = now_ms.saturating_add(ttl_ms);
        let mut open = crate::lock(&self.open);
        if open.len() >= MAX_OPEN_NONCES {
            open.retain(|_, exp| *exp > now_ms);
        }
        if open.len() >= MAX_OPEN_NONCES {
            return Err("too many open challenges, try again in a minute".into());
        }
        open.insert(nonce.clone(), expires);
        Ok((nonce, expires))
    }

    /// Takes the nonce out. Fails for a nonce that is unknown, used or expired.
    pub fn consume(&self, nonce: &str, now_ms: u64) -> Result<(), String> {
        let expires = crate::lock(&self.open).remove(nonce);
        match expires {
            Some(exp) if exp > now_ms => Ok(()),
            _ => Err(
                "the nonce is unknown, used or expired: fetch a new one from /api/challenge".into(),
            ),
        }
    }

    /// Number of nonces held.
    pub fn len(&self) -> usize {
        crate::lock(&self.open).len()
    }

    /// True without a nonce.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The fields of a join request, parsed.
#[derive(Clone, Debug)]
pub struct JoinRequest {
    pub public_b64: String,
    pub public: Vec<u8>,
    pub handle: String,
    pub nonce: String,
    pub counter: u64,
}

/// Reads the fields of a join body: `pub`, `handle`, `nonce`, `counter`, `sig`.
pub fn parse_join(body: &Value) -> Result<JoinRequest, String> {
    let text = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("the join request needs the text field `{k}`"))
    };
    let public_b64 = text("pub")?.to_owned();
    let public = unb64(&public_b64).map_err(|e| format!("`pub` is {e}"))?;
    if public.len() != 32 {
        return Err("`pub` must be an Ed25519 public key of 32 bytes in base64url".into());
    }
    let handle = text("handle")?.to_owned();
    if !valid_handle(&handle) {
        return Err(
            "`handle` must have 1 to 32 characters: letters, digits, `.`, `-` and `_`".into(),
        );
    }
    let nonce = text("nonce")?.to_owned();
    text("sig")?;
    let counter = body
        .get("counter")
        .and_then(Value::as_u64)
        .ok_or("the join request needs the number field `counter`")?;
    Ok(JoinRequest {
        public_b64,
        public,
        handle,
        nonce,
        counter,
    })
}

/// Checks the proof of work and the signature of a parsed join.
pub fn verify_join(rules: &JoinRules, req: &JoinRequest, body: &Value) -> Result<(), String> {
    if !pow_ok(
        &req.nonce,
        &req.public,
        req.counter,
        rules.proof_of_work_bits,
    ) {
        return Err(format!(
            "the proof of work is missing or wrong: {} leading zero bits are needed",
            rules.proof_of_work_bits
        ));
    }
    verify_request(body, &req.public).map_err(|e| format!("bad signature on the join: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_is_used_once_and_expires() {
        let c = Challenges::default();
        let (n, exp) = c.issue(1_000, NONCE_TTL_MS).unwrap();
        assert_eq!(exp, 61_000);
        assert_eq!(n.len(), 32);
        c.consume(&n, 2_000).unwrap();
        assert!(c.consume(&n, 2_000).is_err(), "used");
        let (n2, _) = c.issue(1_000, NONCE_TTL_MS).unwrap();
        assert!(c.consume(&n2, 61_000).is_err(), "expired");
        assert!(c.consume("nope", 0).is_err());
        assert!(c.is_empty());
    }

    #[test]
    fn rules_are_off_by_default() {
        let r = JoinRules::default();
        assert!(!r.enabled);
        assert_eq!(r.proof_of_work_bits, 20);
    }
}
