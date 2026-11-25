//! The requests `key.rotate` and `key.revoke`.
//!
//! Each writes one FACT `key/<member>` from the hub. The body carries the signature of the
//! member, so anyone can check the statement later without trusting the hub.

use serde_json::{Map, Value, json};

use super::book::{
    KeyState, Proof, current_row, public_known, revoke_body, revoke_request, rotate_body,
    rotate_request,
};
use super::crypto::{canonical_bytes, is_member_id, unb64, verify_bytes};
use crate::hooks::OpContext;
use crate::msg::{Kind, Message, Role};
use crate::time::rfc3339;

/// Name of the trust level that a key member has.
pub const TRUST: &str = "tier0";

/// Longest reason of a revocation, in characters.
const MAX_REASON: usize = 200;

fn fact(member: &str, body: String) -> Message {
    Message {
        id: 0,
        ts: String::new(),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Fact,
        topic: format!("key/{member}"),
        body,
        refs: vec![],
        to: vec![],
    }
}

fn text<'a>(body: &'a Value, field: &str) -> Result<&'a str, String> {
    body.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the request needs the text field `{field}`"))
}

/// The id of this hub that `handle_text` put into the body of the request.
fn hub_of(body: &Value) -> Result<&str, String> {
    body.get("hub")
        .and_then(Value::as_str)
        .filter(|h| super::book::is_hub_id(h))
        .ok_or_else(|| "the request names no hub: sign it for the hub you send it to".to_string())
}

fn check_sig(body: &Value, request: &Value, public: &str) -> Result<(), String> {
    let public = unb64(public).map_err(|_| "the stored key is damaged".to_string())?;
    let sig = unb64(text(body, "sig")?).map_err(|e| format!("`sig` is {e}"))?;
    verify_bytes(&public, &canonical_bytes(request), &sig)
        .map_err(|e| format!("bad signature on the request: {e}"))
}

/// Routes the `key.*` ops.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    match op {
        "key.rotate" => rotate(ctx, body),
        "key.revoke" => revoke(ctx, body),
        other => Err(format!("unknown op `{other}` (key.rotate, key.revoke)")),
    }
}

fn rotate(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    if ctx.caller.trust != TRUST {
        return Err(
            "key.rotate is for key members: sign in with your key, a token has none".into(),
        );
    }
    let member = ctx.caller.name.clone();
    let (current, handle, joined_at, state) = current_row(ctx.store.connection(), &member)?
        .ok_or_else(|| format!("no key is registered for `{member}`"))?;
    if state != KeyState::Active {
        return Err("this key is not active".into());
    }
    let new_pub = text(body, "new_pub")?;
    if unb64(new_pub).map(|b| b.len()) != Ok(32) {
        return Err("`new_pub` must be an Ed25519 public key of 32 bytes in base64url".into());
    }
    if public_known(ctx.store.connection(), new_pub)? {
        return Err("`new_pub` is a key that the hub already knows".into());
    }
    let hub_id = hub_of(body)?;
    check_sig(
        body,
        &rotate_request(hub_id, &member, &current, new_pub),
        &current,
    )?;
    let sig = text(body, "sig")?.to_owned();
    let id = ctx.emit(fact(
        &member,
        rotate_body(
            hub_id, &member, &current, new_pub, &handle, &joined_at, &sig,
        ),
    ))?;
    let mut out = Map::new();
    out.insert("member".into(), member.into());
    out.insert("pub".into(), new_pub.into());
    out.insert("id".into(), id.into());
    Ok(out)
}

fn revoke(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    let own = ctx.caller.trust == TRUST;
    let owner = ctx.caller.role == Role::Orchestrator;
    if !own && !owner {
        return Err("key.revoke needs your own key or an orchestrator token".into());
    }
    let member = if own {
        if let Some(m) = body.get("member").and_then(Value::as_str)
            && m != ctx.caller.name
        {
            return Err("a key member may revoke its own key only".into());
        }
        ctx.caller.name.clone()
    } else {
        let m = text(body, "member")?.to_owned();
        if !is_member_id(&m) {
            return Err(format!("`{m}` is not a member id"));
        }
        m
    };
    let reason = body
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if reason.chars().count() > MAX_REASON || reason.chars().any(char::is_control) {
        return Err(format!(
            "`reason` is one line of at most {MAX_REASON} characters"
        ));
    }
    if !own && reason.trim().is_empty() {
        return Err("an orchestrator must give a `reason`".into());
    }
    let (current, handle, joined_at, state) = current_row(ctx.store.connection(), &member)?
        .ok_or_else(|| format!("no key is registered for `{member}`"))?;
    if state != KeyState::Active {
        return Err(format!("the key of `{member}` is not active"));
    }
    let name = ctx.caller.name.clone();
    let sig_text;
    let proof = if own {
        check_sig(
            body,
            &revoke_request(hub_of(body)?, &member, &current, &reason),
            &current,
        )?;
        sig_text = text(body, "sig")?.to_owned();
        Proof::Signature(&sig_text)
    } else {
        Proof::Owner(&name)
    };
    let id = ctx.emit(fact(
        &member,
        revoke_body(
            body.get("hub").and_then(Value::as_str).unwrap_or_default(),
            &member,
            &current,
            &handle,
            &joined_at,
            &reason,
            proof,
        ),
    ))?;
    let mut out = Map::new();
    out.insert("member".into(), member.into());
    out.insert("revoked_at".into(), json!(rfc3339(ctx.now_ms)));
    out.insert("id".into(), id.into());
    Ok(out)
}
