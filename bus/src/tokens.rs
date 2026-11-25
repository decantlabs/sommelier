//! The member token file (`tokens.json` in the hub data folder).
//!
//! Only the hub owner edits it, with `sommelier token add|rotate|revoke|list`. The
//! hub rereads it when it changes, so a revoked or rotated token is cut off
//! within seconds.
//!
//! The file holds a salted SHA 256 of each token, never the token itself: the
//! token is printed once when it is issued. Tokens carry 192 random bits, so a
//! fast hash is enough (no password stretching needed). Files written by the
//! first version held plain tokens in a `token` field: they are still read, and
//! converted to hashes the next time a token command saves the file.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::msg::Role;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub role: Role,
    /// Hex of 16 random bytes.
    #[serde(default)]
    pub salt: String,
    /// Hex of SHA 256 over the salt bytes followed by the token.
    #[serde(default)]
    pub hash: String,
    pub created: String,
    /// Plain token from a file of the first version, converted on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl Member {
    fn matches(&self, token: &str) -> bool {
        match hash_token(&self.salt, token) {
            Some(h) => ct_eq(h.as_bytes(), self.hash.as_bytes()),
            None => false,
        }
    }

    fn set_token(&mut self, token: &str) -> Result<(), String> {
        self.salt = hex(&random::<16>()?);
        self.hash = hash_token(&self.salt, token).ok_or("bad salt")?;
        self.token = None;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenFile {
    pub members: Vec<Member>,
    /// How many plain tokens were converted to hashes while loading.
    #[serde(skip)]
    pub converted: usize,
}

impl TokenFile {
    /// A missing file is an empty table. Plain tokens are hashed in memory.
    pub fn load(path: &Path) -> io::Result<TokenFile> {
        let mut t: TokenFile = match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => TokenFile::default(),
            Err(e) => return Err(e),
        };
        for m in &mut t.members {
            if let Some(plain) = m.token.clone() {
                m.set_token(&plain).map_err(io::Error::other)?;
                t.converted += 1;
            }
        }
        Ok(t)
    }

    /// Writes a temporary file and renames it over the old one.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, text + "\n")?;
        std::fs::rename(&tmp, path)
    }

    /// The member owning `token`. Checks every entry, so the time does not tell which matched.
    pub fn lookup(&self, token: &str) -> Option<&Member> {
        let mut found = None;
        for m in &self.members {
            if m.matches(token) {
                found = Some(m);
            }
        }
        found
    }

    /// Case insensitive: names are stored in lower case.
    pub fn get(&self, name: &str) -> Option<&Member> {
        let name = name.to_ascii_lowercase();
        self.members.iter().find(|m| m.name == name)
    }

    /// Adds a member and returns the new token. Names are case insensitive and
    /// stored in lower case (`Alice` becomes `alice`).
    pub fn add(&mut self, name: &str, role: Role) -> Result<String, String> {
        let name = name.to_ascii_lowercase();
        if !valid_name(&name) {
            return Err(format!(
                "`{name}` is not a valid name: 1 to 32 characters from a to z (any case), 0 to 9, `_`, `.` and `-`, and not `hub`"
            ));
        }
        if role == Role::Hub {
            return Err("the hub role cannot be issued".into());
        }
        if self.get(&name).is_some() {
            return Err(format!(
                "`{name}` already has a token, use `sommelier token rotate {name}` for a new one"
            ));
        }
        let token = new_token()?;
        let mut m = Member {
            name,
            role,
            salt: String::new(),
            hash: String::new(),
            created: crate::time::rfc3339(crate::time::now_ms()),
            token: None,
        };
        m.set_token(&token)?;
        self.members.push(m);
        Ok(token)
    }

    /// Replaces the token of `name`, keeps its role. The old token stops working.
    pub fn rotate(&mut self, name: &str) -> Result<(String, Role), String> {
        let name = name.to_ascii_lowercase();
        let m = self
            .members
            .iter_mut()
            .find(|m| m.name == name)
            .ok_or_else(|| format!("no token for `{name}`"))?;
        let token = new_token()?;
        m.set_token(&token)?;
        Ok((token, m.role))
    }

    pub fn revoke(&mut self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        let before = self.members.len();
        self.members.retain(|m| m.name != name);
        before != self.members.len()
    }
}

fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| format!("no random numbers from the OS: {e}"))?;
    Ok(b)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Hex SHA 256 of the salt bytes followed by the token, `None` for a malformed salt.
fn hash_token(salt_hex: &str, token: &str) -> Option<String> {
    let salt = unhex(salt_hex).filter(|s| !s.is_empty())?;
    let mut h = Sha256::new();
    h.update(&salt);
    h.update(token.as_bytes());
    Some(hex(&h.finalize()))
}

/// `sb_` and 48 hex digits (192 random bits).
pub fn new_token() -> Result<String, String> {
    Ok(format!("sb_{}", hex(&random::<24>()?)))
}

pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name != "hub"
        && name.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-')
        })
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_lookup_revoke() {
        let mut t = TokenFile::default();
        let tok = t.add("alice", Role::Member).unwrap();
        assert!(tok.starts_with("sb_") && tok.len() == 51);
        assert_eq!(t.lookup(&tok).unwrap().name, "alice");
        assert!(t.lookup("sb_wrong").is_none());
        assert!(t.add("alice", Role::Member).is_err());
        assert!(
            t.add("ALICE", Role::Member).is_err(),
            "same name in another case"
        );
        let up = t.add("Rex.Two", Role::Observer).unwrap();
        assert_eq!(t.lookup(&up).unwrap().name, "rex.two");
        assert!(t.revoke("REX.TWO"));
        assert!(t.add("hub", Role::Member).is_err());
        assert!(t.add("Bad Name", Role::Member).is_err());
        assert!(t.add("x", Role::Hub).is_err());
        assert!(t.revoke("alice"));
        assert!(t.lookup(&tok).is_none());
        assert!(!t.revoke("alice"));
    }

    #[test]
    fn file_holds_no_token_and_rotate_replaces_it() {
        let mut t = TokenFile::default();
        let tok = t.add("alice", Role::Member).unwrap();
        let text = serde_json::to_string(&t).unwrap();
        assert!(!text.contains(&tok), "{text}");
        assert!(!text.contains("\"token\""), "{text}");
        let back: TokenFile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.lookup(&tok).unwrap().name, "alice");

        let (new, role) = t.rotate("Alice").unwrap();
        assert_eq!(role, Role::Member);
        assert_ne!(new, tok);
        assert!(t.lookup(&tok).is_none());
        assert_eq!(t.lookup(&new).unwrap().name, "alice");
        assert!(t.rotate("nobody").is_err());
    }

    #[test]
    fn plain_tokens_of_version_one_are_converted() {
        let dir = std::env::temp_dir().join(format!("sommelier-tok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokens.json");
        std::fs::write(
            &path,
            r#"{"members":[{"name":"old","role":"member","token":"sb_plain","created":"x"}]}"#,
        )
        .unwrap();
        let t = TokenFile::load(&path).unwrap();
        assert_eq!(t.converted, 1);
        assert_eq!(t.lookup("sb_plain").unwrap().name, "old");
        t.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sb_plain"), "{text}");
        assert_eq!(
            TokenFile::load(&path)
                .unwrap()
                .lookup("sb_plain")
                .unwrap()
                .name,
            "old"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
