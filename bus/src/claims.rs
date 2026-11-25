//! The claim table: who holds which topic.
//!
//! A CLAIM on a free topic takes it, a CLAIM by the holder refreshes it, a CLAIM
//! by anyone else is refused. RELEASE by the holder or an orchestrator frees it.
//! Any message or heartbeat from the holder keeps all its claims alive. The hub
//! frees a claim whose holder stayed silent longer than the claim timeout.

use std::collections::BTreeMap;

use crate::msg::{Kind, Message, Role};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub holder: String,
    pub since_id: u64,
    pub since_ts: String,
    pub last_seen_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Claims {
    map: BTreeMap<String, Claim>,
}

impl Claims {
    pub fn check_claim(&self, topic: &str, from: &str) -> Result<(), String> {
        if topic.is_empty() {
            return Err("CLAIM needs a topic, for example issue/186".into());
        }
        match self.map.get(topic) {
            Some(c) if c.holder != from => Err(format!(
                "CLAIM refused: {topic} is held by {} since #{} ({}), ask them or wait for a RELEASE",
                c.holder, c.since_id, c.since_ts
            )),
            _ => Ok(()),
        }
    }

    pub fn check_release(&self, topic: &str, from: &str, role: Role) -> Result<(), String> {
        if topic.is_empty() {
            return Err("RELEASE needs a topic".into());
        }
        match self.map.get(topic) {
            None => Err(format!("RELEASE refused: {topic} is not claimed")),
            Some(c) if c.holder != from && role != Role::Orchestrator => Err(format!(
                "RELEASE refused: {topic} is held by {}, only the holder or an orchestrator may release it",
                c.holder
            )),
            _ => Ok(()),
        }
    }

    /// Applies a stored message. Only call it for messages that passed the checks.
    pub fn apply(&mut self, m: &Message, now_ms: u64) {
        match m.kind {
            Kind::Claim => {
                self.map.entry(m.topic.clone()).or_insert_with(|| Claim {
                    holder: m.from.clone(),
                    since_id: m.id,
                    since_ts: m.ts.clone(),
                    last_seen_ms: now_ms,
                });
            }
            Kind::Release => {
                self.map.remove(&m.topic);
            }
            _ => {}
        }
        self.touch(&m.from, now_ms);
    }

    /// Keeps every claim of `who` alive.
    pub fn touch(&mut self, who: &str, now_ms: u64) {
        for c in self.map.values_mut().filter(|c| c.holder == who) {
            c.last_seen_ms = now_ms;
        }
    }

    /// Claims silent for longer than `ttl_ms`, oldest topic name first.
    pub fn expired(&self, now_ms: u64, ttl_ms: u64) -> Vec<(String, Claim)> {
        self.map
            .iter()
            .filter(|(_, c)| now_ms.saturating_sub(c.last_seen_ms) > ttl_ms)
            .map(|(t, c)| (t.clone(), c.clone()))
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Claim)> {
        self.map.iter()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u64, from: &str, kind: Kind, topic: &str) -> Message {
        Message {
            id,
            ts: "t".into(),
            from: from.into(),
            role: Role::Member,
            kind,
            topic: topic.into(),
            body: String::new(),
            refs: vec![],
            to: vec![],
        }
    }

    #[test]
    fn claim_conflict_release_and_expiry() {
        let mut c = Claims::default();
        assert!(c.check_claim("", "a").is_err());
        c.check_claim("issue/1", "a").unwrap();
        c.apply(&msg(1, "a", Kind::Claim, "issue/1"), 1000);
        assert!(c.check_claim("issue/1", "a").is_ok());
        let e = c.check_claim("issue/1", "b").unwrap_err();
        assert!(e.contains("held by a since #1"), "{e}");
        assert!(c.check_release("issue/1", "b", Role::Member).is_err());
        assert!(c.check_release("issue/1", "b", Role::Orchestrator).is_ok());
        assert!(c.check_release("issue/2", "a", Role::Member).is_err());

        c.apply(&msg(2, "a", Kind::Chat, ""), 5000);
        assert!(c.expired(6000, 2000).is_empty());
        assert_eq!(c.expired(8000, 2000).len(), 1);

        c.apply(&msg(3, "a", Kind::Release, "issue/1"), 9000);
        assert!(c.is_empty());
    }
}
