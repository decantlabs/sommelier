//! Plain HTTP on the hub port: the dashboard files and `GET /api/state`.
//!
//! The hub peeks at the request head of every new connection. A WebSocket
//! upgrade goes to the bus as before, anything else is answered here and closed.
//! Only GET and HEAD, only files directly from the UI folder (no listing, no
//! hidden files, no `..`), with a strict Content Security Policy.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::hub::Hub;
use crate::proto::percent_decode;

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = crate::brief::MAX_BRIEF_BYTES;
const MAX_FILE: u64 = 8 * 1024 * 1024;

/// A request head: method, path, query and lower cased header names.
pub struct Head {
    pub len: usize,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: HashMap<String, String>,
}

impl Head {
    pub fn is_websocket(&self) -> bool {
        self.headers
            .get("upgrade")
            .is_some_and(|v| v.to_ascii_lowercase().contains("websocket"))
    }

    fn parse(raw: &str, len: usize) -> Option<Head> {
        let mut lines = raw.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let method = first.next()?.to_owned();
        let target = first.next()?;
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
            None => (target.to_owned(), None),
        };
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        Some(Head {
            len,
            method,
            path,
            query,
            headers,
        })
    }
}

/// Waits until the whole request head has arrived and parses it without consuming it.
pub async fn peek_head(stream: &TcpStream) -> io::Result<Head> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = vec![0u8; MAX_HEAD];
    loop {
        let n = stream.peek(&mut buf).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if let Some(p) = buf[..n].windows(4).position(|w| w == b"\r\n\r\n") {
            let raw = String::from_utf8_lossy(&buf[..p]);
            return Head::parse(&raw, p + 4)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad request line"));
        }
        if n == buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
        if Instant::now() > deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `connect-src` names the page's own host for `wss://` explicitly, since older
/// browsers do not count WebSockets as `'self'`.
fn csp(host: Option<&String>) -> String {
    let host = host.map(|h| h.as_str()).filter(|h| {
        !h.is_empty()
            && h.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b':' | b'[' | b']'))
    });
    let connect = match host {
        Some(h) => format!("'self' wss://{h} ws://{h}"),
        None => "'self'".into(),
    };
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; \
         connect-src {connect}; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    )
}

struct Reply {
    status: u16,
    ctype: &'static str,
    body: Vec<u8>,
    extra: Vec<(&'static str, String)>,
}

impl Reply {
    fn text(status: u16, msg: &str) -> Reply {
        Reply {
            status,
            ctype: "text/plain; charset=utf-8",
            body: format!("{msg}\n").into_bytes(),
            extra: vec![],
        }
    }

    fn json(status: u16, v: &serde_json::Value) -> Reply {
        Reply {
            status,
            ctype: "application/json",
            body: v.to_string().into_bytes(),
            extra: vec![("Cache-Control", "no-store".into())],
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

/// A path below the UI folder: segments of letters, digits, `_`, `.` and `-`, not
/// starting with a dot, so no `..`, no hidden files, no drive letters.
fn ui_file(ui: &Path, url_path: &str) -> Option<PathBuf> {
    let rel = url_path.strip_prefix('/')?;
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let mut p = ui.to_path_buf();
    for seg in rel.split('/') {
        let ok = !seg.is_empty()
            && !seg.starts_with('.')
            && seg.len() <= 100
            && seg
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'));
        if !ok {
            return None;
        }
        p.push(seg);
    }
    // Symlinks must not lead out of the folder either.
    let real = p.canonicalize().ok()?;
    let root = ui.canonicalize().ok()?;
    let md = std::fs::metadata(&real).ok()?;
    (real.starts_with(&root) && md.is_file() && md.len() <= MAX_FILE).then_some(real)
}

fn extract_token(head: &Head) -> (Option<String>, Option<String>) {
    let query_token = head
        .query
        .as_deref()
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "token")
        .and_then(|(_, v)| percent_decode(v).ok());
    let header = head
        .headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_owned());
    (header, query_token)
}

fn key_reply(r: Result<serde_json::Value, (u16, String)>) -> Reply {
    match r {
        Ok(v) => Reply::json(200, &v),
        Err((code, msg)) => Reply::json(code, &serde_json::json!({ "ok": false, "error": msg })),
    }
}

async fn route(hub: &Arc<Hub>, head: &Head, body: &[u8]) -> Reply {
    // Two doors without a token: a nonce, and the join of a new key (see PROTOCOL.md, Key members).
    match (head.path.as_str(), head.method.as_str()) {
        ("/api/challenge", "GET" | "HEAD") => return key_reply(hub.challenge()),
        ("/api/join", "POST") => return key_reply(hub.join(body)),
        ("/api/challenge" | "/api/join", _) => {
            let allow = if head.path == "/api/join" {
                "POST"
            } else {
                "GET, HEAD"
            };
            let mut r = Reply::text(405, &format!("only {allow}"));
            r.extra.push(("Allow", allow.into()));
            return r;
        }
        _ => {}
    }
    if head.path == "/api/brief"
        || head.path.starts_with("/api/brief/")
        || (head.path.starts_with("/api/")
            && !matches!(head.path.as_str(), "/api/state" | "/api/policy"))
    {
        let (header, query_token) = extract_token(head);
        let caller = match hub.check_token(header, query_token) {
            Ok((m, _)) => Some(m),
            Err((code, msg)) => {
                return Reply::json(
                    code.as_u16(),
                    &serde_json::json!({ "ok": false, "error": msg }),
                );
            }
        };
        if let Some(out) = crate::hooks::on_http(
            hub,
            &head.method,
            &head.path,
            head.query.as_deref(),
            body,
            caller.as_ref(),
        ) {
            return Reply {
                status: out.status,
                ctype: out.content_type,
                body: out.body,
                extra: out.headers,
            };
        }
    }
    if head.method != "GET" && head.method != "HEAD" {
        let mut r = Reply::text(405, "only GET and HEAD");
        r.extra.push(("Allow", "GET, HEAD".into()));
        return r;
    }
    if matches!(head.path.as_str(), "/api/state" | "/api/policy") {
        let (header, query_token) = extract_token(head);
        // Key members read the state too, the policy stays with tokens.
        let checked = match head.headers.get("authorization") {
            Some(raw) if head.path == "/api/state" && raw.starts_with("SommelierKey ") => {
                hub.check_auth(Some(raw), None).map(|(m, _)| m)
            }
            _ => hub.check_token(header, query_token).map(|(m, _)| m),
        };
        return match checked {
            Ok(_) if head.path == "/api/policy" => Reply::json(200, &hub.policy_response()),
            Ok(_) => Reply::json(200, &hub.snapshot().await),
            Err((code, msg)) => Reply::json(code.as_u16(), &serde_json::json!({ "error": msg })),
        };
    }
    let Some(ui) = hub.config().ui_dir.as_deref() else {
        return Reply::text(404, "not found (this hub serves no dashboard)");
    };
    match ui_file(ui, &head.path) {
        Some(file) => match tokio::fs::read(&file).await {
            Ok(body) => Reply {
                status: 200,
                ctype: content_type(&file),
                body,
                extra: vec![
                    ("Cache-Control", "no-cache".into()),
                    ("Content-Security-Policy", csp(head.headers.get("host"))),
                ],
            },
            Err(_) => Reply::text(404, "not found"),
        },
        None => Reply::text(404, "not found"),
    }
}

/// Answers one plain HTTP request and closes the connection.
pub async fn serve(hub: &Arc<Hub>, mut stream: TcpStream, head: Head) -> io::Result<()> {
    let mut consumed = vec![0u8; head.len];
    stream.read_exact(&mut consumed).await?;

    let content_len = head
        .headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let r = if content_len > MAX_BODY {
        Reply::json(
            413,
            &serde_json::json!({
                "ok": false,
                "error": format!("payload is {content_len} bytes, maximum is {MAX_BODY} bytes (64 KiB)")
            }),
        )
    } else {
        let mut body = vec![0u8; content_len];
        if content_len > 0 {
            tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut body))
                .await
                .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        }
        route(hub, &head, &body).await
    };
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n",
        r.status,
        reason(r.status),
        r.ctype,
        r.body.len()
    );
    for (k, v) in &r.extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    if head.method != "HEAD" {
        bytes.extend_from_slice(&r.body);
    }
    stream.write_all(&bytes).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_paths_stay_inside() {
        let dir = std::env::temp_dir().join(format!("sommelier-ui-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("index.html"), "x").unwrap();
        std::fs::write(dir.join(".secret"), "x").unwrap();
        assert!(ui_file(&dir, "/").is_some());
        assert!(ui_file(&dir, "/index.html").is_some());
        assert!(ui_file(&dir, "/sub").is_none(), "no directories");
        assert!(ui_file(&dir, "/.secret").is_none());
        assert!(ui_file(&dir, "/../Cargo.toml").is_none());
        assert!(ui_file(&dir, "/sub/../index.html").is_none());
        assert!(ui_file(&dir, "/C:/Windows/win.ini").is_none());
        assert!(ui_file(&dir, "/a%2F..").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn csp_host_is_sanitized() {
        assert!(csp(Some(&"bus.example.org".to_string())).contains("wss://bus.example.org"));
        let evil = "x; script-src *".to_string();
        let c = csp(Some(&evil));
        assert!(
            !c.contains("script-src *") && c.contains("connect-src 'self';"),
            "{c}"
        );
    }
}
