//! Ed25519 keys, member ids, canonical signing and the proof of work of a join.
//!
//! Everything here is plain: no clock, no file, no network. The caller passes the
//! nonce and the key in.

use data_encoding::{BASE32_NOPAD, BASE64URL_NOPAD};
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// First line of the text a connection signs to prove its key.
pub const AUTH_TAG: &str = "sommelier-auth-v2";

/// The key pair of one member. Debug never shows the private part.
pub struct MemberKey {
    pair: Ed25519KeyPair,
    pkcs8: Vec<u8>,
}

impl std::fmt::Debug for MemberKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MemberKey({})", self.public_b64())
    }
}

impl MemberKey {
    /// A fresh key pair from the OS random source.
    pub fn generate() -> Result<MemberKey, String> {
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| "no random numbers from the OS to make a key".to_string())?;
        MemberKey::from_pkcs8(doc.as_ref())
    }

    /// Reads a key pair from its PKCS8 bytes.
    pub fn from_pkcs8(bytes: &[u8]) -> Result<MemberKey, String> {
        let pair = Ed25519KeyPair::from_pkcs8(bytes)
            .map_err(|_| "the key file does not hold a valid Ed25519 key".to_string())?;
        Ok(MemberKey {
            pair,
            pkcs8: bytes.to_vec(),
        })
    }

    /// The private key as PKCS8 bytes, for the key file only. Never print it.
    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// The 32 byte public key.
    pub fn public(&self) -> Vec<u8> {
        self.pair.public_key().as_ref().to_vec()
    }

    /// The public key as base64url without padding.
    pub fn public_b64(&self) -> String {
        b64(self.pair.public_key().as_ref())
    }

    /// Signs raw bytes.
    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.pair.sign(msg).as_ref().to_vec()
    }
}

/// Base64url without padding.
pub fn b64(bytes: &[u8]) -> String {
    BASE64URL_NOPAD.encode(bytes)
}

/// Reads base64url without padding.
pub fn unb64(text: &str) -> Result<Vec<u8>, String> {
    BASE64URL_NOPAD
        .decode(text.as_bytes())
        .map_err(|_| "not valid base64url".to_string())
}

/// The member id: `m` plus the first 16 characters of the lower case base32 (no padding)
/// of the SHA 256 of the FIRST public key of the member. It never changes on rotation.
pub fn member_id(first_public: &[u8]) -> String {
    let digest = Sha256::digest(first_public);
    let text = BASE32_NOPAD.encode(&digest).to_ascii_lowercase();
    format!("m{}", &text[..16])
}

/// True for a string that has the form of a member id.
pub fn is_member_id(s: &str) -> bool {
    s.len() == 17
        && s.starts_with('m')
        && s[1..]
            .bytes()
            .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c))
}

/// A handle: 1 to 32 characters, letters, digits, dot, dash, underscore.
pub fn valid_handle(handle: &str) -> bool {
    (1..=32).contains(&handle.len())
        && handle
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
}

/// The bytes that a signature covers: the compact JSON of `value` with the keys sorted in
/// every object and without the top level field `sig`.
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut v = value.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("sig");
    }
    v.sort_all_objects();
    v.to_string().into_bytes()
}

/// Signs the canonical form of `value` and returns the signature as base64url.
pub fn sign_request(key: &MemberKey, value: &Value) -> String {
    b64(&key.sign(&canonical_bytes(value)))
}

/// Checks the field `sig` of `value` against `public` (32 bytes).
pub fn verify_request(value: &Value, public: &[u8]) -> Result<(), String> {
    let sig = value
        .get("sig")
        .and_then(Value::as_str)
        .ok_or("the request has no `sig`")?;
    let sig = unb64(sig).map_err(|e| format!("`sig` is {e}"))?;
    verify_bytes(public, &canonical_bytes(value), &sig)
}

/// Checks a signature over raw bytes.
pub fn verify_bytes(public: &[u8], msg: &[u8], sig: &[u8]) -> Result<(), String> {
    UnparsedPublicKey::new(&ED25519, public)
        .verify(msg, sig)
        .map_err(|_| "the signature does not match the key".to_string())
}

/// The text a connection signs: the tag, the hub id, the address the client connected to (`host:port`,
/// see `addr::address_of_url`) and the nonce, one per line.
pub fn auth_text(hub_id: &str, address: &str, nonce: &str) -> String {
    format!("{AUTH_TAG}\n{hub_id}\n{address}\n{nonce}")
}

/// True when SHA 256 over the nonce text, the public key and the counter (8 bytes, big
/// endian) starts with at least `bits` zero bits.
pub fn pow_ok(nonce: &str, public: &[u8], counter: u64, bits: u32) -> bool {
    let mut h = Sha256::new();
    h.update(nonce.as_bytes());
    h.update(public);
    h.update(counter.to_be_bytes());
    leading_zero_bits(&h.finalize()) >= bits
}

fn leading_zero_bits(digest: &[u8]) -> u32 {
    let mut n = 0;
    for b in digest {
        if *b == 0 {
            n += 8;
        } else {
            n += b.leading_zeros();
            break;
        }
    }
    n
}

/// The smallest counter that satisfies [`pow_ok`].
pub fn solve_pow(nonce: &str, public: &[u8], bits: u32) -> u64 {
    (0u64..)
        .find(|c| pow_ok(nonce, public, *c, bits))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn member_id_is_stable_and_has_the_documented_form() {
        let public = [7u8; 32];
        let id = member_id(&public);
        assert_eq!(id, member_id(&public));
        assert_eq!(id.len(), 17);
        assert!(is_member_id(&id), "{id}");
        assert_ne!(id, member_id(&[8u8; 32]));
        let expected = BASE32_NOPAD
            .encode(&Sha256::digest(public))
            .to_ascii_lowercase();
        assert_eq!(id, format!("m{}", &expected[..16]));
        assert!(!is_member_id("m"));
        assert!(!is_member_id("M0123456789abcdef"));
        assert!(!is_member_id("alice"));
    }

    #[test]
    fn signature_verifies_and_fails_after_any_change() {
        let key = MemberKey::generate().unwrap();
        let mut req = json!({"b": 2, "a": {"z": 1, "y": [1, 2]}, "op": "x"});
        let sig = sign_request(&key, &req);
        req["sig"] = json!(sig);
        verify_request(&req, &key.public()).unwrap();

        // The order of fields does not matter, a changed field does.
        let reordered: Value = serde_json::from_str(&format!(
            r#"{{"sig":"{sig}","op":"x","a":{{"y":[1,2],"z":1}},"b":2}}"#
        ))
        .unwrap();
        verify_request(&reordered, &key.public()).unwrap();
        for (field, new) in [("b", json!(3)), ("op", json!("y")), ("extra", json!(1))] {
            let mut bad = req.clone();
            bad[field] = new;
            assert!(verify_request(&bad, &key.public()).is_err(), "{field}");
        }
        let mut bad = req.clone();
        bad["a"]["z"] = json!(9);
        assert!(verify_request(&bad, &key.public()).is_err());

        let other = MemberKey::generate().unwrap();
        assert!(verify_request(&req, &other.public()).is_err());
        let mut no_sig = req.clone();
        no_sig.as_object_mut().unwrap().remove("sig");
        assert!(verify_request(&no_sig, &key.public()).is_err());
    }

    #[test]
    fn canonical_form_is_compact_sorted_and_without_sig() {
        let v = json!({"b": 1, "a": {"d": 1, "c": 2}, "sig": "x"});
        assert_eq!(
            String::from_utf8(canonical_bytes(&v)).unwrap(),
            r#"{"a":{"c":2,"d":1},"b":1}"#
        );
    }

    #[test]
    fn key_round_trips_through_pkcs8_and_hides_itself() {
        let key = MemberKey::generate().unwrap();
        let back = MemberKey::from_pkcs8(key.pkcs8()).unwrap();
        assert_eq!(back.public(), key.public());
        assert!(!format!("{key:?}").contains(&b64(key.pkcs8())));
        assert!(MemberKey::from_pkcs8(b"nope").is_err());
    }

    #[test]
    fn proof_of_work_accepts_a_solved_counter_and_refuses_a_wrong_one() {
        let public = [3u8; 32];
        let counter = solve_pow("nonce-1", &public, 10);
        assert!(pow_ok("nonce-1", &public, counter, 10));
        let wrong = (0u64..)
            .find(|c| !pow_ok("nonce-1", &public, *c, 10))
            .unwrap();
        assert!(!pow_ok("nonce-1", &public, wrong, 10));
        assert!(pow_ok("nonce-1", &public, wrong, 0), "zero bits accept all");
        assert!(!pow_ok("nonce-1", &public, counter, 300));
    }

    #[test]
    fn handles() {
        for ok in ["a", "Alice_1", "x.y-z", &"a".repeat(32)] {
            assert!(valid_handle(ok), "{ok}");
        }
        for bad in ["", "a b", "ä", "a/b", &"a".repeat(33)] {
            assert!(!valid_handle(bad), "{bad}");
        }
    }
}
