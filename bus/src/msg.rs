//! The message: kinds, roles, limits, validation, filters and the one line form.

use serde::{Deserialize, Serialize};

/// What a message is about. Serialized as the upper case name, `RUN_REQUEST` and so on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Kind {
    Ready,
    Landed,
    Fix,
    Stuck,
    Finding,
    Claim,
    Release,
    Task,
    Accept,
    Decline,
    RunRequest,
    RunResult,
    Hello,
    Chat,
    Status,
    Policy,
    Alert,
    Fact,
    Offer,
    TaskUpdate,
}

impl Kind {
    pub const ALL: [Kind; 20] = [
        Kind::Ready,
        Kind::Landed,
        Kind::Fix,
        Kind::Stuck,
        Kind::Finding,
        Kind::Claim,
        Kind::Release,
        Kind::Task,
        Kind::Accept,
        Kind::Decline,
        Kind::RunRequest,
        Kind::RunResult,
        Kind::Hello,
        Kind::Chat,
        Kind::Status,
        Kind::Policy,
        Kind::Alert,
        Kind::Fact,
        Kind::Offer,
        Kind::TaskUpdate,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Ready => "READY",
            Kind::Landed => "LANDED",
            Kind::Fix => "FIX",
            Kind::Stuck => "STUCK",
            Kind::Finding => "FINDING",
            Kind::Claim => "CLAIM",
            Kind::Release => "RELEASE",
            Kind::Task => "TASK",
            Kind::Accept => "ACCEPT",
            Kind::Decline => "DECLINE",
            Kind::RunRequest => "RUN_REQUEST",
            Kind::RunResult => "RUN_RESULT",
            Kind::Hello => "HELLO",
            Kind::Chat => "CHAT",
            Kind::Status => "STATUS",
            Kind::Policy => "POLICY",
            Kind::Alert => "ALERT",
            Kind::Fact => "FACT",
            Kind::Offer => "OFFER",
            Kind::TaskUpdate => "TASK_UPDATE",
        }
    }

    /// Case insensitive, `run-request` and `run_request` both work.
    pub fn parse(s: &str) -> Option<Kind> {
        let up = s.trim().to_ascii_uppercase().replace('-', "_");
        Kind::ALL.iter().copied().find(|k| k.as_str() == up)
    }

    /// `READY, LANDED, ...` for error messages.
    pub fn list() -> String {
        Kind::ALL.map(Kind::as_str).join(", ")
    }

    /// Parses `READY,FIX` (commas or spaces).
    pub fn parse_list(s: &str) -> Result<Vec<Kind>, String> {
        s.split([',', ' '])
            .filter(|p| !p.is_empty())
            .map(|p| {
                Kind::parse(p)
                    .ok_or_else(|| format!("unknown kind `{p}`, known kinds: {}", Kind::list()))
            })
            .collect()
    }
}

/// What a token may do. `Hub` is only used by the hub for its own messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Orchestrator,
    Member,
    Observer,
    Hub,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Orchestrator => "orchestrator",
            Role::Member => "member",
            Role::Observer => "observer",
            Role::Hub => "hub",
        }
    }

    /// Roles a token can be issued for (not `hub`).
    pub fn parse_issuable(s: &str) -> Option<Role> {
        match s.trim().to_ascii_lowercase().as_str() {
            "orchestrator" => Some(Role::Orchestrator),
            "member" => Some(Role::Member),
            "observer" => Some(Role::Observer),
            _ => None,
        }
    }
}

/// One message as stored and as sent to subscribers. Field order is the wire order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub ts: String,
    pub from: String,
    pub role: Role,
    pub kind: Kind,
    pub topic: String,
    pub body: String,
    pub refs: Vec<String>,
    /// Addressees by name. Empty means everyone. Left out of the JSON when empty,
    /// so frames without an addressee look exactly like protocol version 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
}

/// The default body limit of a kind in characters, `None` for kinds without one (STATUS has its
/// own checks). Short messages are cheap: every one is read by the agents of everyone it reaches.
pub fn default_body_chars(kind: Kind) -> Option<usize> {
    match kind {
        Kind::Accept | Kind::Decline => Some(200),
        Kind::Chat | Kind::Task | Kind::RunRequest | Kind::Claim | Kind::Release | Kind::Hello => {
            Some(600)
        }
        Kind::Ready | Kind::Fix | Kind::Stuck | Kind::RunResult | Kind::Finding | Kind::Landed => {
            Some(1200)
        }
        Kind::Status | Kind::Policy | Kind::Alert | Kind::Fact | Kind::Offer | Kind::TaskUpdate => {
            None
        }
    }
}

/// Size limits. The frame, topic and refs limits are in bytes of UTF 8, the body limit by kind is
/// in characters.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Body limit in characters per kind, on top of `max_body`. A kind missing here has none.
    pub kind_chars: std::collections::HashMap<Kind, usize>,
    /// A whole request frame.
    pub max_frame: usize,
    pub max_body: usize,
    pub max_topic: usize,
    pub max_refs: usize,
    pub max_ref: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            kind_chars: Kind::ALL
                .iter()
                .filter_map(|k| default_body_chars(*k).map(|n| (*k, n)))
                .collect(),
            max_frame: 8192,
            max_body: 4096,
            max_topic: 128,
            max_refs: 16,
            max_ref: 128,
        }
    }
}

/// Refuses a body longer than the limit of its kind and says how to cut it down.
pub fn check_kind_limit(limits: &Limits, kind: Kind, body: &str) -> Result<(), String> {
    let Some(max) = limits.kind_chars.get(&kind).copied() else {
        return Ok(());
    };
    let n = body.chars().count();
    if n > max {
        return Err(format!(
            "{} body is {n} characters, the limit for {} is {max}. Put details into the commit, the issue or a file and name it",
            kind.as_str(),
            kind.as_str()
        ));
    }
    Ok(())
}

/// Parses `READY=1500,CHAT=300` (commas or spaces) into overrides of the body limits.
pub fn parse_kind_limits(s: &str) -> Result<Vec<(Kind, usize)>, String> {
    s.split([',', ' '])
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, n) = p
                .split_once('=')
                .ok_or_else(|| format!("`{p}` is not KIND=CHARS"))?;
            let kind = Kind::parse(k)
                .ok_or_else(|| format!("unknown kind `{k}`, known kinds: {}", Kind::list()))?;
            let n = n
                .parse::<usize>()
                .map_err(|_| format!("`{n}` is not a number of characters"))?;
            Ok((kind, n))
        })
        .collect()
}

/// Checks the client supplied parts of a message.
pub fn validate(limits: &Limits, topic: &str, body: &str, refs: &[String]) -> Result<(), String> {
    if body.len() > limits.max_body {
        return Err(format!(
            "body is {} bytes, the limit is {}",
            body.len(),
            limits.max_body
        ));
    }
    if body.chars().any(char::is_control) {
        return Err("body contains a newline or another control character, a message is one line (join lines with ` | `)".into());
    }
    if topic.len() > limits.max_topic {
        return Err(format!(
            "topic is {} bytes, the limit is {}",
            topic.len(),
            limits.max_topic
        ));
    }
    if topic.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("topic must not contain spaces or control characters".into());
    }
    if refs.len() > limits.max_refs {
        return Err(format!(
            "{} refs, the limit is {}",
            refs.len(),
            limits.max_refs
        ));
    }
    for r in refs {
        if r.is_empty() || r.len() > limits.max_ref {
            return Err(format!("each ref must have 1 to {} bytes", limits.max_ref));
        }
        if r.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!(
                "ref `{r}` must not contain spaces or control characters"
            ));
        }
    }
    Ok(())
}

/// Which messages a subscriber wants. Empty means everything.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub kinds: Vec<Kind>,
    /// Exact topic, or a prefix when it ends with `*` (`issue/*`).
    pub topic: Option<String>,
    pub from: Vec<String>,
    /// Only messages for everyone or addressed to this name.
    pub to: Option<String>,
    /// With `to`: also leave out messages addressed to another worker of the same name
    /// (`name/other`). Messages for `name` alone, for `name/worker` and for everyone stay.
    pub worker: Option<String>,
}

impl Filter {
    pub fn matches(&self, m: &Message) -> bool {
        self.to.as_deref().is_none_or(|me| {
            m.to.is_empty()
                || m.to.iter().any(|t| match split_addressee(t) {
                    (name, None) => name == me,
                    (name, Some(w)) => {
                        name == me && self.worker.as_deref().is_none_or(|mine| mine == w)
                    }
                })
        }) && (self.kinds.is_empty() || self.kinds.contains(&m.kind))
            && self
                .topic
                .as_deref()
                .is_none_or(|t| topic_matches(t, &m.topic))
            && (self.from.is_empty() || self.from.iter().any(|f| f == &m.from))
    }
}

/// Splits an addressee `name/worker` into its two parts.
pub fn split_addressee(t: &str) -> (&str, Option<&str>) {
    match t.split_once('/') {
        Some((n, w)) => (n, Some(w)),
        None => (t, None),
    }
}

/// A worker name is the free text of the status rows: letters, digits, dash, underscore, dot.
pub fn check_worker_name(w: &str) -> Result<(), String> {
    if w.is_empty()
        || w.len() > 64
        || !w
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "worker `{w}` must have 1 to 64 letters, digits, dash, underscore or dot"
        ));
    }
    Ok(())
}

pub fn topic_matches(pattern: &str, topic: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => topic.starts_with(prefix),
        None => topic == pattern,
    }
}

/// Splits `a,b c` into names.
pub fn parse_names(s: &str) -> Vec<String> {
    s.split([',', ' '])
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The compact one line form:
/// `#1234 20:05:11 alice READY issue/189 shaders/ssa-phi-placement 1a2b3c4 : phi count bounded`.
pub fn format_line(m: &Message) -> String {
    let mut s = format!(
        "#{} {} {} {}",
        m.id,
        crate::time::clock(&m.ts),
        m.from,
        m.kind.as_str()
    );
    if !m.to.is_empty() {
        s.push_str(" @");
        s.push_str(&m.to.join(","));
    }
    s.push(' ');
    s.push_str(if m.topic.is_empty() { "-" } else { &m.topic });
    for r in &m.refs {
        s.push(' ');
        s.push_str(r);
    }
    if !m.body.is_empty() {
        s.push_str(" : ");
        // The hub refuses control characters, this only guards against a hand edited log.
        s.extend(m.body.chars().map(|c| if c.is_control() { ' ' } else { c }));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Message {
        Message {
            id: 1234,
            ts: "2026-11-24T20:05:11.000Z".into(),
            from: "alice".into(),
            role: Role::Member,
            kind: Kind::Ready,
            topic: "issue/189".into(),
            body: "phi count bounded".into(),
            refs: vec!["shaders/ssa-phi-placement".into(), "1a2b3c4".into()],
            to: vec![],
        }
    }

    #[test]
    fn line_form() {
        assert_eq!(
            format_line(&sample()),
            "#1234 20:05:11 alice READY issue/189 shaders/ssa-phi-placement 1a2b3c4 : phi count bounded"
        );
        let mut m = sample();
        m.topic.clear();
        m.refs.clear();
        m.body.clear();
        assert_eq!(format_line(&m), "#1234 20:05:11 alice READY -");
        m.to = vec!["rex".into(), "ana/d3d".into()];
        assert_eq!(format_line(&m), "#1234 20:05:11 alice READY @rex,ana/d3d -");
    }

    #[test]
    fn wire_form_is_one_line_in_field_order() {
        let mut m = sample();
        m.kind = Kind::RunRequest;
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.starts_with(r#"{"id":1234,"ts":"#));
        assert!(j.contains(r#""kind":"RUN_REQUEST""#));
        assert!(!j.contains('\n'));
        assert!(!j.contains("\"to\""), "no addressee, no key: {j}");
        assert_eq!(serde_json::from_str::<Message>(&j).unwrap(), m);
        m.to = vec!["rex".into()];
        let j = serde_json::to_string(&m).unwrap();
        assert!(
            j.ends_with(r#","refs":["shaders/ssa-phi-placement","1a2b3c4"],"to":["rex"]}"#),
            "{j}"
        );
        assert_eq!(serde_json::from_str::<Message>(&j).unwrap(), m);
    }

    #[test]
    fn kinds_parse() {
        assert_eq!(Kind::parse("run-request"), Some(Kind::RunRequest));
        assert_eq!(Kind::parse("ready"), Some(Kind::Ready));
        assert_eq!(Kind::parse("nope"), None);
        assert_eq!(
            Kind::parse_list("READY, fix").unwrap(),
            vec![Kind::Ready, Kind::Fix]
        );
        assert!(Kind::parse_list("READY,bad").is_err());
        for k in Kind::ALL {
            let j = serde_json::to_string(&k).unwrap();
            assert_eq!(j, format!("\"{}\"", k.as_str()));
        }
    }

    #[test]
    fn validation() {
        let l = Limits::default();
        assert!(validate(&l, "issue/1", "ok", &[]).is_ok());
        assert!(validate(&l, "issue/1", "two\nlines", &[]).is_err());
        assert!(validate(&l, "has space", "x", &[]).is_err());
        assert!(validate(&l, "t", &"x".repeat(4097), &[]).is_err());
        assert!(validate(&l, "t", "x", &["".into()]).is_err());
        assert!(validate(&l, "t", "x", &vec!["r".to_string(); 17]).is_err());
    }

    #[test]
    fn kind_limits() {
        let l = Limits::default();
        assert!(check_kind_limit(&l, Kind::Accept, &"x".repeat(200)).is_ok());
        let e = check_kind_limit(&l, Kind::Decline, &"x".repeat(201)).unwrap_err();
        assert!(
            e.contains("201") && e.contains("200") && e.contains("commit, the issue or a file")
        );
        for (k, n) in [
            (Kind::Chat, 600),
            (Kind::Task, 600),
            (Kind::RunRequest, 600),
            (Kind::Claim, 600),
            (Kind::Release, 600),
            (Kind::Hello, 600),
            (Kind::Ready, 1200),
            (Kind::Fix, 1200),
            (Kind::Stuck, 1200),
            (Kind::RunResult, 1200),
            (Kind::Finding, 1200),
            (Kind::Landed, 1200),
        ] {
            assert!(check_kind_limit(&l, k, &"x".repeat(n)).is_ok(), "{k:?}");
            assert!(
                check_kind_limit(&l, k, &"x".repeat(n + 1)).is_err(),
                "{k:?}"
            );
        }
        // Characters, not bytes: 200 two byte characters are fine for ACCEPT.
        assert!(check_kind_limit(&l, Kind::Accept, &"\u{e9}".repeat(200)).is_ok());
        assert!(check_kind_limit(&l, Kind::Status, &"x".repeat(5000)).is_ok());
        assert_eq!(
            parse_kind_limits("ready=50, chat=7").unwrap(),
            vec![(Kind::Ready, 50), (Kind::Chat, 7)]
        );
        assert!(parse_kind_limits("ready").is_err());
        assert!(parse_kind_limits("nope=1").is_err());
        assert!(parse_kind_limits("ready=x").is_err());
    }

    #[test]
    fn worker_filter() {
        let mut m = sample();
        let plain = Filter {
            to: Some("rex".into()),
            ..Default::default()
        };
        let w = Filter {
            to: Some("rex".into()),
            worker: Some("d3d".into()),
            ..Default::default()
        };
        m.to = vec!["rex/d3d".into()];
        assert!(plain.matches(&m), "no worker filter sees every worker");
        assert!(w.matches(&m));
        m.to = vec!["rex/other".into()];
        assert!(plain.matches(&m));
        assert!(!w.matches(&m), "another worker of the same name");
        m.to = vec!["rex".into()];
        assert!(w.matches(&m), "the whole participant");
        m.to = vec![];
        assert!(w.matches(&m), "everyone");
        m.to = vec!["ana/d3d".into()];
        assert!(!w.matches(&m), "same worker name, other participant");
        m.to = vec!["ana/x".into(), "rex/d3d".into()];
        assert!(w.matches(&m));
        assert!(check_worker_name("d3d12-fixes_1.x").is_ok());
        assert!(check_worker_name("").is_err());
        assert!(check_worker_name("a b").is_err());
        assert!(check_worker_name("a/b").is_err());
    }

    #[test]
    fn filters() {
        let m = sample();
        assert!(Filter::default().matches(&m));
        let f = Filter {
            topic: Some("issue/*".into()),
            ..Default::default()
        };
        assert!(f.matches(&m));
        let f = Filter {
            topic: Some("issue/18".into()),
            ..Default::default()
        };
        assert!(!f.matches(&m));
        let f = Filter {
            kinds: vec![Kind::Fix],
            ..Default::default()
        };
        assert!(!f.matches(&m));
        let f = Filter {
            from: vec!["a".into(), "alice".into()],
            ..Default::default()
        };
        assert!(f.matches(&m));
        let me = Filter {
            to: Some("rex".into()),
            ..Default::default()
        };
        assert!(me.matches(&m), "for everyone");
        let mut m = m;
        m.to = vec!["ana".into()];
        assert!(!me.matches(&m), "for someone else");
        m.to.push("rex".into());
        assert!(me.matches(&m), "for rex among others");
    }
}
