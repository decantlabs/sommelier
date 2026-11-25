//! The hub side of a join: the rules, the nonces and the checks of a join request.
//!
//! Nothing here reads a clock or a file. The hub passes `now` in.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::review::hmac_sha256;

use super::crypto::{pow_ok, unb64, valid_handle, verify_request};

/// Largest proof of work the hub accepts as a setting. 40 bits is hours of CPU.
pub const MAX_POW_BITS: u32 = 40;

/// How long a nonce lives, in milliseconds.
pub const NONCE_TTL_MS: u64 = 60_000;

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

/// Length of a nonce in characters: 16 for the expiry, 32 random, 32 for the mark.
const NONCE_LEN: usize = 80;

/// How often the set of used nonces drops the expired ones, in milliseconds.
const PURGE_EVERY_MS: u64 = 10_000;

/// The nonces of the hub. A nonce is made of its expiry time, a random part and an HMAC of
/// both under a secret of the hub, so the hub keeps nothing for a nonce it handed out. It
/// only remembers the nonces that were USED, until they expire, and that set grows with valid
/// signatures only.
#[derive(Debug)]
pub struct Challenges {
    secret: [u8; 32],
    used: Mutex<Used>,
}

#[derive(Debug, Default)]
struct Used {
    until: HashMap<String, u64>,
    purged_at: u64,
}

impl Default for Challenges {
    /// A hub secret that lives in memory only: nonces stop working at a restart.
    fn default() -> Self {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("the OS has random numbers");
        Challenges::with_secret(secret)
    }
}

impl Challenges {
    /// Nonces under the given hub secret. The secret is never logged or sent. A random boot value of
    /// this start goes into the key of the nonce MAC: the set of used nonces lives in memory only, so a
    /// header captured before a restart must not be valid after it (it is refused as unknown).
    pub fn with_secret(secret: [u8; 32]) -> Self {
        let mut boot = [0u8; 16];
        getrandom::fill(&mut boot).expect("the OS has random numbers");
        let mut input = b"nonce-boot-v1".to_vec();
        input.extend_from_slice(&boot);
        Challenges {
            secret: hmac_sha256(&secret, &input),
            used: Mutex::new(Used::default()),
        }
    }

    fn mark(&self, expires: u64, random: &str) -> String {
        let head = format!("{expires:016x}{random}");
        let mac = hmac_sha256(&self.secret, format!("nonce-v1{head}").as_bytes());
        let tag: String = mac[..16].iter().map(|b| format!("{b:02x}")).collect();
        format!("{head}{tag}")
    }

    /// A new random nonce and the time it expires (epoch ms).
    pub fn issue(&self, now_ms: u64, ttl_ms: u64) -> Result<(String, u64), String> {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).map_err(|e| format!("no random numbers from the OS: {e}"))?;
        let random: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let expires = now_ms.saturating_add(ttl_ms);
        Ok((self.mark(expires, &random), expires))
    }

    /// Checks that the hub made the nonce and that it has not expired. Uses nothing up.
    pub fn check(&self, nonce: &str, now_ms: u64) -> Result<u64, String> {
        let unknown = || {
            "the nonce is unknown, used or expired: fetch a new one from /api/challenge".to_string()
        };
        if nonce.len() != NONCE_LEN
            || !nonce
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(unknown());
        }
        let expires = u64::from_str_radix(&nonce[..16], 16).map_err(|_| unknown())?;
        let want = self.mark(expires, &nonce[16..48]);
        // Both sides are lower case hex of the same length: compare without an early exit.
        let same = want
            .bytes()
            .zip(nonce.bytes())
            .fold(0u8, |d, (a, b)| d | (a ^ b));
        if same != 0 {
            return Err(unknown());
        }
        if expires <= now_ms {
            return Err(unknown());
        }
        Ok(expires)
    }

    /// Marks a checked nonce as used. Fails when it was used before. Call it only after the
    /// signature was verified, so the set grows with valid signatures only.
    pub fn mark_used(&self, nonce: &str, now_ms: u64) -> Result<(), String> {
        let expires = self.check(nonce, now_ms)?;
        let mut used = crate::lock(&self.used);
        if now_ms.saturating_sub(used.purged_at) >= PURGE_EVERY_MS {
            used.until.retain(|_, exp| *exp > now_ms);
            used.purged_at = now_ms;
        }
        if used.until.insert(nonce.to_owned(), expires).is_some() {
            return Err(
                "the nonce is unknown, used or expired: fetch a new one from /api/challenge".into(),
            );
        }
        Ok(())
    }

    /// Checks and uses up a nonce in one step. Fails for a nonce that is unknown, used or
    /// expired.
    pub fn consume(&self, nonce: &str, now_ms: u64) -> Result<(), String> {
        self.mark_used(nonce, now_ms)
    }

    /// Number of used nonces held.
    pub fn len(&self) -> usize {
        crate::lock(&self.used).until.len()
    }

    /// True without a used nonce.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The front door's limit of joins per address block, the one call of the join path into it.
/// The refusal names the limit and the time to retry.
pub fn limit_join(door: &crate::door::Door, block: &str, now_ms: u64) -> Result<(), String> {
    door.charge_join(block, now_ms)
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
        assert_eq!(n.len(), NONCE_LEN);
        c.consume(&n, 2_000).unwrap();
        assert!(c.consume(&n, 2_000).is_err(), "used");
        let (n2, _) = c.issue(1_000, NONCE_TTL_MS).unwrap();
        assert!(c.consume(&n2, 61_000).is_err(), "expired");
        assert!(c.consume("nope", 0).is_err());
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn issuing_keeps_nothing_and_a_forged_nonce_fails() {
        let c = Challenges::default();
        for _ in 0..1_000 {
            c.issue(1_000, NONCE_TTL_MS).unwrap();
        }
        assert!(c.is_empty());
        // Another hub secret, a changed expiry and a changed random part are all refused.
        let other = Challenges::default();
        let (n, _) = c.issue(1_000, NONCE_TTL_MS).unwrap();
        assert!(other.check(&n, 2_000).is_err());
        let later = format!("{:016x}{}", 9_999_999u64, &n[16..]);
        assert!(c.check(&later, 2_000).is_err());
        let flipped = format!("{}{}", &n[..20], if &n[20..21] == "0" { "1" } else { "0" });
        assert!(c.check(&format!("{flipped}{}", &n[21..]), 2_000).is_err());
        c.check(&n, 2_000).unwrap();
    }

    #[test]
    fn a_nonce_from_before_a_restart_is_refused() {
        let secret = [9u8; 32];
        let before = Challenges::with_secret(secret);
        let (nonce, _) = before.issue(1_000, NONCE_TTL_MS).unwrap();
        before.check(&nonce, 2_000).unwrap();
        // The same secret file, a new start: the used set is empty, the nonce must not work.
        let after = Challenges::with_secret(secret);
        assert!(after.check(&nonce, 2_000).is_err());
        assert!(after.consume(&nonce, 2_000).is_err());
        let (fresh, _) = after.issue(1_000, NONCE_TTL_MS).unwrap();
        after.consume(&fresh, 2_000).unwrap();
    }

    #[test]
    fn rules_are_off_by_default() {
        let r = JoinRules::default();
        assert!(!r.enabled);
        assert_eq!(r.proof_of_work_bits, 20);
    }
}
