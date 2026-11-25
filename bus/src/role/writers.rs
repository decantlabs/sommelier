//! Who may write what, in one place.
//!
//! Every message kind, every FACT topic prefix and every module op of the hub has one answer here. The hub
//! asks this table when it takes a message from a client (`publish_message`) and when a hook emits one
//! (`append_hook_message`). Every projection keeps its own check of the sender as the second guard, so an old
//! or hand edited log line cannot give anyone a lease, a landing, a stamp or capacity at replay.
//!
//! The table of the ops is the one place that says who may call a module op. The ops of the task and land
//! modules ask it at their entry. The ops of other modules keep their own checks, and `tests/writers.rs`
//! pins the answers of this table so a change of one shows in a test.

use rusqlite::Connection;

use crate::hub::HUB_ONLY_TOPICS;
use crate::msg::{Kind, Message, Role};

/// Who may write a message kind or a topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writer {
    /// Only the hub itself. A client is refused, a projection takes the line only from the role `hub`.
    Hub,
    /// The orchestrator (and the hub).
    Orchestrator,
    /// A member, and only for itself: the owner named in the topic is the sender.
    Own,
    /// Any member with a token that may send.
    Anyone,
}

/// Who may write a message of this kind. FACT is decided by the topic, see [`fact_writer`].
pub fn kind_writer(kind: Kind) -> Writer {
    match kind {
        Kind::Policy | Kind::Offer | Kind::TaskUpdate | Kind::Alert => Writer::Hub,
        Kind::Task => Writer::Orchestrator,
        Kind::Fact
        | Kind::Ready
        | Kind::Landed
        | Kind::Fix
        | Kind::Stuck
        | Kind::Finding
        | Kind::Claim
        | Kind::Release
        | Kind::Accept
        | Kind::Decline
        | Kind::RunRequest
        | Kind::RunResult
        | Kind::Hello
        | Kind::Chat
        | Kind::Status => Writer::Anyone,
    }
}

/// The topic prefixes of a FACT that a member writes for itself. The segment after the prefix names the owner.
pub const OWN_TOPICS: [&str; 3] = ["capacity/", "quota/", "spend/"];

/// Who may write a FACT on this topic.
pub fn fact_writer(topic: &str) -> Writer {
    if HUB_ONLY_TOPICS.iter().any(|p| topic.starts_with(p)) {
        Writer::Hub
    } else if OWN_TOPICS.iter().any(|p| topic.starts_with(p)) {
        Writer::Own
    } else {
        Writer::Anyone
    }
}

/// The owner a topic of [`OWN_TOPICS`] names, `None` for another topic.
pub fn topic_owner(topic: &str) -> Option<&str> {
    OWN_TOPICS
        .iter()
        .find_map(|p| topic.strip_prefix(p))
        .map(|rest| rest.split('/').next().unwrap_or(""))
}

/// True when the sender of `msg` is the hub itself.
pub fn is_hub(msg: &Message) -> bool {
    msg.role == Role::Hub
}

/// True when `msg` is a FACT of a member for itself, or of the hub. A projection of an own topic uses this as
/// the second guard.
pub fn own_topic_ok(msg: &Message) -> bool {
    if is_hub(msg) {
        return true;
    }
    topic_owner(&msg.topic).is_some_and(|owner| !owner.is_empty() && owner == msg.from)
}

/// The check of a message that a client sends. Returns the text of the refusal.
pub fn check_client(msg: &Message) -> Result<(), String> {
    match msg.role {
        Role::Observer => return Err("observers are read only, this token cannot send".into()),
        Role::Hub => return Err("the hub role cannot send".into()),
        _ => {}
    }
    check_kind_and_topic(msg)
}

/// The check of a message that a hook or a module op emits. The hub itself may emit every kind.
pub fn check_hook(msg: &Message) -> Result<(), String> {
    if is_hub(msg) {
        return Ok(());
    }
    check_kind_and_topic(msg)
}

fn check_kind_and_topic(msg: &Message) -> Result<(), String> {
    let kind = msg.kind;
    match kind_writer(kind) {
        Writer::Hub => {
            return Err(format!("only the hub may send {}", kind.as_str()));
        }
        Writer::Orchestrator if !matches!(msg.role, Role::Orchestrator | Role::Hub) => {
            return Err(format!("only an orchestrator may send {}", kind.as_str()));
        }
        _ => {}
    }
    if kind == Kind::Fact {
        match fact_writer(&msg.topic) {
            Writer::Hub => {
                let prefix = HUB_ONLY_TOPICS
                    .iter()
                    .find(|p| msg.topic.starts_with(**p))
                    .copied()
                    .unwrap_or("");
                return Err(format!(
                    "FACT topics under {prefix} are written by the hub only"
                ));
            }
            Writer::Own if !own_topic_ok(msg) => {
                return Err(format!(
                    "a member may write `{}` only for itself, the owner in the topic must be `{}`",
                    msg.topic, msg.from
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Who may call a module op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpWho {
    /// Any caller that may run ops. The op reads, or acts for the caller only.
    Anyone,
    /// The holder of the lease or the offer of the order, the orchestrator or the hub. The op checks it.
    Holder,
    /// The orchestrator or the hub.
    Orchestrator,
    /// A member for the rows it owns (its own runs). The orchestrator, the lander and the hub for every row.
    Own,
    /// The holder of the role `lander`, the orchestrator or the hub.
    Lander,
    /// The hub only, no client op.
    Hub,
}

/// Who may call a module op of the task, land, run, plan, pause, resume, role and alert families.
/// `None` for an op that another module owns and checks itself.
pub fn op_who(op: &str) -> Option<OpWho> {
    Some(match op {
        "task.add" | "task.cancel" | "task.requeue" => OpWho::Orchestrator,
        "task.checkpoint" | "task.done" | "task.stuck" | "task.handover" | "task.decline" => {
            OpWho::Holder
        }
        "task.list" | "task.show" | "task.next" | "task.work" | "task.offers" | "task.ask"
        | "task.heartbeat" => OpWho::Anyone,
        "land.result" | "land.next" => OpWho::Lander,
        "land.approve" => OpWho::Orchestrator,
        "land.branch_upsert" | "land.sweep" | "run.sweep" => OpWho::Lander,
        "run.request" | "run.register" | "run.result" => OpWho::Own,
        "land.enqueue" | "land.list" | "run.list" => OpWho::Anyone,
        "plan.dry" | "plan.run" | "plan.explain" => OpWho::Anyone,
        "plan.pause" | "plan.resume" | "pause" | "resume" => OpWho::Orchestrator,
        "role.list" | "role.check" => OpWho::Anyone,
        _ => return None,
    })
}

/// True when the member `name` holds the role `lander` in the ROLE table right now.
pub fn holds_role(conn: &Connection, role: &str, name: &str) -> bool {
    super::query_row(conn, role, "global")
        .ok()
        .flatten()
        .is_some_and(|row| {
            row.state == super::RoleState::Held && row.holder.as_deref() == Some(name)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, from: &str, kind: Kind, topic: &str) -> Message {
        Message {
            id: 0,
            ts: String::new(),
            from: from.into(),
            role,
            kind,
            topic: topic.into(),
            body: "{}".into(),
            refs: vec![],
            to: vec![],
        }
    }

    #[test]
    fn a_client_may_not_send_a_kind_of_the_hub() {
        for kind in [Kind::TaskUpdate, Kind::Offer, Kind::Alert, Kind::Policy] {
            for role in [Role::Member, Role::Orchestrator] {
                assert!(check_client(&msg(role, "a", kind, "task/t1")).is_err());
            }
        }
        assert!(check_client(&msg(Role::Member, "a", Kind::Task, "")).is_err());
        assert!(check_client(&msg(Role::Orchestrator, "o", Kind::Task, "")).is_ok());
        assert!(check_client(&msg(Role::Member, "a", Kind::Chat, "")).is_ok());
    }

    #[test]
    fn an_own_topic_names_the_sender() {
        let ok = msg(Role::Member, "alice", Kind::Fact, "capacity/alice/m1");
        assert!(check_client(&ok).is_ok());
        let bad = msg(Role::Member, "alice", Kind::Fact, "capacity/bob/m1");
        assert!(check_client(&bad).unwrap_err().contains("only for itself"));
        let none = msg(Role::Member, "alice", Kind::Fact, "capacity/");
        assert!(check_client(&none).is_err());
        let orch = msg(Role::Orchestrator, "o", Kind::Fact, "spend/bob/p");
        assert!(check_client(&orch).is_err());
    }

    #[test]
    fn the_hub_emits_everything_and_hooks_follow_the_table() {
        assert!(check_hook(&msg(Role::Hub, "hub", Kind::TaskUpdate, "task/t")).is_ok());
        assert!(check_hook(&msg(Role::Member, "a", Kind::Alert, "alert/X/y")).is_err());
        assert!(check_hook(&msg(Role::Member, "a", Kind::Fact, "quota/a/p/w")).is_ok());
        assert!(check_hook(&msg(Role::Member, "a", Kind::Fact, "quota/b/p/w")).is_err());
    }

    #[test]
    fn every_hub_only_prefix_is_a_hub_topic() {
        for p in HUB_ONLY_TOPICS {
            assert_eq!(fact_writer(&format!("{p}x")), Writer::Hub, "{p}");
        }
        assert_eq!(fact_writer("issue/1"), Writer::Anyone);
    }
}
