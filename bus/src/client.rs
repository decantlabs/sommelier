//! Client side: connect with a token, one shot requests, and the reconnecting tail.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message as WsMsg};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::msg::Message;
use crate::proto::ConnectParams;

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Where the hub is and who we are.
///
/// `url` may hold a single hub URL (`ws://127.0.0.1:7300`) or a comma separated
/// list of hub addresses (`ws://primary:7300,ws://standby:7301`). Clients try
/// addresses in order and take the first that answers as primary.
#[derive(Clone, Debug)]
pub struct Opts {
    /// `ws://127.0.0.1:7300` or `wss://bus.example.org` (or comma separated list).
    pub url: String,
    pub token: String,
}

impl Opts {
    /// Splits `url` on commas into trimmed non-empty hub addresses in priority order.
    pub fn urls(&self) -> Vec<String> {
        parse_urls(&self.url)
    }

    /// Builds [`Opts`] from an ordered list of hub addresses.
    pub fn from_urls(urls: &[String], token: impl Into<String>) -> Self {
        Self {
            url: urls.join(","),
            token: token.into(),
        }
    }
}

/// Parses a comma separated list of hub URLs into trimmed non-empty strings.
pub fn parse_urls(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A failed connect. `fatal` means retrying will not help (bad token, bad URL).
#[derive(Debug)]
pub struct ConnectError {
    pub msg: String,
    pub fatal: bool,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

/// Client ping interval. Cloudflare closes WebSockets that stay silent for about 100 s.
pub const PING_EVERY: Duration = Duration::from_secs(30);

/// Installs the TLS crypto provider for `wss://` URLs. Safe to call more than once.
pub fn init() {
    #[cfg(feature = "tls")]
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Opens a connection. The token travels in the `Authorization` header, never in the URL.
///
/// When `opts.url` lists multiple hub addresses (comma separated), tries each in order
/// and returns the first that answers as primary (skipping unreachable hosts and 503 standby).
pub async fn connect(opts: &Opts, params: &ConnectParams) -> Result<Ws, ConnectError> {
    init();
    let urls = opts.urls();
    if urls.is_empty() {
        return Err(ConnectError {
            msg: "no hub URL configured".into(),
            fatal: true,
        });
    }
    let mut last_err = None;
    for url in &urls {
        match connect_one(url, &opts.token, params).await {
            Ok(ws) => return Ok(ws),
            Err(e) if e.fatal => return Err(e),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or(ConnectError {
        msg: "no hub address answered".into(),
        fatal: false,
    }))
}

async fn connect_one(
    raw_url: &str,
    token: &str,
    params: &ConnectParams,
) -> Result<Ws, ConnectError> {
    let fatal = |msg: String| ConnectError { msg, fatal: true };
    let mut url = raw_url.trim().to_owned();
    // `ws://host:port` without a path would make the request line `GET ?since=1`.
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    if !url[after_scheme..].contains('/') {
        url.push('/');
    }
    let q = params.to_query();
    if !q.is_empty() {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&q);
    }
    let mut req = url
        .as_str()
        .into_client_request()
        .map_err(|e| fatal(format!("bad hub URL `{raw_url}`: {e}")))?;
    // A token that names a key file signs in with the key: the key never leaves the file.
    let value = if crate::key::client::marker_path(token).is_some() {
        crate::key::client::auth_header(raw_url, token)
            .await
            .map_err(|e| ConnectError {
                msg: e.msg,
                fatal: e.fatal,
            })?
    } else {
        format!("Bearer {}", token.trim())
    };
    let auth = HeaderValue::from_str(&value)
        .map_err(|_| fatal("the token contains characters a header cannot carry".into()))?;
    req.headers_mut().insert("authorization", auth);
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(resp)) => {
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            let code = resp.status().as_u16();
            Err(ConnectError {
                msg: format!("hub refused the connection: {code} {body}"),
                fatal: (400..500).contains(&code) && code != 408 && code != 429,
            })
        }
        Err(tungstenite::Error::Url(e)) => Err(fatal(format!("bad hub URL `{raw_url}`: {e}"))),
        Err(e) => Err(ConnectError {
            msg: format!("cannot reach the hub at {raw_url}: {e}"),
            fatal: false,
        }),
    }
}

/// Fetches and verifies the current policy from the authenticated HTTP endpoint.
/// Tries each configured hub address in order.
pub async fn fetch_policy(opts: &Opts) -> Result<crate::policy::Snapshot, String> {
    let urls = opts.urls();
    if urls.is_empty() {
        return Err("no hub URL configured".into());
    }
    let mut last_err = String::from("no hub address answered");
    for raw_url in &urls {
        match fetch_policy_one(raw_url, &opts.token).await {
            Ok(snap) => return Ok(snap),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

async fn fetch_policy_one(raw_url: &str, token: &str) -> Result<crate::policy::Snapshot, String> {
    if crate::key::client::marker_path(token).is_some() {
        return Err("the policy is for token members, a key member does not fetch it".into());
    }
    let mut url = reqwest::Url::parse(raw_url.trim()).map_err(|_| "invalid hub URL")?;
    let scheme = match url.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return Err("invalid hub URL scheme".into()),
    };
    url.set_scheme(scheme)
        .map_err(|_| "invalid hub URL scheme")?;
    url.set_path("/api/policy");
    url.set_query(None);
    url.set_fragment(None);
    init();
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?
        .get(url)
        .bearer_auth(token.trim())
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "policy request failed with HTTP {}",
            response.status()
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let response =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid policy response: {e}"))?;
    crate::policy::Snapshot::verify(response).map_err(|e| e.to_string())
}

/// One request connection (no live stream) for several requests in a row.
pub struct Conn {
    ws: Ws,
}

impl Conn {
    pub async fn open(opts: &Opts) -> Result<Conn, String> {
        let params = ConnectParams {
            live: false,
            ..Default::default()
        };
        let ws = connect(opts, &params).await.map_err(|e| e.msg)?;
        Ok(Conn { ws })
    }

    /// Sends one request and waits up to 20 s for its reply.
    pub async fn call(&mut self, req: &Value) -> Result<Value, String> {
        self.ws
            .send(WsMsg::text(req.to_string()))
            .await
            .map_err(|e| format!("send: {e}"))?;
        let ws = &mut self.ws;
        let wait = async {
            while let Some(frame) = ws.next().await {
                match frame.map_err(|e| format!("receive: {e}"))? {
                    WsMsg::Text(t) => {
                        let v: Value = serde_json::from_str(t.as_str())
                            .map_err(|e| format!("bad reply: {e}"))?;
                        if v.get("reply").is_some() {
                            return Ok(v);
                        }
                    }
                    WsMsg::Close(f) => return Err(format!("hub closed the connection: {f:?}")),
                    _ => {}
                }
            }
            Err("hub closed the connection".to_string())
        };
        tokio::time::timeout(Duration::from_secs(20), wait)
            .await
            .map_err(|_| "no reply within 20 s".to_string())?
    }

    /// Like [`Conn::call`], turns `ok:false` into an error and waits out a rate limit
    /// (the hub says how long) instead of failing.
    pub async fn call_ok(&mut self, req: &Value) -> Result<Value, String> {
        for _ in 0..30 {
            let v = self.call(req).await?;
            if v["ok"].as_bool() == Some(true) {
                return Ok(v);
            }
            let e = v["error"].as_str().unwrap_or("request refused");
            let Some(wait) = e
                .strip_prefix("rate limit")
                .and_then(|r| r.split("try again in ").nth(1))
                .and_then(|r| r.trim_end_matches(" s").parse::<f64>().ok())
            else {
                return Err(e.to_owned());
            };
            tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.05, 30.0) + 0.05)).await;
        }
        Err("still rate limited after 30 tries".into())
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

/// Sends one request on a fresh connection without the live stream and returns the reply.
pub async fn request(opts: &Opts, req: Value) -> Result<Value, String> {
    let mut c = Conn::open(opts).await?;
    let reply = c.call(&req).await;
    c.close().await;
    reply
}

/// Like [`request`] but turns `ok:false` into an error with the hub's text.
pub async fn request_ok(opts: &Opts, req: Value) -> Result<Value, String> {
    let v = request(opts, req).await?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v)
    } else {
        Err(v["error"].as_str().unwrap_or("request refused").to_owned())
    }
}

/// Streams messages to `on_msg` (the parsed message and its raw JSON line) until it
/// returns false. Follows the live stream and reconnects with `since=<last id + 1>`,
/// so nothing is lost while the connection is down.
///
/// With `once`, only the replay asked for by `since` or `last` is delivered, then the
/// call returns: the hub sends the whole replay before it answers the `whoami` that
/// follows the connect, so that answer marks the end.
pub async fn tail(
    opts: &Opts,
    mut params: ConnectParams,
    once: bool,
    mut on_msg: impl FnMut(&Message, &str) -> bool,
) -> Result<(), String> {
    let reconnect = !once;
    if once {
        params.live = false;
    }
    let mut last_id: Option<u64> = None;
    let mut backoff = Duration::from_secs(1);
    loop {
        if let Some(l) = last_id {
            params.since = Some(l + 1);
            params.last = None;
        }
        match connect(opts, &params).await {
            Ok(ws) => {
                backoff = Duration::from_secs(1);
                let (mut sink, mut stream) = ws.split();
                // The newest id gives a resume point even if no message arrives before a drop.
                let _ = sink
                    .send(WsMsg::text(
                        json!({"op": "whoami", "rid": "tail"}).to_string(),
                    ))
                    .await;
                let mut ping = tokio::time::interval(PING_EVERY);
                ping.tick().await;
                loop {
                    tokio::select! {
                        frame = stream.next() => match frame {
                            Some(Ok(WsMsg::Text(t))) => {
                                let t = t.as_str();
                                let Ok(v) = serde_json::from_str::<Value>(t) else { continue };
                                if v.get("reply").is_some() {
                                    if v["ok"].as_bool() != Some(true) {
                                        eprintln!("sommelier: {}", v["error"].as_str().unwrap_or("error"));
                                    } else if v["rid"] == "tail" {
                                        if once {
                                            let _ = sink.close().await;
                                            return Ok(());
                                        }
                                        if last_id.is_none() {
                                            last_id = v["last_id"].as_u64();
                                        }
                                    }
                                    continue;
                                }
                                let Ok(m) = serde_json::from_value::<Message>(v) else { continue };
                                last_id = Some(last_id.map_or(m.id, |l| l.max(m.id)));
                                if !on_msg(&m, t) {
                                    let _ = sink.close().await;
                                    return Ok(());
                                }
                            }
                            Some(Ok(WsMsg::Close(f))) => {
                                if let Some(f) = f {
                                    eprintln!("sommelier: hub closed the connection: {}", f.reason);
                                    if f.reason.as_str() == "token revoked" {
                                        return Err("token revoked".into());
                                    }
                                    if f.reason.as_str() == "key revoked" {
                                        return Err("key revoked".into());
                                    }
                                }
                                break;
                            }
                            None | Some(Err(_)) => break,
                            Some(Ok(_)) => {}
                        },
                        _ = ping.tick() => {
                            if sink.send(WsMsg::Ping(Default::default())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) if e.fatal => return Err(e.msg),
            Err(e) => eprintln!("sommelier: {e}"),
        }
        if !reconnect {
            return Err("connection closed".into());
        }
        eprintln!("sommelier: reconnecting in {} s", backoff.as_secs());
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}
