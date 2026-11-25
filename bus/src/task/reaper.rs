//! Lease reaper and offer expiration (section 4.2, step 8).

use rusqlite::Connection;
use serde_json::json;

use crate::msg::{Kind, Message, Role};
use crate::time::rfc3339;

use super::schema::{latest_checkpoint, query_leases, query_offers, query_task};

/// Timers controlling lease suspicion and expiration (section 4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseTimers {
    pub suspect_sec: u64,
    pub dead_supervised_sec: u64,
    pub lease_min: u64,
    pub restart_grace_sec: u64,
}

impl Default for LeaseTimers {
    fn default() -> Self {
        Self {
            suspect_sec: 120,
            dead_supervised_sec: 180,
            lease_min: 45,
            restart_grace_sec: 120,
        }
    }
}

/// Evaluates active/suspect leases and open offers, returning `TASK_UPDATE`, `OFFER`, and `ALERT`
/// messages for the hub to stamp, append, project, and broadcast (section 4.2).
pub fn reap_leases_and_offers(
    conn: &Connection,
    now_sec: u64,
    hub_started_at: u64,
    timers: LeaseTimers,
) -> Result<Vec<Message>, String> {
    let mut out = Vec::new();
    let ts = rfc3339(now_sec * 1000);

    for lease in query_leases(conn)? {
        if !matches!(lease.state.as_str(), "active" | "suspect") {
            continue;
        }
        let silent = now_sec.saturating_sub(lease.heartbeat_at);
        let limit = if lease.supervised {
            timers.dead_supervised_sec
        } else {
            timers.lease_min.saturating_mul(60)
        };
        let expired = lease.expires_at > 0 && now_sec >= lease.expires_at;

        if (silent > limit || expired)
            && now_sec > hub_started_at.saturating_add(timers.restart_grace_sec)
        {
            let Some(task) = query_task(conn, &lease.task_id)? else {
                continue;
            };
            let cp = latest_checkpoint(conn, &lease.task_id, Some(lease.attempt))?
                .or(latest_checkpoint(conn, &lease.task_id, None)?);
            let mut resume_note = cp
                .as_ref()
                .map(|c| c.note.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| task.resume_note.clone());
            if resume_note.is_empty() {
                resume_note = format!("requeued after lease lost (attempt {})", lease.attempt);
            }
            let resume_sha = cp
                .as_ref()
                .map(|c| c.sha.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| task.resume_sha.clone());
            let next_attempt = task.attempt.saturating_add(1);

            let update_body = json!({
                "v": 1,
                "task": task.id,
                "prev": task.state,
                "state": "ready",
                "attempt": next_attempt,
                "by": "hub",
                "reason": "gone",
                "resume_note": resume_note,
                "resume_sha": resume_sha,
                "lease": {
                    "task_id": lease.task_id,
                    "attempt": lease.attempt,
                    "owner": lease.owner,
                    "worker": lease.worker,
                    "machine": lease.machine,
                    "class": lease.class,
                    "pools": lease.pools,
                    "granted_at": lease.granted_at,
                    "heartbeat_at": lease.heartbeat_at,
                    "expires_at": lease.expires_at,
                    "est_minutes_p90": lease.est_minutes_p90,
                    "est_cost": lease.est_cost,
                    "est_end_at": lease.est_end_at,
                    "state": "released",
                    "end_reason": "gone",
                    "supervised": lease.supervised,
                },
                "checkpoint": cp,
            });
            out.push(Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{}", task.id),
                body: update_body.to_string(),
                refs: vec!["state:ready".into(), "reason:gone".into()],
                to: Vec::new(),
            });

            let alert_body = json!({
                "v": 1,
                "code": "LEASE_LOST",
                "key": task.id,
                "severity": "warn",
                "state": "open",
                "opened_at": now_sec,
                "last_at": now_sec,
                "count": 1,
                "text": format!(
                    "lease lost for {} (owner {}, worker {}, silent {} s)",
                    task.id, lease.owner, lease.worker, silent
                ),
                "data": {
                    "task_id": task.id,
                    "attempt": lease.attempt,
                    "owner": lease.owner,
                    "worker": lease.worker,
                    "silent_secs": silent,
                },
            });
            out.push(Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::Alert,
                topic: format!("alert/LEASE_LOST/{}", task.id),
                body: alert_body.to_string(),
                refs: vec!["state:open".into()],
                to: vec!["coordinator".into()],
            });
        } else if silent > timers.suspect_sec && lease.state == "active" {
            let Some(task) = query_task(conn, &lease.task_id)? else {
                continue;
            };
            let update_body = json!({
                "v": 1,
                "task": task.id,
                "prev": task.state,
                "state": task.state,
                "attempt": task.attempt,
                "by": "hub",
                "reason": "heartbeat_late",
                "lease": {
                    "task_id": lease.task_id,
                    "attempt": lease.attempt,
                    "owner": lease.owner,
                    "worker": lease.worker,
                    "machine": lease.machine,
                    "class": lease.class,
                    "pools": lease.pools,
                    "granted_at": lease.granted_at,
                    "heartbeat_at": lease.heartbeat_at,
                    "expires_at": lease.expires_at,
                    "est_minutes_p90": lease.est_minutes_p90,
                    "est_cost": lease.est_cost,
                    "est_end_at": lease.est_end_at,
                    "state": "suspect",
                    "end_reason": lease.end_reason,
                    "supervised": lease.supervised,
                },
            });
            out.push(Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{}", task.id),
                body: update_body.to_string(),
                refs: vec!["lease:suspect".into()],
                to: Vec::new(),
            });
        }
    }

    for offer in query_offers(conn)? {
        if offer.state != "open" || offer.expires_at == 0 || now_sec < offer.expires_at {
            continue;
        }
        let offer_body = json!({
            "v": 1,
            "offer_id": offer.id,
            "task": offer.task_id,
            "to_owner": offer.to_owner,
            "class": offer.class,
            "sent_at": offer.sent_at,
            "expires": offer.expires_at,
            "state": "expired",
            "reason": "offer_expired",
        });
        out.push(Message {
            id: 0,
            ts: ts.clone(),
            from: "hub".into(),
            role: Role::Hub,
            kind: Kind::Offer,
            topic: format!("task/{}", offer.task_id),
            body: offer_body.to_string(),
            refs: vec!["offer:expired".into()],
            to: vec![offer.to_owner.clone()],
        });

        if let Some(task) = query_task(conn, &offer.task_id)?
            && task.state == "offered"
        {
            let update_body = json!({
                "v": 1,
                "task": task.id,
                "prev": "offered",
                "state": "ready",
                "attempt": task.attempt,
                "by": "hub",
                "reason": "offer_expired",
            });
            out.push(Message {
                id: 0,
                ts: ts.clone(),
                from: "hub".into(),
                role: Role::Hub,
                kind: Kind::TaskUpdate,
                topic: format!("task/{}", task.id),
                body: update_body.to_string(),
                refs: vec!["state:ready".into()],
                to: Vec::new(),
            });
        }
    }

    Ok(out)
}
