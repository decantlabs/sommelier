//! IDLE alert state machine: one alert per idle slot, not repeated within 10 minutes,
//! closed when the slot works (step 1, section 4.1 and 5.2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::msg::{Kind, Message, Role};

/// Minimum repeat interval for an open IDLE alert (10 minutes).
pub const IDLE_REPEAT_MS: u64 = 10 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertRow {
    pub code: String,
    pub key: String,
    pub severity: String,
    pub to: String,
    pub state: String,
    pub opened_at: u64,
    pub last_at: u64,
    pub count: u64,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct IdleAlertTracker {
    pub alerts: BTreeMap<String, AlertRow>,
}

/// Slot observation passed to [`IdleAlertTracker::on_tick`].
#[derive(Clone, Debug)]
pub struct SlotStatus {
    /// Unique slot key, e.g. `owner/class/slot` or `owner/agent`.
    pub key: String,
    pub owner: String,
    pub class: String,
    pub is_idle: bool,
    pub idle_secs: u64,
    pub reason: Option<String>,
}

impl IdleAlertTracker {
    /// Rebuilds alert tracker state from an ALERT message in the log.
    pub fn apply(&mut self, msg: &Message, msg_ms: u64) {
        if msg.kind != Kind::Alert {
            return;
        }
        let Some(rest) = msg.topic.strip_prefix("alert/IDLE/") else {
            return;
        };
        let key = rest.to_owned();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.body) {
            let state = v["state"].as_str().unwrap_or("open").to_owned();
            let count = v["count"].as_u64().unwrap_or(1);
            let opened_at = v["opened_at"].as_u64().unwrap_or(msg_ms / 1000);
            let last_at = v["last_at"].as_u64().unwrap_or(msg_ms / 1000);
            let text = v["text"].as_str().unwrap_or(&msg.body).to_owned();
            let to = msg.to.first().cloned().unwrap_or_default();
            self.alerts.insert(
                key.clone(),
                AlertRow {
                    code: "IDLE".into(),
                    key,
                    severity: v["severity"].as_str().unwrap_or("warn").to_owned(),
                    to,
                    state,
                    opened_at,
                    last_at,
                    count,
                    text,
                },
            );
        }
    }

    /// Evaluates current slot statuses and emits ALERT messages when a slot crosses
    /// `idle_alert_sec` or when an alerted idle slot starts working again.
    ///
    /// Rules (Done when):
    /// - One IDLE alert when `is_idle && idle_secs >= idle_alert_sec`.
    /// - Not repeated within 10 minutes (`IDLE_REPEAT_MS`).
    /// - Closed when the slot works (`!is_idle`).
    pub fn evaluate(
        &mut self,
        slots: &[SlotStatus],
        idle_alert_sec: u64,
        coordinator: Option<&str>,
        now_ms: u64,
    ) -> Vec<Message> {
        let mut out = Vec::new();
        let now_sec = now_ms / 1000;
        let to_vec: Vec<String> = coordinator.map(|c| vec![c.to_owned()]).unwrap_or_default();
        let to_str = coordinator.unwrap_or("").to_owned();

        for slot in slots {
            if slot.is_idle && slot.idle_secs >= idle_alert_sec {
                let should_send = match self.alerts.get(&slot.key) {
                    None => true,
                    Some(row) if row.state == "closed" => true,
                    Some(row) => now_ms.saturating_sub(row.last_at * 1000) >= IDLE_REPEAT_MS,
                };
                if should_send {
                    let (opened_at, count) = match self.alerts.get(&slot.key) {
                        Some(row) if row.state != "closed" => (row.opened_at, row.count + 1),
                        _ => (now_sec, 1),
                    };
                    let text = match &slot.reason {
                        Some(r) => format!(
                            "slot {} ({}) idle for {} s ({r})",
                            slot.key, slot.class, slot.idle_secs
                        ),
                        None => format!(
                            "slot {} ({}) idle for {} s",
                            slot.key, slot.class, slot.idle_secs
                        ),
                    };
                    let row = AlertRow {
                        code: "IDLE".into(),
                        key: slot.key.clone(),
                        severity: "warn".into(),
                        to: to_str.clone(),
                        state: "open".into(),
                        opened_at,
                        last_at: now_sec,
                        count,
                        text: text.clone(),
                    };
                    self.alerts.insert(slot.key.clone(), row);
                    let body = json!({
                        "v": 1,
                        "code": "IDLE",
                        "key": slot.key,
                        "severity": "warn",
                        "state": "open",
                        "opened_at": opened_at,
                        "last_at": now_sec,
                        "count": count,
                        "text": text,
                        "data": {
                            "owner": slot.owner,
                            "class": slot.class,
                            "idle_secs": slot.idle_secs,
                            "reason": slot.reason,
                        }
                    })
                    .to_string();
                    out.push(Message {
                        id: 0,
                        ts: String::new(),
                        from: "hub".into(),
                        role: Role::Hub,
                        kind: Kind::Alert,
                        topic: format!("alert/IDLE/{}", slot.key),
                        body,
                        refs: vec!["state:open".into()],
                        to: to_vec.clone(),
                    });
                }
            } else if !slot.is_idle
                && let Some(row) = self.alerts.get_mut(&slot.key)
                && row.state != "closed"
            {
                row.state = "closed".into();
                row.last_at = now_sec;
                let text = format!("slot {} ({}) is now working", slot.key, slot.class);
                row.text = text.clone();
                let body = json!({
                    "v": 1,
                    "code": "IDLE",
                    "key": slot.key,
                    "severity": "info",
                    "state": "closed",
                    "opened_at": row.opened_at,
                    "last_at": now_sec,
                    "count": row.count,
                    "text": text,
                    "data": {
                        "owner": slot.owner,
                        "class": slot.class,
                    }
                })
                .to_string();
                out.push(Message {
                    id: 0,
                    ts: String::new(),
                    from: "hub".into(),
                    role: Role::Hub,
                    kind: Kind::Alert,
                    topic: format!("alert/IDLE/{}", slot.key),
                    body,
                    refs: vec!["state:closed".into()],
                    to: to_vec.clone(),
                });
            }
        }
        out
    }
}
