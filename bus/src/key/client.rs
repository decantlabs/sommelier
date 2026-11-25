//! The client side of a key member: the key file, joining, signing in, rotation, revocation.
//!
//! The private key stays in `member.key` in the client data folder. What the rest of the
//! client sees as its "token" is a marker line that names that file, so the key itself never
//! travels through the places that handle bearer tokens.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::book::{revoke_request, rotate_request};
use super::crypto::{MemberKey, auth_text, b64, member_id, sign_request, solve_pow, unb64};
use crate::client::{Opts, parse_urls, request_ok};

/// Name of the key file in the client data folder.
pub const KEY_FILE: &str = "member.key";

/// Name of the marker file next to it. Commands that read a token file read this one.
pub const MARKER_FILE: &str = "member.auth";

/// A token that starts with this names a key file: `sommelier-key-file:<path>`.
pub const MARKER: &str = "sommelier-key-file:";

const KEY_TAG: &str = "sommelier-key-v1";

/// A key file: the member id and the current key pair.
#[derive(Debug)]
pub struct KeyFile {
    pub member: String,
    pub key: MemberKey,
}

/// Path of the key file in `data_dir`.
pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(KEY_FILE)
}

/// The token string that makes a client sign in with the key file at `path`.
pub fn marker_for(path: &Path) -> String {
    format!("{MARKER}{}", path.display())
}

/// The path in a marker token, `None` for a bearer token.
pub fn marker_path(token: &str) -> Option<PathBuf> {
    token.trim().strip_prefix(MARKER).map(PathBuf::from)
}

/// Reads a key file.
pub fn load(path: &Path) -> Result<KeyFile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the key file {}: {e}", path.display()))?;
    let mut parts = text.split_whitespace();
    let (Some(KEY_TAG), Some(member), Some(secret), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(format!(
            "{} is not a sommelier key file, run `sommelier join` to make one",
            path.display()
        ));
    };
    let bytes = unb64(secret).map_err(|e| format!("{}: the key is {e}", path.display()))?;
    Ok(KeyFile {
        member: member.to_owned(),
        key: MemberKey::from_pkcs8(&bytes).map_err(|e| format!("{}: {e}", path.display()))?,
    })
}

/// Writes a key file through a temporary file. On Unix only the owner may read it.
pub fn save(path: &Path, file: &KeyFile) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("key.tmp");
    let line = format!("{KEY_TAG} {} {}\n", file.member, b64(file.key.pkcs8()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    f.write_all(line.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// Loads the key file of `data_dir` or makes one. The flag says whether it was made now.
pub fn load_or_create(data_dir: &Path) -> Result<(KeyFile, bool), String> {
    let path = key_path(data_dir);
    if path.is_file() {
        return Ok((load(&path)?, false));
    }
    let key = MemberKey::generate()?;
    let file = KeyFile {
        member: member_id(&key.public()),
        key,
    };
    save(&path, &file)?;
    write_marker(data_dir)?;
    Ok((file, true))
}

fn write_marker(data_dir: &Path) -> Result<(), String> {
    let path = data_dir.join(MARKER_FILE);
    std::fs::write(&path, format!("{}\n", marker_for(&key_path(data_dir))))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The marker file of the default data folder when a key file is there, for commands that
/// have no token. The file holds a path, never a secret.
pub fn default_marker_file() -> Option<String> {
    let dir = crate::default_data_dir();
    if !key_path(&dir).is_file() {
        return None;
    }
    let marker = dir.join(MARKER_FILE);
    if !marker.is_file() {
        write_marker(&dir).ok()?;
    }
    Some(marker.display().to_string())
}

/// `ws://host:port` and friends as the http URL of `path` on the same hub.
pub fn http_url(raw: &str, path: &str) -> Result<reqwest::Url, String> {
    let mut url =
        reqwest::Url::parse(raw.trim()).map_err(|_| format!("invalid hub URL `{raw}`"))?;
    let scheme = match url.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return Err("invalid hub URL scheme".into()),
    };
    url.set_scheme(scheme)
        .map_err(|_| "invalid hub URL scheme".to_string())?;
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn http_client() -> Result<reqwest::Client, String> {
    crate::client::init();
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())
}

/// What `GET /api/challenge` answers.
#[derive(Clone, Debug)]
pub struct Challenge {
    pub hub_id: String,
    pub nonce: String,
    pub pow_bits: u32,
}

/// Fetches a challenge from the hub at `raw_url`.
pub async fn fetch_challenge(raw_url: &str) -> Result<Challenge, String> {
    let url = http_url(raw_url, "/api/challenge")?;
    let resp = http_client()?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("cannot reach the hub at {raw_url}: {}", e.without_url()))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("the hub sent no challenge: {}", e.without_url()))?;
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("the hub sent no challenge: HTTP {status}"))?;
    if !status.is_success() {
        return Err(format!(
            "the hub refused a challenge: {status} {}",
            v["error"].as_str().unwrap_or("")
        ));
    }
    let text = |k: &str| {
        v[k].as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("the challenge has no `{k}`"))
    };
    Ok(Challenge {
        hub_id: text("hub_id")?,
        nonce: text("nonce")?,
        pow_bits: v["pow_bits"].as_u64().unwrap_or(0) as u32,
    })
}

/// A failed sign in. `fatal` means that retrying will not help.
#[derive(Debug)]
pub struct AuthError {
    pub msg: String,
    pub fatal: bool,
}

/// The `Authorization` header value for the key file that `token` names, made with a fresh
/// challenge of the hub at `raw_url`.
pub async fn auth_header(raw_url: &str, token: &str) -> Result<String, AuthError> {
    let path = marker_path(token).ok_or(AuthError {
        msg: "the token does not name a key file".into(),
        fatal: true,
    })?;
    let file = load(&path).map_err(|msg| AuthError { msg, fatal: true })?;
    let ch = fetch_challenge(raw_url)
        .await
        .map_err(|msg| AuthError { msg, fatal: false })?;
    let sig = b64(&file.key.sign(auth_text(&ch.hub_id, &ch.nonce).as_bytes()));
    Ok(format!("SommelierKey {}.{}.{sig}", file.member, ch.nonce))
}

/// What a join returned.
#[derive(Clone, Debug)]
pub struct JoinOutcome {
    pub member: String,
    pub key_path: PathBuf,
    /// True when this call made the key file.
    pub new_key: bool,
    /// True when the hub registered the key now, false when it knew it already.
    pub registered: bool,
}

/// Makes the key if there is none, solves the proof of work and joins the hub.
pub async fn join(
    url_list: &str,
    data_dir: &Path,
    handle: Option<&str>,
) -> Result<JoinOutcome, String> {
    let urls = parse_urls(url_list);
    if urls.is_empty() {
        return Err("no hub URL configured".into());
    }
    let (file, new_key) = load_or_create(data_dir)?;
    let handle = handle.unwrap_or(&file.member).to_owned();
    let public = file.key.public();
    let mut last = String::from("no hub address answered");
    for raw in &urls {
        let ch = match fetch_challenge(raw).await {
            Ok(c) => c,
            Err(e) => {
                last = e;
                continue;
            }
        };
        let (nonce, bits) = (ch.nonce.clone(), ch.pow_bits);
        let pk = public.clone();
        let counter = tokio::task::spawn_blocking(move || solve_pow(&nonce, &pk, bits))
            .await
            .map_err(|e| e.to_string())?;
        let mut body = json!({"pub": file.key.public_b64(), "handle": handle,
                              "nonce": ch.nonce, "counter": counter});
        body["sig"] = json!(sign_request(&file.key, &body));
        let resp = http_client()?
            .post(http_url(raw, "/api/join")?)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("cannot reach the hub at {raw}: {}", e.without_url()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("bad answer to the join: {}", e.without_url()))?;
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|_| format!("bad answer to the join: HTTP {status}"))?;
        if !status.is_success() {
            return Err(format!(
                "the hub refused the join: {}",
                v["error"].as_str().unwrap_or("no reason given")
            ));
        }
        let member = v["member"].as_str().unwrap_or_default().to_owned();
        if member != file.member {
            // The key was registered under its first key: the file learns the id.
            let fixed = KeyFile {
                member: member.clone(),
                key: MemberKey::from_pkcs8(file.key.pkcs8())?,
            };
            save(&key_path(data_dir), &fixed)?;
        }
        return Ok(JoinOutcome {
            member,
            key_path: key_path(data_dir),
            new_key,
            registered: v["joined"].as_bool().unwrap_or(true),
        });
    }
    Err(last)
}

/// Options that sign in with the key file of `data_dir`.
pub fn opts_for(url: &str, data_dir: &Path) -> Opts {
    Opts {
        url: url.to_owned(),
        token: marker_for(&key_path(data_dir)),
    }
}

/// Replaces the key of this member by a new one. Returns the new public key.
pub async fn rotate(url: &str, data_dir: &Path) -> Result<String, String> {
    let path = key_path(data_dir);
    let old = load(&path)?;
    let new = MemberKey::generate()?;
    let mut req = rotate_request(&old.member, &old.key.public_b64(), &new.public_b64());
    let sig = sign_request(&old.key, &req);
    req["sig"] = json!(sig);
    let next = path.with_extension("key.next");
    let next_file = KeyFile {
        member: old.member.clone(),
        key: new,
    };
    save(&next, &next_file)?;
    let body =
        json!({"op": "key.rotate", "new_pub": next_file.key.public_b64(), "sig": req["sig"]});
    match request_ok(&opts_for(url, data_dir), body).await {
        Ok(_) => {
            std::fs::rename(&next, &path)
                .map_err(|e| format!("the hub rotated the key but {} could not be replaced: {e}, the new key is in {}", path.display(), next.display()))?;
            Ok(next_file.key.public_b64())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&next);
            Err(e)
        }
    }
}

/// Revokes this member's own key.
pub async fn revoke(url: &str, data_dir: &Path, reason: &str) -> Result<(), String> {
    let file = load(&key_path(data_dir))?;
    let mut req = revoke_request(&file.member, &file.key.public_b64(), reason);
    let sig = sign_request(&file.key, &req);
    req["sig"] = json!(sig);
    let body = json!({"op": "key.revoke", "reason": reason, "sig": req["sig"]});
    request_ok(&opts_for(url, data_dir), body).await.map(|_| ())
}
