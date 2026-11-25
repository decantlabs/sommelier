//! Task brief storage addressed by SHA 256 (section 5.2).
//!
//! HTTP endpoints:
//! - `PUT /api/brief/<id>` (orchestrator token auth, max 64 KiB)
//! - `GET /api/brief/<id>` (member or orchestrator token auth)
//!
//! A brief is stored in `<data_dir>/briefs/` under its task id and its SHA 256 hex digest,
//! so it can be retrieved either by task id or by SHA 256 hash.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::msg::Role;
use crate::tokens::Member;

/// Maximum brief body size in bytes (64 KiB, section 5.2).
pub const MAX_BRIEF_BYTES: usize = 64 * 1024;

/// Metadata returned when storing or inspecting a brief.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefRecord {
    pub id: String,
    pub sha256: String,
    pub bytes: usize,
}

/// Computes the lower-case hex SHA 256 digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Validates a brief identifier (`<task>` or `<sha256>`): 1 to 128 ASCII letters, digits, `_`, `-`, or `.`.
pub fn validate_brief_id(id: &str) -> Result<&str, String> {
    let id = id.trim();
    if id.is_empty() || id.len() > 128 {
        return Err("brief id must have 1 to 128 characters".into());
    }
    if id.starts_with('.')
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
    {
        return Err(
            "brief id must contain only ASCII letters, digits, dash, underscore or dot".into(),
        );
    }
    Ok(id)
}

fn briefs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("briefs")
}

/// Stores a brief under `<data_dir>/briefs/<id>.md` and `<data_dir>/briefs/sha256/<sha256>.md`.
/// If `expected_sha256` is given, verifies that the content's SHA 256 matches it (refusing on mismatch).
pub fn put_brief(
    data_dir: &Path,
    id: &str,
    content: &[u8],
    expected_sha256: Option<&str>,
) -> Result<BriefRecord, String> {
    let id = validate_brief_id(id)?;
    if content.len() > MAX_BRIEF_BYTES {
        return Err(format!(
            "brief is {} bytes, the limit is {} bytes (64 KiB)",
            content.len(),
            MAX_BRIEF_BYTES
        ));
    }
    let text = std::str::from_utf8(content).map_err(|_| "brief must be valid UTF-8 text")?;
    let hash = sha256_hex(text.as_bytes());
    if let Some(exp) = expected_sha256.map(str::trim).filter(|s| !s.is_empty())
        && !exp.eq_ignore_ascii_case(&hash)
    {
        return Err(format!("brief sha256 mismatch: expected {exp}, got {hash}"));
    }

    let dir = briefs_dir(data_dir);
    let sha_dir = dir.join("sha256");
    fs::create_dir_all(&sha_dir).map_err(|e| format!("cannot create briefs dir: {e}"))?;

    write_atomic(&dir.join(format!("{id}.md")), text.as_bytes())
        .map_err(|e| format!("cannot store brief `{id}`: {e}"))?;
    write_atomic(&sha_dir.join(format!("{hash}.md")), text.as_bytes())
        .map_err(|e| format!("cannot store brief hash `{hash}`: {e}"))?;

    Ok(BriefRecord {
        id: id.to_owned(),
        sha256: hash,
        bytes: content.len(),
    })
}

/// Reads a brief by task id or by SHA 256 digest from `<data_dir>/briefs/`.
pub fn get_brief(data_dir: &Path, id_or_sha: &str) -> Result<Option<(String, String)>, String> {
    let key = validate_brief_id(id_or_sha)?;
    let dir = briefs_dir(data_dir);
    let by_id = dir.join(format!("{key}.md"));
    let by_sha = dir
        .join("sha256")
        .join(format!("{}.md", key.to_ascii_lowercase()));

    let target = if by_id.is_file() {
        by_id
    } else if by_sha.is_file() {
        by_sha
    } else {
        return Ok(None);
    };

    let text =
        fs::read_to_string(&target).map_err(|e| format!("cannot read brief `{key}`: {e}"))?;
    let hash = sha256_hex(text.as_bytes());
    Ok(Some((text, hash)))
}

/// Reads a brief by id or hash and verifies its SHA 256 matches `expected_sha256` when provided.
pub fn verify_brief_for_task(
    data_dir: &Path,
    id_or_sha: &str,
    expected_sha256: Option<&str>,
) -> Result<(String, String), String> {
    let (text, actual) =
        get_brief(data_dir, id_or_sha)?.ok_or_else(|| format!("brief `{id_or_sha}` not found"))?;
    if let Some(exp) = expected_sha256.map(str::trim).filter(|s| !s.is_empty())
        && !exp.eq_ignore_ascii_case(&actual)
    {
        return Err(format!(
            "brief sha256 mismatch for `{id_or_sha}`: expected {exp}, got {actual}"
        ));
    }
    Ok((text, actual))
}

fn brief_http_url(base_url: &str, id: &str, query: Option<&str>) -> Result<reqwest::Url, String> {
    let id = validate_brief_id(id)?;
    let mut url = reqwest::Url::parse(base_url.trim()).map_err(|_| "invalid hub URL".to_owned())?;
    let scheme = match url.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return Err("invalid hub URL scheme".into()),
    };
    url.set_scheme(scheme)
        .map_err(|_| "invalid hub URL scheme".to_owned())?;
    url.set_path(&format!("/api/brief/{id}"));
    url.set_query(query);
    url.set_fragment(None);
    Ok(url)
}

/// Uploads a brief via `PUT /api/brief/<id>` with optional `?sha256=<hash>` verification.
pub async fn put_brief_http(
    opts: &crate::client::Opts,
    id: &str,
    content: &[u8],
    expected_sha256: Option<&str>,
) -> Result<BriefRecord, String> {
    if content.len() > MAX_BRIEF_BYTES {
        return Err(format!(
            "brief is {} bytes, the limit is {} bytes (64 KiB)",
            content.len(),
            MAX_BRIEF_BYTES
        ));
    }
    let query = expected_sha256
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("sha256={s}"));
    let url = brief_http_url(&opts.url, id, query.as_deref())?;
    crate::client::init();
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?
        .put(url)
        .bearer_auth(opts.token.trim())
        .body(content.to_vec())
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("cannot read brief upload response: {e}"))?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("invalid brief upload response: {e}"))?;
    if !status.is_success() || body["ok"].as_bool() != Some(true) {
        let err = body["error"]
            .as_str()
            .unwrap_or("brief upload failed")
            .to_owned();
        return Err(err);
    }
    Ok(BriefRecord {
        id: body["id"].as_str().unwrap_or(id).to_owned(),
        sha256: body["sha256"].as_str().unwrap_or_default().to_owned(),
        bytes: body["bytes"].as_u64().unwrap_or(content.len() as u64) as usize,
    })
}

/// Downloads a brief via `GET /api/brief/<id_or_sha>` and verifies its SHA 256 if expected.
pub async fn get_brief_http(
    opts: &crate::client::Opts,
    id_or_sha: &str,
    expected_sha256: Option<&str>,
) -> Result<(String, String), String> {
    let url = brief_http_url(&opts.url, id_or_sha, None)?;
    crate::client::init();
    // A key member signs in with its key, a token member sends the token.
    let auth = if crate::key::client::marker_path(&opts.token).is_some() {
        crate::key::client::auth_header(&opts.url, &opts.token)
            .await
            .map_err(|e| e.msg)?
    } else {
        format!("Bearer {}", opts.token.trim())
    };
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .header("authorization", auth)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("brief fetch failed with HTTP {}", resp.status()));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| format!("cannot read brief response: {e}"))?;
    let actual = sha256_hex(text.as_bytes());
    if let Some(exp) = expected_sha256.map(str::trim).filter(|s| !s.is_empty())
        && !exp.eq_ignore_ascii_case(&actual)
    {
        return Err(format!(
            "brief sha256 mismatch for `{id_or_sha}`: expected {exp}, got {actual}"
        ));
    }
    Ok((text, actual))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("md.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// HTTP response returned by `brief::on_http` and `hooks::on_http`.
#[derive(Clone, Debug)]
pub struct HttpOutcome {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

impl HttpOutcome {
    pub fn json(status: u16, val: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: val.to_string().into_bytes(),
            headers: vec![("Cache-Control", "no-store".into())],
        }
    }

    pub fn text(status: u16, content_type: &'static str, text: impl Into<String>) -> Self {
        Self {
            status,
            content_type,
            body: text.into().into_bytes(),
            headers: vec![("Cache-Control", "no-store".into())],
        }
    }
}

/// True for a member that signed in with a key: `check_key` gives it an empty hash and no plain token.
fn is_key_member(m: &Member) -> bool {
    m.hash.is_empty() && m.token.is_none()
}

/// True for 64 hex characters, a brief named by its hash.
fn looks_like_hash(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|c| c.is_ascii_hexdigit())
}

/// Handles `PUT /api/brief/<id>` (orchestrator token auth, max 64 KiB)
/// and `GET /api/brief/<id>` (member or orchestrator token auth).
///
/// A key member reads a brief only by the id of an order on which it holds the active lease, never by
/// hash, and only while it has a tier. The tables come from the state database in `data_dir`, read
/// after the log is caught up. Token members and orchestrators are unchanged.
pub fn on_http(
    data_dir: &Path,
    method: &str,
    path: &str,
    query: Option<&str>,
    body: &[u8],
    caller: Option<&Member>,
) -> Option<HttpOutcome> {
    let holds = |member: &Member, id: &str| {
        let Ok(mut store) = crate::store::Store::open(data_dir) else {
            return false;
        };
        let _ = store.catch_up_from_log(&data_dir.join("bus.jsonl"));
        let conn = store.connection();
        let leased = crate::task::query_active_lease_for_task(conn, id)
            .ok()
            .flatten()
            .is_some_and(|l| l.owner == member.name);
        leased && crate::stamp::tier_table::tier_from_table(conn, &member.name, None) >= 0
    };
    route(data_dir, method, path, query, body, caller, &holds)
}

fn route(
    data_dir: &Path,
    method: &str,
    path: &str,
    query: Option<&str>,
    body: &[u8],
    caller: Option<&Member>,
    key_holds: &dyn Fn(&Member, &str) -> bool,
) -> Option<HttpOutcome> {
    let rest = path.strip_prefix("/api/brief/")?;
    let id = match validate_brief_id(rest) {
        Ok(id) => id,
        Err(e) => return Some(HttpOutcome::json(400, &json!({ "ok": false, "error": e }))),
    };

    let Some(member) = caller else {
        return Some(HttpOutcome::json(
            401,
            &json!({ "ok": false, "error": "missing or invalid token" }),
        ));
    };

    match method {
        "PUT" => {
            if member.role != Role::Orchestrator {
                return Some(HttpOutcome::json(
                    403,
                    &json!({ "ok": false, "error": "only an orchestrator token may PUT /api/brief/<id>" }),
                ));
            }
            if body.len() > MAX_BRIEF_BYTES {
                return Some(HttpOutcome::json(
                    413,
                    &json!({
                        "ok": false,
                        "error": format!("brief is {} bytes, maximum is {} bytes (64 KiB)", body.len(), MAX_BRIEF_BYTES)
                    }),
                ));
            }
            let expected_sha = query.and_then(|q| {
                q.split('&')
                    .filter_map(|kv| kv.split_once('='))
                    .find(|(k, _)| *k == "sha256")
                    .map(|(_, v)| v)
            });
            match put_brief(data_dir, id, body, expected_sha) {
                Ok(rec) => Some(HttpOutcome::json(
                    200,
                    &json!({
                        "ok": true,
                        "id": rec.id,
                        "sha256": rec.sha256,
                        "bytes": rec.bytes,
                    }),
                )),
                Err(e) => Some(HttpOutcome::json(400, &json!({ "ok": false, "error": e }))),
            }
        }
        "GET" | "HEAD" => {
            if !matches!(member.role, Role::Orchestrator | Role::Member) {
                return Some(HttpOutcome::json(
                    403,
                    &json!({ "ok": false, "error": "member or orchestrator token required for GET /api/brief/<id>" }),
                ));
            }
            if is_key_member(member) {
                // A key member reads the brief of the order it holds and nothing else.
                if looks_like_hash(id) || !key_holds(member, id) {
                    return Some(HttpOutcome::json(
                        403,
                        &json!({ "ok": false, "error": "a key member reads a brief only by the id of an order it holds the active lease of" }),
                    ));
                }
            }
            match get_brief(data_dir, id) {
                Ok(Some((text, sha256))) => {
                    let mut out = HttpOutcome::text(200, "text/markdown; charset=utf-8", text);
                    out.headers.push(("X-Brief-Sha256", sha256));
                    Some(out)
                }
                Ok(None) => Some(HttpOutcome::json(
                    404,
                    &json!({ "ok": false, "error": format!("brief `{id}` not found") }),
                )),
                Err(e) => Some(HttpOutcome::json(400, &json!({ "ok": false, "error": e }))),
            }
        }
        _ => {
            let mut out = HttpOutcome::json(
                405,
                &json!({ "ok": false, "error": "only GET, HEAD and PUT are allowed on /api/brief/<id>" }),
            );
            out.headers.push(("Allow", "GET, HEAD, PUT".into()));
            Some(out)
        }
    }
}
