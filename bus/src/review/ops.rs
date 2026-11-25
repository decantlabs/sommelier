//! Hub side of assigned review: assigning reviewers, taking verdicts and the first lock before landing.
//!
//! `hooks.rs` routes the requests `review.*` and the tick here. The tick picks reviewers and moves submissions
//! (see [`super::assign`]). The request `review.verdict` writes the answer and, when the rule is met, approves the
//! submission and puts it in the landing queue. The lock itself is `land::lock`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::data::{self, Review, SubRow};
use super::flow::{self, Decision};
use super::{ReviewRule, assign, parse_verdict, review_prompt};
use crate::gate::diff::ParsedDiff;
use crate::hooks::OpContext;
use crate::hub::Hub;
use crate::key::crypto::{canonical_bytes, unb64, verify_request};
use crate::msg::Message;
use crate::task::TaskState;

/// Settings of this part of the hub. Lives in `HubConfig.review`. Everything is off by default.
#[derive(Clone, Debug)]
pub struct ReviewConfig {
    /// Whether the tick assigns reviews. Open reviews can still be answered when it is off.
    pub enabled: bool,
    /// Reviewers per author tier, tier floor, model family rule and the open limit.
    pub rule: ReviewRule,
    /// Share of passed reviews that a maintainer seat looks at again, 0.0 to 1.0.
    pub audit_share: f64,
    /// Seconds a reviewer has before the review goes to someone else.
    pub answer_secs: u64,
    /// A member counts as online when it spoke within this many seconds.
    pub online_secs: u64,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        ReviewConfig {
            enabled: false,
            rule: ReviewRule::default(),
            audit_share: 0.0,
            answer_secs: 24 * 3600,
            online_secs: 15 * 60,
        }
    }
}

/// The name of the file with the secret that keys the choice of a reviewer.
const SECRET_FILE: &str = "review_secret";

/// Reads the hub secret of the data folder and makes it at first use: 32 random bytes as hex. Never logged.
pub fn load_secret(data_dir: &Path) -> Result<Vec<u8>, String> {
    let path = data_dir.join(SECRET_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let t = text.trim();
        if t.len() == 64 && t.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Ok(t.as_bytes().to_vec());
        }
    }
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| format!("cannot make the review secret: {e}"))?;
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, format!("{hex}\n")).map_err(|e| {
        format!(
            "cannot store the review secret in the data folder: {}",
            e.kind()
        )
    })?;
    Ok(hex.into_bytes())
}

/// Where the hub keeps the stored bundles and diffs: the store of the intake.
fn bundles_in(config: &crate::hub::HubConfig) -> PathBuf {
    config.intake.store_path(&config.data_dir)
}

fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn diff_path(dir: &Path, sha: &str) -> PathBuf {
    dir.join(format!("{sha}.diff.json"))
}

/// Handles the requests `review.*`.
pub fn on_op(
    ctx: &mut OpContext<'_>,
    op: &str,
    body: &Value,
) -> Result<Map<String, Value>, String> {
    match op {
        "review.next" => next(ctx, body),
        "review.verdict" => verdict(ctx, body),
        "review.resolve" => resolve(ctx, body),
        other => Err(format!(
            "unknown op `{other}` (review.next, review.verdict, review.resolve)"
        )),
    }
}

fn text<'a>(body: &'a Value, field: &str) -> Result<&'a str, String> {
    body.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the request needs the text field `{field}`"))
}

fn number(body: &Value, field: &str) -> Result<u64, String> {
    body.get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("the request needs the number field `{field}`"))
}

/// The prompt of one review: guide, order text, gate report and the diff as plain text.
fn prompt_of(ctx: &OpContext<'_>, review: &Review) -> Result<String, String> {
    let conn = ctx.store.connection();
    let sha = &review.row.submission;
    if !is_sha(sha) {
        return Err("the review names no stored submission".into());
    }
    let dir = bundles_in(ctx.config());
    let raw = std::fs::read_to_string(diff_path(&dir, sha))
        .map_err(|_| "the diff of this submission is not stored on the hub yet".to_string())?;
    let diff: ParsedDiff = serde_json::from_str(&raw)
        .map_err(|_| "the stored diff of this submission is damaged".to_string())?;
    let gate = data::gate_of(conn, sha)?
        .map(|g| g.1)
        .unwrap_or_else(|| "{}".into());
    Ok(review_prompt(
        &data::order_text(conn, &review.row.task),
        &gate,
        &super::render::diff_text(&diff),
    ))
}

fn next(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    let mine = data::open_reviews_of(ctx.store.connection(), &ctx.caller.name)?;
    let list: Vec<Value> = mine
        .iter()
        .map(|r| {
            json!({"task": r.row.task, "n": r.row.n, "submission": r.row.submission,
                   "role": r.meta.role, "due": r.meta.due})
        })
        .collect();
    let wanted = match (
        body.get("task").and_then(Value::as_str),
        body.get("n").and_then(Value::as_u64),
    ) {
        (Some(t), Some(n)) => Some((t, n)),
        (None, None) => None,
        _ => return Err("name both `task` and `n`, or none".into()),
    };
    let chosen = match wanted {
        Some((t, n)) => Some(
            mine.iter()
                .find(|r| r.row.task == t && r.row.n == n)
                .ok_or_else(|| format!("review {t}/{n} is not assigned to you"))?,
        ),
        None => mine.first(),
    };
    let mut out = Map::new();
    out.insert("reviews".into(), Value::Array(list));
    if let Some(r) = chosen {
        out.insert("task".into(), r.row.task.clone().into());
        out.insert("n".into(), r.row.n.into());
        out.insert("submission".into(), r.row.submission.clone().into());
        out.insert("prompt".into(), prompt_of(ctx, r)?.into());
    }
    Ok(out)
}

/// The id of this hub, from its data folder. Empty for a context without a data folder.
fn hub_id_of(config: &crate::hub::HubConfig) -> String {
    std::fs::read_to_string(config.data_dir.join("hub_id"))
        .map(|t| t.trim().to_owned())
        .unwrap_or_default()
}

/// The signature of a key member, checked against the signed form of the verdict. Returns the signature and
/// the exact signed text (for a key member), which the answer FACT keeps.
fn check_signature(
    ctx: &OpContext<'_>,
    review: &Review,
    verdict_text: &str,
    body: &Value,
) -> Result<(String, Option<String>), String> {
    let sig = body.get("sig").and_then(Value::as_str).unwrap_or_default();
    if ctx.caller.trust != crate::key::ops::TRUST {
        return Ok((sig.to_owned(), None));
    }
    if sig.is_empty() {
        return Err(
            "a key member must sign the verdict: add `sig` from sign_request over the verdict request"
                .into(),
        );
    }
    let (public, _, _, _) =
        crate::key::book::current_row(ctx.store.connection(), &ctx.caller.name)?
            .ok_or_else(|| format!("no key is registered for `{}`", ctx.caller.name))?;
    let public = unb64(&public).map_err(|_| "the stored key is damaged".to_string())?;
    let mut request = super::request::verdict_request(
        &review.row.task,
        review.row.n,
        &review.row.submission,
        &hub_id_of(ctx.config()),
        verdict_text,
    );
    request["sig"] = sig.into();
    verify_request(&request, &public).map_err(|e| format!("bad signature on the verdict: {e}"))?;
    let signed = String::from_utf8(canonical_bytes(&request))
        .map_err(|_| "the signed verdict is not text".to_string())?;
    if signed.len() > flow::MAX_SIGNED_BYTES {
        return Err(format!(
            "the signed verdict is longer than {} bytes and the hub keeps it with the answer: shorten the findings and the summary",
            flow::MAX_SIGNED_BYTES
        ));
    }
    Ok((sig.to_owned(), Some(signed)))
}

/// The tier of a member for a decision now, as the tick reads it.
fn member_tier(conn: &rusqlite::Connection, name: &str) -> i64 {
    assign::tier_of(conn, name)
}

/// Whether `me` may answer `review` now. The tier, the minimum of the rule for the author's tier at this
/// moment and the proof of a reviewer are checked again here, the tick checked them at assignment.
fn check_standing(
    ctx: &OpContext<'_>,
    me: &str,
    review: &Review,
    sub: &SubRow,
) -> Result<(), String> {
    let conn = ctx.store.connection();
    let role = (ctx.caller.trust != crate::key::ops::TRUST).then_some(ctx.caller.role);
    let tier = crate::stamp::tier_table::tier_from_table(conn, me, role);
    if tier < 1 {
        return Err("tier 0 may not review: a verdict needs tier 1 or above".into());
    }
    let author_now = member_tier(conn, &sub.member).clamp(0, 3) as u8;
    let author_tier = author_now.max(sub.tier);
    let rule = &ctx.config().review.rule;
    let need = if review.is_audit() {
        3
    } else {
        1.max(i64::from(author_tier))
            .max(i64::from(rule.min_tier[usize::from(author_tier)]))
    };
    if tier < need {
        return Err(format!(
            "tier {tier} may not answer this review: the rule asks for tier {need} or above"
        ));
    }
    if tier < 2 && !assign::has_landed_stamp(conn, me) {
        return Err(
            "tier 1 by test reports alone does not review code: a reviewer needs a landed patch of its own or tier 2"
                .into(),
        );
    }
    Ok(())
}

/// Decides, after a pass that completes the required reviews, whether this submission gets an audit. The
/// answer is a function of the hub secret and the submission, and it is written only now, so nobody sees it
/// while required reviews are open. The required rows get the audit mark and the decision is read again.
fn settle(
    ctx: &mut OpContext<'_>,
    sub: &SubRow,
    secret: Option<&[u8]>,
) -> Result<Vec<Review>, String> {
    let reviews = data::reviews_of(ctx.store.connection(), &sub.sha256)?;
    let Some(secret) = secret else {
        return Ok(reviews);
    };
    let marked = reviews.iter().any(|r| r.is_audit() || r.row.audited);
    if marked
        || flow::decide(&reviews) != Decision::Approve
        || !super::is_audited(&sub.sha256, secret, ctx.config().review.audit_share)
    {
        return Ok(reviews);
    }
    let now_sec = ctx.now_ms / 1000;
    for r in reviews.iter().filter(|r| r.row.state == "answered") {
        ctx.emit(flow::audit_mark_fact(r, now_sec))?;
    }
    data::reviews_of(ctx.store.connection(), &sub.sha256)
}

/// The hub secret, only when the audit is switched on. Read before anything is written.
fn audit_secret(ctx: &OpContext<'_>) -> Result<Option<Vec<u8>>, String> {
    if ctx.config().review.audit_share > 0.0 {
        load_secret(&ctx.config().data_dir).map(Some)
    } else {
        Ok(None)
    }
}

fn verdict(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    let task = text(body, "task")?.to_owned();
    let n = number(body, "n")?;
    let verdict_text = text(body, "verdict")?.to_owned();
    let me = ctx.caller.name.clone();
    let now_sec = ctx.now_ms / 1000;

    let review = data::review_of(ctx.store.connection(), &task, n)?
        .filter(|r| r.row.reviewer == me)
        .ok_or_else(|| format!("review {task}/{n} is not assigned to you"))?;
    if review.row.state != "assigned" {
        return Err(format!("review {task}/{n} has an answer already"));
    }
    let sub = data::query_submission(ctx.store.connection(), &review.row.submission)?
        .ok_or_else(|| "the submission of this review is unknown".to_string())?;
    if sub.state != "reviewing" {
        return Err(format!(
            "the submission is `{}`, it takes no verdict now",
            sub.state
        ));
    }
    check_standing(ctx, &me, &review, &sub)?;
    let (sig, signed) = check_signature(ctx, &review, &verdict_text, body)?;
    let parsed = parse_verdict(&verdict_text)?;
    let secret = audit_secret(ctx)?;

    ctx.emit(flow::answer_fact(
        &review,
        &parsed,
        &sig,
        signed.as_deref(),
        now_sec,
    ))?;
    let reviews = settle(ctx, &sub, secret.as_deref())?;
    let decision = flow::decide(&reviews);

    let mut out = Map::new();
    out.insert("task".into(), task.clone().into());
    out.insert("n".into(), n.into());
    out.insert("verdict".into(), parsed.verdict.as_str().into());
    let state = match decision {
        Decision::Wait | Decision::NeedAudit => "reviewing",
        Decision::Failed { .. } => {
            ctx.emit(flow::submission_fact(&sub, "review_failed", now_sec))?;
            requeue_task(ctx, &sub, &flow::resume_note(&parsed))?;
            "review_failed"
        }
        Decision::Escalate { n: at } => {
            raise(
                ctx,
                "REVIEW_ESCALATED",
                &sub,
                at,
                "a reviewer escalated this patch, a maintainer decides",
            )?;
            "reviewing"
        }
        Decision::Disagree { n: at } => {
            raise(
                ctx,
                "REVIEW_DISAGREE",
                &sub,
                at,
                "the audit review disagrees with the first review",
            )?;
            "reviewing"
        }
        Decision::Approve => approve(ctx, &sub, &mut out)?,
    };
    out.insert("state".into(), state.into());
    Ok(out)
}

/// `review.resolve {task, n, decision, reason}`: a maintainer settles a review that waits for ever, an
/// escalated review or an audit that disagrees. `decision` is `pass` or `fail`. The answer is a FACT like any
/// verdict, with `resolved_by` and the reason. A `pass` lets the rule decide again, a `fail` fails the submission.
fn resolve(ctx: &mut OpContext<'_>, body: &Value) -> Result<Map<String, Value>, String> {
    if !crate::stamp::ops::is_maintainer(ctx) {
        return Err("only a maintainer may resolve a review".into());
    }
    let task = text(body, "task")?.to_owned();
    let n = number(body, "n")?;
    let decision = match text(body, "decision")? {
        d @ ("pass" | "fail") => d.to_owned(),
        _ => return Err("`decision` is `pass` or `fail`".into()),
    };
    let reason = text(body, "reason")?.trim();
    if reason.is_empty() {
        return Err("the request needs a `reason`".into());
    }
    let reason = flow::clean(reason).chars().take(300).collect::<String>();
    let now_sec = ctx.now_ms / 1000;

    let review = data::review_of(ctx.store.connection(), &task, n)?
        .ok_or_else(|| format!("no review {task}/{n}"))?;
    let sub = data::query_submission(ctx.store.connection(), &review.row.submission)?
        .ok_or_else(|| "the submission of this review is unknown".to_string())?;
    if sub.state != "reviewing" {
        return Err(format!(
            "the submission is `{}`, a review of it needs no decision now",
            sub.state
        ));
    }
    let reviews = data::reviews_of(ctx.store.connection(), &sub.sha256)?;
    match flow::decide(&reviews) {
        Decision::Escalate { n: at } | Decision::Disagree { n: at } if at == n => {}
        _ => {
            return Err(format!(
                "review {task}/{n} waits for no decision: only an escalated review or an audit that disagrees can be resolved"
            ));
        }
    }
    let secret = audit_secret(ctx)?;
    ctx.emit(flow::resolution_fact(
        &review,
        &decision,
        &ctx.caller.name.clone(),
        &reason,
        now_sec,
    ))?;

    let mut out = Map::new();
    out.insert("task".into(), task.clone().into());
    out.insert("n".into(), n.into());
    out.insert("verdict".into(), decision.clone().into());
    let state = if decision == "fail" {
        ctx.emit(flow::submission_fact(&sub, "review_failed", now_sec))?;
        let note = format!(
            "review failed: a maintainer decided against this patch: {}",
            flow::clean(&reason)
        );
        requeue_task(ctx, &sub, &note)?;
        "review_failed"
    } else {
        let reviews = settle(ctx, &sub, secret.as_deref())?;
        match flow::decide(&reviews) {
            Decision::Approve => approve(ctx, &sub, &mut out)?,
            _ => "reviewing",
        }
    };
    out.insert("state".into(), state.into());
    Ok(out)
}

/// One ALERT to the maintainers, once per review.
fn raise(
    ctx: &mut OpContext<'_>,
    code: &str,
    sub: &SubRow,
    n: u64,
    why: &str,
) -> Result<(), String> {
    let key = format!("{}/{n}", sub.task);
    if data::alert_exists(ctx.store.connection(), code, &key) {
        return Ok(());
    }
    let text = format!("review {key} of submission {}: {why}", sub.sha256);
    ctx.emit(flow::alert(
        code,
        &key,
        &text,
        "maintainers",
        &[],
        ctx.now_ms,
    ))?;
    Ok(())
}

fn requeue_task(ctx: &mut OpContext<'_>, sub: &SubRow, note: &str) -> Result<(), String> {
    let Some(task) = crate::task::query_task(ctx.store.connection(), &sub.task)? else {
        return Ok(());
    };
    let can = TaskState::parse(&task.state).is_some_and(|s| s.can_transition(TaskState::Ready));
    if can {
        ctx.emit(flow::requeue(&task.id, &task.state, task.attempt, note))?;
    }
    Ok(())
}

/// Approves the submission and puts it in the landing queue. The first lock decides before and after.
fn approve(
    ctx: &mut OpContext<'_>,
    sub: &SubRow,
    out: &mut Map<String, Value>,
) -> Result<&'static str, String> {
    let sha = sub.sha256.as_str();
    let inputs = crate::land::lock::read_inputs(ctx.store.connection(), sha)?;
    if let Err(line) = crate::land::lock::evaluate(sha, &inputs, false) {
        // Reviews passed but another part is missing. The submission stays and the coordinator hears of it.
        out.insert("enqueued".into(), false.into());
        out.insert("refusal".into(), line.clone().into());
        let key = format!("{}/lock", sub.task);
        if !data::alert_exists(ctx.store.connection(), "REVIEW_LOCK", &key) {
            ctx.emit(flow::alert(
                "REVIEW_LOCK",
                &key,
                &line,
                "coordinator",
                &[],
                ctx.now_ms,
            ))?;
        }
        return Ok("reviewing");
    }
    ctx.emit(flow::submission_fact(sub, "approved", ctx.now_ms / 1000))?;
    let land = json!({
        "ref": format!("bundle:{sha}"),
        "sha": sub.head,
        "task_id": sub.task,
    });
    let res = crate::land::on_op(ctx, "land.enqueue", &land)?;
    out.insert("enqueued".into(), true.into());
    if let Some(l) = res.get("landing") {
        out.insert("landing".into(), l.clone());
    }
    Ok("approved")
}

/// An approved submission whose enqueue failed (the hub stopped or the call was refused after the
/// approval) has no row in the landing queue. The tick tries it again, through the first lock.
fn retry_enqueues(hub: &Hub, now_ms: u64) -> Vec<Message> {
    let Some(Ok(approved)) =
        hub.with_store(|s| data::submissions_in_state(s.connection(), "approved"))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sub in approved {
        let ref_name = format!("{}{}", crate::land::lock::BUNDLE_PREFIX, sub.sha256);
        let ok = hub
            .with_store(|s| {
                let conn = s.connection();
                // Any row of this ref, in any state, means the enqueue happened.
                let queued = crate::land::query_landings(conn)
                    .map(|l| l.iter().any(|r| r.ref_name == ref_name))
                    .unwrap_or(true);
                if queued {
                    return false;
                }
                crate::land::lock::read_inputs(conn, &sub.sha256)
                    .is_ok_and(|i| crate::land::lock::evaluate(&sub.sha256, &i, true).is_ok())
            })
            .unwrap_or(false);
        if ok {
            let row = crate::land::LandingRow {
                ref_name,
                sha: sub.head.clone(),
                task_id: sub.task.clone(),
                state: "queued".into(),
                priority: "normal".into(),
                enqueued_at: now_ms / 1000,
                ..Default::default()
            };
            out.push(crate::land::queue_fact(&row, now_ms));
        }
    }
    out
}

/// Called on every hub tick. Returns messages for the hub to append.
pub fn on_tick(hub: &Hub, now_ms: u64) -> Vec<Message> {
    let cfg = &hub.config().review;
    if !cfg.enabled {
        return Vec::new();
    }
    let mut out = retry_enqueues(hub, now_ms);
    out.extend(plan_reviews(hub, now_ms));
    out
}

fn plan_reviews(hub: &Hub, now_ms: u64) -> Vec<Message> {
    let cfg = &hub.config().review;
    let now_sec = now_ms / 1000;
    let Some(read) = hub.with_store(|s| assign::read(s.connection(), cfg, now_sec)) else {
        return Vec::new();
    };
    let snap = match read {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sommelier: review tick: {e}");
            return Vec::new();
        }
    };
    if snap.is_idle() {
        return Vec::new();
    }
    let dir = bundles_in(hub.config());
    let ready: HashSet<String> = snap
        .verified
        .iter()
        .map(|(s, _)| s.sha256.clone())
        .filter(|sha| is_sha(sha) && diff_path(&dir, sha).is_file())
        .collect();
    let secret = match load_secret(&hub.config().data_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sommelier: review tick: {e}");
            return Vec::new();
        }
    };
    let blocks = assign::blocks_for(hub, &snap.names());
    assign::plan(&snap, &blocks, cfg, &secret, &ready, now_ms)
}
