//! The agent roster: the latest STATUS per participant and agent.
//!
//! A STATUS message has the topic `agent/<agent name>`, the body says in one line
//! what the agent works on, and refs may carry `state:running|idle|blocked|done`,
//! `model:<name>` and `branch:<name>`. Like claims, the roster is derived from
//! messages only, so the hub rebuilds it from the log on start.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::msg::{Kind, Message};

/// An entry without an update for this long is marked stale.
pub const STALE_MS: u64 = 20 * 60 * 1000;
/// An entry without an update for this long leaves the snapshot.
pub const DROP_MS: u64 = 24 * 60 * 60 * 1000;

pub const STATES: [&str; 4] = ["running", "idle", "blocked", "done"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Agent {
    pub owner: String,
    pub agent: String,
    pub state: String,
    pub model: String,
    pub branch: String,
    pub text: String,
    pub ts: String,
    /// Time of the last row whose state or text differed from the row before it.
    /// A row refreshed with the same state and text keeps the old value.
    pub changed: String,
    pub id: u64,
    pub stale: bool,
    #[serde(skip)]
    pub ts_ms: u64,
}

/// Checks topic and refs of a STATUS before it is stored.
pub fn check(topic: &str, refs: &[String]) -> Result<(), String> {
    let agent = topic.strip_prefix("agent/").unwrap_or_default();
    if agent.is_empty() || agent.len() > 64 {
        return Err("STATUS needs the topic agent/<agent name> (1 to 64 characters)".into());
    }
    for r in refs {
        if let Some(s) = r.strip_prefix("state:")
            && !STATES.contains(&s)
        {
            return Err(format!(
                "unknown state `{s}`, use one of {}",
                STATES.join(", ")
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Roster {
    map: BTreeMap<(String, String), Agent>,
}

impl Roster {
    /// Applies a stored message. Anything but a well formed STATUS is ignored.
    pub fn apply(&mut self, m: &Message) {
        if m.kind != Kind::Status {
            return;
        }
        let Some(agent) = m.topic.strip_prefix("agent/").filter(|a| !a.is_empty()) else {
            return;
        };
        let key = (m.from.clone(), agent.to_owned());
        let mut e = Agent {
            owner: m.from.clone(),
            agent: agent.to_owned(),
            state: "running".into(),
            model: String::new(),
            branch: String::new(),
            text: m.body.clone(),
            ts: m.ts.clone(),
            changed: m.ts.clone(),
            id: m.id,
            stale: false,
            ts_ms: crate::time::parse_ms(&m.ts).unwrap_or_else(crate::time::now_ms),
        };
        for r in &m.refs {
            if let Some(v) = r.strip_prefix("state:") {
                e.state = v.into();
            } else if let Some(v) = r.strip_prefix("model:") {
                e.model = v.into();
            } else if let Some(v) = r.strip_prefix("branch:") {
                e.branch = v.into();
            }
        }
        if let Some(prev) = self.map.get(&key)
            && prev.state == e.state
            && prev.text == e.text
        {
            e.changed = prev.changed.clone();
        }
        if e.state == "done" {
            self.map.remove(&key);
        } else {
            self.map.insert(key, e);
        }
    }

    /// Entries updated within the last 24 hours, `stale` set after 20 minutes,
    /// ordered by owner and agent. `owner` limits it to one participant.
    pub fn snapshot(&self, now_ms: u64, owner: Option<&str>) -> Vec<Agent> {
        self.map
            .values()
            .filter(|a| owner.is_none_or(|o| a.owner == o))
            .filter(|a| now_ms.saturating_sub(a.ts_ms) <= DROP_MS)
            .map(|a| Agent {
                stale: now_ms.saturating_sub(a.ts_ms) > STALE_MS,
                ..a.clone()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::Role;
    use crate::time::rfc3339;

    fn status(id: u64, from: &str, agent: &str, body: &str, refs: &[&str], ms: u64) -> Message {
        Message {
            id,
            ts: rfc3339(ms),
            from: from.into(),
            role: Role::Orchestrator,
            kind: Kind::Status,
            topic: format!("agent/{agent}"),
            body: body.into(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
            to: vec![],
        }
    }

    #[test]
    fn latest_wins_done_drops_stale_and_old() {
        let t0 = 1_800_000_000_000;
        let mut r = Roster::default();
        r.apply(&status(
            1,
            "v0",
            "p3",
            "shaders",
            &["model:sonnet", "branch:b1"],
            t0,
        ));
        r.apply(&status(
            2,
            "v0",
            "p3",
            "phi placement",
            &["state:blocked"],
            t0 + 1000,
        ));
        r.apply(&status(3, "v0", "p4", "audio", &[], t0));
        r.apply(&status(4, "alice", "p3", "linux run", &["state:idle"], t0));
        let s = r.snapshot(t0 + 2000, None);
        assert_eq!(s.len(), 3);
        let p3 = s
            .iter()
            .find(|a| a.owner == "v0" && a.agent == "p3")
            .unwrap();
        assert_eq!(
            (p3.state.as_str(), p3.text.as_str(), p3.model.as_str()),
            ("blocked", "phi placement", "")
        );
        assert_eq!(r.snapshot(t0, Some("alice")).len(), 1);

        r.apply(&status(5, "v0", "p4", "", &["state:done"], t0 + 3000));
        assert_eq!(r.snapshot(t0 + 3000, Some("v0")).len(), 1);

        let later = r.snapshot(t0 + STALE_MS + 5000, None);
        assert!(later.iter().all(|a| a.stale));
        assert!(r.snapshot(t0 + DROP_MS + 5000, None).is_empty());
    }

    #[test]
    fn changed_moves_only_when_state_or_text_differ() {
        let t0 = 1_800_000_000_000;
        let mut r = Roster::default();
        r.apply(&status(1, "g", "main", "waiting", &["state:idle"], t0));
        r.apply(&status(
            2,
            "g",
            "main",
            "waiting",
            &["state:idle"],
            t0 + 60_000,
        ));
        let a = &r.snapshot(t0 + 61_000, None)[0];
        assert_eq!(a.ts, rfc3339(t0 + 60_000));
        assert_eq!(a.changed, rfc3339(t0));
        r.apply(&status(
            3,
            "g",
            "main",
            "waiting",
            &["state:running"],
            t0 + 120_000,
        ));
        assert_eq!(
            r.snapshot(t0 + 121_000, None)[0].changed,
            rfc3339(t0 + 120_000)
        );
        r.apply(&status(
            4,
            "g",
            "main",
            "other text",
            &["state:running"],
            t0 + 180_000,
        ));
        assert_eq!(
            r.snapshot(t0 + 181_000, None)[0].changed,
            rfc3339(t0 + 180_000)
        );
    }

    #[test]
    fn checks() {
        assert!(check("agent/p3", &["state:running".into()]).is_ok());
        assert!(check("agent/", &[]).is_err());
        assert!(check("issue/3", &[]).is_err());
        assert!(check("agent/p3", &["state:sleeping".into()]).is_err());
    }
}
