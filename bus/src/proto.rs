//! Requests a client sends, replies the hub answers with, and the connect parameters.

use serde_json::{Map, Value};

use crate::msg::{Filter, Kind, parse_names};

/// A parsed client request.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Send {
        policy_hash: Option<String>,
        kind: Kind,
        topic: String,
        body: String,
        refs: Vec<String>,
        to: Vec<String>,
    },
    Heartbeat,
    Whoami,
    Claims,
    Who,
    Idle,
    /// Agent rows, of one owner (`me` for the sender) or of everyone.
    Roster {
        owner: Option<String>,
    },
    Log {
        since: Option<u64>,
        last: Option<u64>,
        limit: Option<u64>,
        filter: FilterSpec,
    },
    ModuleOp {
        op: String,
        body: Value,
    },
}

/// Filter as it travels in a request or a URL, still unparsed into [`Filter`] kinds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FilterSpec {
    pub kinds: Vec<Kind>,
    pub topic: Option<String>,
    pub from: Vec<String>,
    /// A name, or `me` for the connection's own name.
    pub to: Option<String>,
    /// A worker name: with it only messages for this worker, for the whole name and for
    /// everyone arrive. It implies `to=me` when `to` is not given.
    pub worker: Option<String>,
}

impl FilterSpec {
    /// `me` is the name of whoever asks, it replaces `to=me`.
    pub fn to_filter(&self, me: &str) -> Filter {
        let to = match (self.to.as_deref(), &self.worker) {
            (None, Some(_)) => Some(me.to_owned()),
            (t, _) => t.map(|t| {
                if t.eq_ignore_ascii_case("me") {
                    me.to_owned()
                } else {
                    t.to_ascii_lowercase()
                }
            }),
        };
        Filter {
            kinds: self.kinds.clone(),
            topic: self.topic.clone(),
            from: self.from.clone(),
            to,
            worker: self.worker.clone(),
        }
    }
}

/// A request with what is needed to answer it even when it did not parse.
#[derive(Debug)]
pub struct Envelope {
    pub op: String,
    pub rid: Option<Value>,
    pub req: Result<Request, String>,
}

/// Fields the hub stamps itself. A client that sends one is refused.
const STAMPED: [&str; 4] = ["id", "ts", "from", "role"];

pub fn parse_request(text: &str) -> Envelope {
    let bad = |op: &str, rid: Option<Value>, e: String| Envelope {
        op: op.into(),
        rid,
        req: Err(e),
    };
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return bad("?", None, format!("not valid JSON: {e}")),
    };
    let Value::Object(map) = v else {
        return bad("?", None, "a request is a JSON object".into());
    };
    let rid = map.get("rid").cloned();
    let op = match map.get("op") {
        Some(Value::String(s)) => s.clone(),
        Some(_) => return bad("?", rid, "`op` must be a string".into()),
        None if map.contains_key("kind") => "send".into(),
        None => {
            return bad(
                "?",
                rid,
                "missing `op` (send, heartbeat, whoami, claims, who, roster, log)".into(),
            );
        }
    };
    let allowed: &[&str] = match op.as_str() {
        "send" => &[
            "op",
            "rid",
            "kind",
            "topic",
            "body",
            "refs",
            "to",
            "policy_hash",
        ],
        "log" => &[
            "op", "rid", "since", "last", "limit", "kinds", "topic", "from", "to", "worker",
        ],
        "heartbeat" | "whoami" | "claims" | "who" | "idle" => &["op", "rid"],
        "roster" => &["op", "rid", "owner"],
        other => {
            let trimmed = other.trim();
            if trimmed.is_empty() {
                return bad(
                    other,
                    rid,
                    "missing `op` (send, heartbeat, whoami, claims, who, idle, roster, log)".into(),
                );
            }
            return Envelope {
                op: trimmed.to_owned(),
                rid,
                req: Ok(Request::ModuleOp {
                    op: trimmed.to_owned(),
                    body: Value::Object(map),
                }),
            };
        }
    };
    for key in map.keys() {
        if op == "send" && STAMPED.contains(&key.as_str()) {
            return bad(
                &op,
                rid,
                format!("field `{key}` is stamped by the hub, a client must not send it"),
            );
        }
        if !allowed.contains(&key.as_str()) {
            return bad(&op, rid, format!("unknown field `{key}` for op {op}"));
        }
    }
    let req = match op.as_str() {
        "send" => parse_send(&map),
        "log" => parse_log(&map),
        "heartbeat" => Ok(Request::Heartbeat),
        "whoami" => Ok(Request::Whoami),
        "claims" => Ok(Request::Claims),
        "idle" => Ok(Request::Idle),
        "roster" => get_str(&map, "owner").map(|o| Request::Roster {
            owner: Some(o).filter(|o| !o.is_empty()),
        }),
        _ => Ok(Request::Who),
    };
    Envelope { op, rid, req }
}

fn get_str(map: &Map<String, Value>, key: &str) -> Result<String, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn get_u64(map: &Map<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non negative integer")),
    }
}

/// An array of strings, or one string with commas.
fn get_list(map: &Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(parse_names(s)),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("`{key}` must hold strings"))
            })
            .collect(),
        Some(_) => Err(format!("`{key}` must be an array of strings")),
    }
}

fn parse_send(map: &Map<String, Value>) -> Result<Request, String> {
    let kind_s = get_str(map, "kind")?;
    let kind = Kind::parse(&kind_s)
        .ok_or_else(|| format!("unknown kind `{kind_s}`, known kinds: {}", Kind::list()))?;
    let refs = match map.get("refs") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "`refs` must hold strings".to_string())
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("`refs` must be an array of strings".into()),
    };
    Ok(Request::Send {
        policy_hash: map
            .contains_key("policy_hash")
            .then(|| get_str(map, "policy_hash"))
            .transpose()?,
        kind,
        topic: get_str(map, "topic")?,
        body: get_str(map, "body")?,
        refs,
        to: get_list(map, "to")?,
    })
}

fn parse_log(map: &Map<String, Value>) -> Result<Request, String> {
    let kinds = get_list(map, "kinds")?
        .iter()
        .map(|k| Kind::parse(k).ok_or_else(|| format!("unknown kind `{k}`")))
        .collect::<Result<_, _>>()?;
    let topic = Some(get_str(map, "topic")?).filter(|t| !t.is_empty());
    Ok(Request::Log {
        since: get_u64(map, "since")?,
        last: get_u64(map, "last")?,
        limit: get_u64(map, "limit")?,
        filter: FilterSpec {
            kinds,
            topic,
            from: get_list(map, "from")?,
            to: Some(get_str(map, "to")?).filter(|t| !t.is_empty()),
            worker: Some(get_str(map, "worker")?).filter(|t| !t.is_empty()),
        },
    })
}

/// `{"reply":op,"ok":true,"rid":...,<extra>}`.
pub fn reply_ok(op: &str, rid: &Option<Value>, extra: Map<String, Value>) -> String {
    let mut m = Map::new();
    m.insert("reply".into(), op.into());
    m.insert("ok".into(), true.into());
    if let Some(r) = rid {
        m.insert("rid".into(), r.clone());
    }
    m.extend(extra);
    Value::Object(m).to_string()
}

/// `{"reply":op,"ok":false,"rid":...,"error":"..."}`.
pub fn reply_err(op: &str, rid: &Option<Value>, error: &str) -> String {
    let mut m = Map::new();
    m.insert("reply".into(), op.into());
    m.insert("ok".into(), false.into());
    if let Some(r) = rid {
        m.insert("rid".into(), r.clone());
    }
    m.insert("error".into(), error.into());
    Value::Object(m).to_string()
}

/// Parameters of a connection, taken from the URL query.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectParams {
    pub token: Option<String>,
    pub since: Option<u64>,
    pub last: Option<u64>,
    pub live: bool,
    pub filter: FilterSpec,
}

impl Default for ConnectParams {
    fn default() -> Self {
        ConnectParams {
            token: None,
            since: None,
            last: None,
            live: true,
            filter: FilterSpec::default(),
        }
    }
}

impl ConnectParams {
    /// Parses `token=..&since=..&last=..&live=0&kinds=READY,FIX&topic=issue/*&from=a,b`.
    pub fn from_query(query: Option<&str>) -> Result<Self, String> {
        let mut p = ConnectParams::default();
        for pair in query.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = percent_decode(v)?;
            let num = |v: &str| {
                v.parse::<u64>()
                    .map_err(|_| format!("`{k}` must be a number"))
            };
            match k {
                "token" => p.token = Some(v),
                "since" => p.since = Some(num(&v)?),
                "last" => p.last = Some(num(&v)?),
                "live" => p.live = !matches!(v.as_str(), "0" | "false" | "no"),
                "kinds" | "kind" => p.filter.kinds = Kind::parse_list(&v)?,
                "topic" => p.filter.topic = Some(v).filter(|t| !t.is_empty()),
                "from" => p.filter.from = parse_names(&v),
                "to" => p.filter.to = Some(v).filter(|t| !t.is_empty()),
                "worker" => p.filter.worker = Some(v).filter(|t| !t.is_empty()),
                _ => return Err(format!("unknown query parameter `{k}`")),
            }
        }
        Ok(p)
    }

    /// The query string a client puts on the URL (the token goes into a header instead).
    pub fn to_query(&self) -> String {
        let mut parts = Vec::new();
        if let Some(s) = self.since {
            parts.push(format!("since={s}"));
        }
        if let Some(l) = self.last {
            parts.push(format!("last={l}"));
        }
        if !self.live {
            parts.push("live=0".into());
        }
        if !self.filter.kinds.is_empty() {
            let k: Vec<_> = self.filter.kinds.iter().map(|k| k.as_str()).collect();
            parts.push(format!("kinds={}", k.join(",")));
        }
        if let Some(t) = &self.filter.topic {
            parts.push(format!("topic={}", percent_encode(t)));
        }
        if !self.filter.from.is_empty() {
            parts.push(format!(
                "from={}",
                percent_encode(&self.filter.from.join(","))
            ));
        }
        if let Some(t) = &self.filter.to {
            parts.push(format!("to={}", percent_encode(t)));
        }
        if let Some(w) = &self.filter.worker {
            parts.push(format!("worker={}", percent_encode(w)));
        }
        parts.join("&")
    }
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b',' | b'*')
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn percent_decode(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex =
                    std::str::from_utf8(&b[i + 1..i + 3]).map_err(|_| "bad percent escape")?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| "bad percent escape")?);
                i += 3;
            }
            b'%' => return Err("bad percent escape".into()),
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "query is not UTF 8".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_parses_and_defaults_op() {
        let e = parse_request(r#"{"kind":"ready","topic":"issue/1","body":"x","refs":["a"]}"#);
        assert_eq!(e.op, "send");
        assert_eq!(
            e.req.unwrap(),
            Request::Send {
                policy_hash: None,
                kind: Kind::Ready,
                topic: "issue/1".into(),
                body: "x".into(),
                refs: vec!["a".into()],
                to: vec![]
            }
        );
    }

    #[test]
    fn stamped_fields_refused() {
        for f in ["from", "id", "ts", "role"] {
            let e = parse_request(&format!(r#"{{"op":"send","kind":"CHAT","{f}":"x"}}"#));
            let err = e.req.unwrap_err();
            assert!(err.contains("stamped by the hub"), "{err}");
        }
    }

    #[test]
    fn errors_are_clear() {
        assert!(parse_request("nope").req.unwrap_err().contains("JSON"));
        assert!(
            parse_request(r#"{"op":""}"#)
                .req
                .unwrap_err()
                .contains("missing `op`")
        );
        let mod_env = parse_request(r#"{"op":"quota.report","owner":"alice","rid":42}"#);
        assert_eq!(mod_env.op, "quota.report");
        assert_eq!(mod_env.rid, Some(Value::from(42)));
        assert_eq!(
            mod_env.req.unwrap(),
            Request::ModuleOp {
                op: "quota.report".into(),
                body: serde_json::json!({"op":"quota.report","owner":"alice","rid":42}),
            }
        );
        assert!(
            parse_request(r#"{"op":"send","kind":"WAT"}"#)
                .req
                .unwrap_err()
                .contains("unknown kind")
        );
        assert!(
            parse_request(r#"{"op":"whoami","x":1}"#)
                .req
                .unwrap_err()
                .contains("unknown field")
        );
        let e = parse_request(r#"{"op":"whoami","rid":7}"#);
        assert_eq!(e.rid, Some(Value::from(7)));
    }

    #[test]
    fn log_request() {
        let e = parse_request(
            r#"{"op":"log","since":5,"kinds":"READY,FIX","from":["a"],"topic":"issue/*"}"#,
        );
        match e.req.unwrap() {
            Request::Log { since, filter, .. } => {
                assert_eq!(since, Some(5));
                assert_eq!(filter.kinds, vec![Kind::Ready, Kind::Fix]);
                assert_eq!(filter.topic.as_deref(), Some("issue/*"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn query_round_trip() {
        let q =
            "token=sb_x&since=12&kinds=READY,RUN_RESULT&topic=run%2F4&from=a,b&live=0&worker=w.1";
        let p = ConnectParams::from_query(Some(q)).unwrap();
        assert_eq!(p.token.as_deref(), Some("sb_x"));
        assert_eq!(p.since, Some(12));
        assert!(!p.live);
        assert_eq!(p.filter.topic.as_deref(), Some("run/4"));
        assert_eq!(p.filter.from, vec!["a", "b"]);
        assert_eq!(p.filter.worker.as_deref(), Some("w.1"));
        assert_eq!(p.filter.to_filter("me1").to.as_deref(), Some("me1"));
        let back = ConnectParams::from_query(Some(&p.to_query())).unwrap();
        assert_eq!(back.filter, p.filter);
        assert!(ConnectParams::from_query(Some("bogus=1")).is_err());
        assert_eq!(percent_decode("a%20b+c").unwrap(), "a b c");
        assert!(percent_decode("a%2").is_err());
    }
}
