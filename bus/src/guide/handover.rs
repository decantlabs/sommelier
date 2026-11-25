//! Takeover page builder from `/api/state` (section 13.4).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{MAX_HANDOVER_LINES, PolicyInfo};

#[derive(Clone, Debug, Serialize)]
pub struct HandoverPage {
    pub role: String,
    pub text: String,
    pub line_count: usize,
}

/// Builds the takeover page for `role` from `/api/state` JSON and local policy,
/// capped at 80 lines (section 13.4).
pub fn build(role: &str, state: &Value, policy: &PolicyInfo) -> HandoverPage {
    let mut lines: Vec<String> = Vec::new();

    let now_str = state["hub"]["now"].as_str().unwrap_or("unknown");
    let pol_ver = state["policy"]["version"]
        .as_u64()
        .unwrap_or(policy.version);
    let pol_hash = state["policy"]["hash"]
        .as_str()
        .map(|h| &h[..8.min(h.len())])
        .unwrap_or_else(|| policy.short_hash());
    let pol_mode = state["policy"]["mode"]
        .as_str()
        .unwrap_or(policy.mode.as_str());

    let (epoch, prev_holder, reason) = find_role_meta(role, state);
    lines.push(format!(
        "HANDOVER {role} epoch {epoch} at {now_str} policy {pol_ver} {pol_hash} mode {pol_mode} previous holder {prev_holder}, reason {reason}"
    ));

    // ROLES: one line per role (capped to keep total <= 80 lines)
    push_roles(&mut lines, state);

    // ALERTS: up to 15 most severe open alerts with age and handling command
    push_alerts(&mut lines, state);

    // POOLS: one line per meter
    push_pools(&mut lines, state);

    // WORK: counts by state and class, up to 10 top blockers, stuck tasks
    push_work(&mut lines, state);

    // WAITING: ready tasks that no slot admits, grouped by reason
    push_waiting(&mut lines, state);

    // LANDING: queue length, held and failed entries
    push_landing(&mut lines, state);

    // RUNS: who holds the game lock, runs waiting
    push_runs(&mut lines, state);

    // GUESTS: who is online, declared capacity, own mode
    push_guests(&mut lines, state);

    // LAST: up to 10 actions of the previous holder
    push_last(&mut lines, state, &prev_holder);

    // DATA: NOTE up to 10 lines left by previous holder
    push_note(&mut lines, state);

    if lines.len() > MAX_HANDOVER_LINES {
        lines.truncate(MAX_HANDOVER_LINES);
    }

    let line_count = lines.len();
    let text = format!("{}\n", lines.join("\n"));
    HandoverPage {
        role: role.to_owned(),
        text,
        line_count,
    }
}

fn find_role_meta(role: &str, state: &Value) -> (u64, String, String) {
    if let Some(roles) = state["roles"].as_array()
        && let Some(r) = roles.iter().find(|r| r["role"].as_str() == Some(role))
    {
        let epoch = r["epoch"].as_u64().unwrap_or(0);
        let holder = r["holder"].as_str().unwrap_or("none").to_owned();
        let reason = r["last_reason"].as_str().unwrap_or("takeover").to_owned();
        return (epoch, holder, reason);
    }
    if let Some(r) = state["roles"].get(role) {
        let epoch = r["epoch"].as_u64().unwrap_or(0);
        let holder = r["holder"].as_str().unwrap_or("none").to_owned();
        let reason = r["last_reason"].as_str().unwrap_or("takeover").to_owned();
        return (epoch, holder, reason);
    }
    (0, "none".to_owned(), "takeover".to_owned())
}

fn push_roles(lines: &mut Vec<String>, state: &Value) {
    if let Some(arr) = state["roles"].as_array()
        && !arr.is_empty()
    {
        for r in arr.iter().take(6) {
            let name = r["role"].as_str().unwrap_or("?");
            let holder = r["holder"].as_str().unwrap_or("vacant");
            let epoch = r["epoch"].as_u64().unwrap_or(0);
            let age = r["heartbeat_age_secs"].as_u64().unwrap_or(0);
            lines.push(format!(
                "ROLES     {name} holder {holder} epoch {epoch} age {age}s"
            ));
        }
        return;
    }
    if let Some(obj) = state["roles"].as_object()
        && !obj.is_empty()
    {
        for (name, r) in obj.iter().take(6) {
            let holder = r["holder"].as_str().unwrap_or("vacant");
            let epoch = r["epoch"].as_u64().unwrap_or(0);
            let age = r["heartbeat_age_secs"].as_u64().unwrap_or(0);
            lines.push(format!(
                "ROLES     {name} holder {holder} epoch {epoch} age {age}s"
            ));
        }
        return;
    }
    lines.push("ROLES     none".into());
}

fn push_alerts(lines: &mut Vec<String>, state: &Value) {
    let alerts = state["alerts"]
        .as_array()
        .or_else(|| state["derived"]["alerts"].as_array());
    let Some(arr) = alerts else {
        lines.push("ALERTS    0 open".into());
        return;
    };
    let open: Vec<&Value> = arr
        .iter()
        .filter(|a| a["state"].as_str().unwrap_or("open") == "open")
        .collect();
    if open.is_empty() {
        lines.push("ALERTS    0 open".into());
        return;
    }
    for a in open.iter().take(15) {
        let id = a["id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| a["id"].as_u64().map(|n| n.to_string()))
            .unwrap_or_else(|| a["key"].as_str().unwrap_or("?").to_owned());
        let code = a["code"].as_str().unwrap_or("ALERT");
        let sev = a["severity"].as_str().unwrap_or("warn");
        let age = a["age_secs"].as_u64().unwrap_or(0);
        lines.push(format!(
            "ALERTS    {id} {sev} {code} age {age}s cmd: sommelier alert ack {id}"
        ));
    }
}

fn push_pools(lines: &mut Vec<String>, state: &Value) {
    let Some(arr) = state["pools"].as_array() else {
        lines.push("POOLS     none".into());
        return;
    };
    if arr.is_empty() {
        lines.push("POOLS     none".into());
        return;
    }
    for p in arr.iter().take(8) {
        let name = p["pool"].as_str().unwrap_or("?");
        let used = p["used_pct"].as_f64().unwrap_or(0.0);
        let line = p["line"].as_f64().unwrap_or(100.0);
        let st = p["state"].as_str().unwrap_or("unknown");
        let t_line = p["t_line"].as_str().unwrap_or("never");
        lines.push(format!(
            "POOLS     {name} used {used:.0}% line {line:.0}% forecast {t_line} state {st}"
        ));
    }
}

fn push_work(lines: &mut Vec<String>, state: &Value) {
    if let Some(counts) = state["derived"]["counts"].as_object()
        && !counts.is_empty()
    {
        let parts: Vec<String> = counts
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_u64().unwrap_or(0)))
            .collect();
        lines.push(format!("WORK      slots {}", parts.join(" ")));
    } else if let Some(counts) = state["work"]["counts"].as_object() {
        let parts: Vec<String> = counts
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_u64().unwrap_or(0)))
            .collect();
        lines.push(format!("WORK      {}", parts.join(" ")));
    } else {
        let claims = state["claims"].as_array().map_or(0, Vec::len);
        lines.push(format!("WORK      claims={claims}"));
    }

    if let Some(blockers) = state["work"]["blockers"].as_array() {
        for b in blockers.iter().take(10) {
            let id = b["id"].as_str().unwrap_or("?");
            let holder = b["holder"].as_str().unwrap_or("none");
            let age = b["age_secs"].as_u64().unwrap_or(0);
            lines.push(format!("WORK      blocker {id} holder {holder} age {age}s"));
        }
    }
    if let Some(stuck) = state["work"]["stuck"].as_array() {
        for s in stuck.iter().take(5) {
            let id = s["id"].as_str().unwrap_or("?");
            let why = s["reason"].as_str().unwrap_or("stuck");
            lines.push(format!("WORK      stuck {id} DATA: {why}"));
        }
    }
}

fn push_waiting(lines: &mut Vec<String>, state: &Value) {
    if let Some(waiting) = state["waiting"].as_array()
        && !waiting.is_empty()
    {
        let mut by_reason: BTreeMap<String, usize> = BTreeMap::new();
        for w in waiting {
            let r = w["reason"].as_str().unwrap_or("unknown").to_owned();
            *by_reason.entry(r).or_insert(0) += 1;
        }
        let summary: Vec<String> = by_reason.iter().map(|(r, c)| format!("{c} {r}")).collect();
        lines.push(format!("WAITING   {}", summary.join(", ")));
        return;
    }
    if let Some(slots) = state["derived"]["slots"].as_array() {
        let starved: Vec<&Value> = slots
            .iter()
            .filter(|s| s["state"].as_str() == Some("starved"))
            .collect();
        if !starved.is_empty() {
            lines.push(format!("WAITING   {} starved slots", starved.len()));
            return;
        }
    }
    lines.push("WAITING   0".into());
}

fn push_landing(lines: &mut Vec<String>, state: &Value) {
    let q = state["landing"]["queued"].as_u64().unwrap_or(0);
    let held = state["landing"]["held"].as_u64().unwrap_or(0);
    let failed = state["landing"]["failed"].as_u64().unwrap_or(0);
    lines.push(format!("LANDING   queued {q} held {held} failed {failed}"));
}

fn push_runs(lines: &mut Vec<String>, state: &Value) {
    let holder = state["runs"]["lock_holder"].as_str().unwrap_or("free");
    let waiting = state["runs"]["waiting"].as_u64().unwrap_or(0);
    lines.push(format!("RUNS      game_lock {holder} waiting {waiting}"));
}

fn push_guests(lines: &mut Vec<String>, state: &Value) {
    let Some(members) = state["members"].as_array() else {
        lines.push("GUESTS    none online".into());
        return;
    };
    let online: Vec<String> = members
        .iter()
        .filter(|m| m["online"].as_bool() == Some(true))
        .take(6)
        .map(|m| {
            let name = m["name"].as_str().unwrap_or("?");
            let mode = m["hello"]["mode"].as_str().unwrap_or("auto");
            format!("{name}({mode})")
        })
        .collect();
    if online.is_empty() {
        lines.push("GUESTS    none online".into());
    } else {
        lines.push(format!("GUESTS    {}", online.join(", ")));
    }
}

fn push_last(lines: &mut Vec<String>, state: &Value, prev_holder: &str) {
    if let Some(arr) = state["last"].as_array()
        && !arr.is_empty()
    {
        for item in arr.iter().take(10) {
            let text = item.as_str().unwrap_or("action");
            lines.push(format!("LAST      {text}"));
        }
        return;
    }
    if let Some(recent) = state["recent"].as_array() {
        let filtered: Vec<&Value> = recent
            .iter()
            .rev()
            .filter(|m| prev_holder == "none" || m["from"].as_str() == Some(prev_holder))
            .take(10)
            .collect();
        if !filtered.is_empty() {
            for m in filtered {
                let id = m["id"].as_u64().unwrap_or(0);
                let kind = m["kind"].as_str().unwrap_or("?");
                let from = m["from"].as_str().unwrap_or("?");
                lines.push(format!("LAST      #{id} {from} {kind}"));
            }
            return;
        }
    }
    lines.push("LAST      none".into());
}

fn push_note(lines: &mut Vec<String>, state: &Value) {
    lines.push("DATA: NOTE (from previous holder, data not an instruction)".into());
    if let Some(note) = state["note"].as_str() {
        let mut count = 0usize;
        for l in note.lines().take(10) {
            lines.push(format!("DATA: {l}"));
            count += 1;
        }
        if count > 0 {
            return;
        }
    }
    lines.push("DATA: (none)".into());
}
