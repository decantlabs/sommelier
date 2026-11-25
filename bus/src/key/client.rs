//! The client side of a key member: the key file, joining, signing in, rotation, revocation.
//!
//! The private key stays in `member.key` in the client data folder. What the rest of the
//! client sees as its "token" is a marker line that names that file, so the key itself never
//! travels through the places that handle bearer tokens.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::addr::address_of_url;
use super::book::{revoke_request, rotate_request};
use super::crypto::{MemberKey, auth_text, b64, member_id, sign_request, solve_pow, unb64};
use super::join::MAX_POW_BITS;
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

/// A line for the output of `join` about who can read the key file, or `None` where the file mode
/// already keeps it private. On Windows the file gets no access list of its own: it is as private as
/// the folder it is in.
pub fn privacy_note() -> Option<&'static str> {
    cfg!(windows).then_some(
        "on Windows the key file is as private as the folder it is in: keep it in a folder that only you can open",
    )
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

/// Path of the file that pins the hub id of each hub address, beside the key file `key`.
pub fn pins_path(key: &Path) -> PathBuf {
    key.with_extension("hubs")
}

fn pin_name(raw_url: &str) -> String {
    raw_url.trim().trim_end_matches('/').to_owned()
}

/// Checks the hub id that the hub at `raw_url` answered against the one pinned for that
/// address in `pins`. An address with no pin yet is pinned now. A different id is refused,
/// and so is an id that is already pinned under another address: a hostile hub at a new
/// address could otherwise hand on the id of the real hub and relay a sign in to it.
/// A pin file that cannot be read is an error and is never replaced.
fn check_pin(pins: &Path, raw_url: &str, hub_id: &str) -> Result<(), String> {
    let name = pin_name(raw_url);
    let text = match std::fs::read_to_string(pins) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(format!(
                "cannot read {}: {e}. Your pins are left as they are, fix the file and try again",
                pins.display()
            ));
        }
    };
    for line in text.lines() {
        if let Some((url, id)) = line.rsplit_once(' ')
            && url == name
        {
            if id == hub_id {
                return Ok(());
            }
            return Err(format!(
                "the hub at {name} answers with another hub id than the one you joined: it may be a different hub or a relay. If you moved the hub on purpose, delete the line that starts with {name} in {} and try again",
                pins.display()
            ));
        }
    }
    if let Some(other) = text
        .lines()
        .filter_map(|l| l.rsplit_once(' '))
        .find(|(url, id)| *id == hub_id && *url != name)
        .map(|(url, _)| url)
    {
        return Err(format!(
            "the hub at {name} answers with the hub id of {other}, which you pinned before: one of the two is a relay. If both are really your hub, add the line `{name} {hub_id}` to {} yourself",
            pins.display()
        ));
    }
    let mut out = text;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("{name} {hub_id}\n"));
    let tmp = pins.with_extension("hubs.tmp");
    std::fs::write(&tmp, out).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, pins).map_err(|e| format!("cannot replace {}: {e}", pins.display()))
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
    let bits = v["pow_bits"].as_u64().unwrap_or(0);
    if bits > u64::from(MAX_POW_BITS) {
        return Err(format!(
            "the hub asks for a proof of work of {bits} bits, more than the {MAX_POW_BITS} bits a hub may ask: it is refused"
        ));
    }
    Ok(Challenge {
        hub_id: text("hub_id")?,
        nonce: text("nonce")?,
        pow_bits: bits as u32,
    })
}

/// A failed sign in. `fatal` means that retrying will not help.
#[derive(Debug)]
pub struct AuthError {
    pub msg: String,
    pub fatal: bool,
}

/// The `Authorization` header for `file`, made with a fresh challenge of the hub at `raw_url`.
/// The hub id must match the one pinned in `pins`.
async fn header_for(raw_url: &str, file: &KeyFile, pins: &Path) -> Result<String, AuthError> {
    let ch = fetch_challenge(raw_url)
        .await
        .map_err(|msg| AuthError { msg, fatal: false })?;
    header_with(&ch, raw_url, file, pins)
}

/// [`header_for`] with the challenge already fetched.
fn header_with(
    ch: &Challenge,
    raw_url: &str,
    file: &KeyFile,
    pins: &Path,
) -> Result<String, AuthError> {
    check_pin(pins, raw_url, &ch.hub_id).map_err(|msg| AuthError { msg, fatal: true })?;
    // The address goes into the signed text: the hub checks it against its own public addresses.
    let address = address_of_url(raw_url).map_err(|msg| AuthError { msg, fatal: true })?;
    let sig = b64(&file
        .key
        .sign(auth_text(&ch.hub_id, &address, &ch.nonce).as_bytes()));
    Ok(format!(
        "SommelierKey {}.{}.{}.{sig}",
        file.member,
        ch.nonce,
        b64(address.as_bytes())
    ))
}

/// The `Authorization` header value for the key file that `token` names, made with a fresh
/// challenge of the hub at `raw_url`.
pub async fn auth_header(raw_url: &str, token: &str) -> Result<String, AuthError> {
    let path = marker_path(token).ok_or(AuthError {
        msg: "the token does not name a key file".into(),
        fatal: true,
    })?;
    let file = load(&path).map_err(|msg| AuthError { msg, fatal: true })?;
    settle_next(raw_url, &path).await;
    // A promoted key is now in the file: read it again.
    let file = load(&path).unwrap_or(file);
    header_for(raw_url, &file, &pins_path(&path)).await
}

/// Path of the new key of a rotation that may not have finished.
fn next_path(key: &Path) -> PathBuf {
    key.with_extension("key.next")
}

/// The file that names the hub a rotation was sent to, beside `member.key.next`.
fn next_hub_path(key: &Path) -> PathBuf {
    key.with_extension("key.next.hub")
}

/// When `member.key.next` exists, a rotation may have been written by the hub while its reply
/// was lost. The new key then is the only key the hub takes. Tries it once at the hub the rotation
/// was sent to: when that hub accepts it, it replaces the key file. A hub with another id, or any
/// other outcome, leaves both files as they are.
async fn settle_next(raw_url: &str, key: &Path) {
    let next = next_path(key);
    let (Ok(new), Ok(old)) = (load(&next), load(key)) else {
        return;
    };
    if new.member != old.member {
        return;
    }
    let Ok(sent_to) = std::fs::read_to_string(next_hub_path(key)) else {
        return;
    };
    let Ok(ch) = fetch_challenge(raw_url).await else {
        return;
    };
    if ch.hub_id != sent_to.trim() {
        return;
    }
    let Ok(header) = header_with(&ch, raw_url, &new, &pins_path(key)) else {
        return;
    };
    let (Ok(url), Ok(http)) = (http_url(raw_url, "/api/state"), http_client()) else {
        return;
    };
    let accepted = http
        .get(url)
        .header("authorization", header)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    if accepted && std::fs::rename(&next, key).is_ok() {
        let _ = std::fs::remove_file(next_hub_path(key));
    }
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
        check_pin(&pins_path(&key_path(data_dir)), raw, &ch.hub_id)?;
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
        let computed = member_id(&public);
        if member != file.member && member != computed {
            return Err(format!(
                "the hub answered with the member id {member} but this key makes {computed}: refused, the key file is not changed"
            ));
        }
        if member != file.member {
            // The key file was edited by hand: it learns the id of the key.
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

/// The first of the hubs in `url_list` that answers, with its id checked against the pins: the hub id
/// that a signed request is made for, and the address to send it to.
async fn hub_of(url_list: &str, key: &Path) -> Result<(String, String), String> {
    let mut last = String::from("no hub URL configured");
    for raw in parse_urls(url_list) {
        match fetch_challenge(&raw).await {
            Ok(ch) => {
                check_pin(&pins_path(key), &raw, &ch.hub_id)?;
                return Ok((raw, ch.hub_id));
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Replaces the key of this member by a new one. Returns the new public key.
///
/// The new key waits in `member.key.next` until the hub confirms it. On any error the file
/// stays: the hub may have written the rotation and lost only the reply, and the next sign in
/// tries the new key (see `auth_header`).
pub async fn rotate(url_list: &str, data_dir: &Path) -> Result<String, String> {
    let path = key_path(data_dir);
    let (url, hub_id) = hub_of(url_list, &path).await?;
    let url = url.as_str();
    settle_next(url, &path).await;
    let old = load(&path)?;
    let new = MemberKey::generate()?;
    let mut req = rotate_request(
        &hub_id,
        &old.member,
        &old.key.public_b64(),
        &new.public_b64(),
    );
    let sig = sign_request(&old.key, &req);
    req["sig"] = json!(sig);
    let next = next_path(&path);
    let next_file = KeyFile {
        member: old.member.clone(),
        key: new,
    };
    save(&next, &next_file)?;
    std::fs::write(next_hub_path(&path), format!("{hub_id}\n"))
        .map_err(|e| format!("cannot write {}: {e}", next_hub_path(&path).display()))?;
    let body = json!({"op": "key.rotate", "hub": hub_id, "new_pub": next_file.key.public_b64(),
                      "sig": req["sig"]});
    match request_ok(&opts_for(url, data_dir), body).await {
        Ok(_) => {
            let _ = std::fs::remove_file(next_hub_path(&path));
            std::fs::rename(&next, &path)
                .map_err(|e| format!("the hub rotated the key but {} could not be replaced: {e}, the new key is in {}", path.display(), next.display()))?;
            Ok(next_file.key.public_b64())
        }
        Err(e) => Err(format!(
            "{e}. The new key stays in {}: if the hub did rotate, your next sign in uses it",
            next.display()
        )),
    }
}

/// Revokes this member's own key.
pub async fn revoke(url_list: &str, data_dir: &Path, reason: &str) -> Result<(), String> {
    let file = load(&key_path(data_dir))?;
    let (url, hub_id) = hub_of(url_list, &key_path(data_dir)).await?;
    let mut req = revoke_request(&hub_id, &file.member, &file.key.public_b64(), reason);
    let sig = sign_request(&file.key, &req);
    req["sig"] = json!(sig);
    let body = json!({"op": "key.revoke", "hub": hub_id, "reason": reason, "sig": req["sig"]});
    request_ok(&opts_for(&url, data_dir), body)
        .await
        .map(|_| ())
}
