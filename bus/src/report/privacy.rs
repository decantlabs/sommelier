//! Private data in report text: finding it and removing it.
//!
//! The tester side calls [`scrub`] before it sends a report. The hub side calls [`find_private`] to
//! check what arrives. Both functions are pure. They know paths with a user folder (Windows, Wine and Unix
//! forms, names with spaces), email addresses, token shaped strings, machine names in common log
//! forms, Windows SIDs, GUIDs, MAC addresses and IP addresses.

use std::sync::LazyLock;

use regex::Regex;

/// What kind of private data a [`Hit`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HitKind {
    /// A path that holds a user folder, such as `C:\Users\name\...` or `/home/name/...`.
    UserPath,
    /// An email address.
    Email,
    /// A key, token or password shaped string.
    Token,
    /// A machine, host or account name in a log form such as `hostname=box`.
    Machine,
}

impl HitKind {
    /// Short name for error lines.
    pub fn label(self) -> &'static str {
        match self {
            HitKind::UserPath => "a path with a user folder",
            HitKind::Email => "an email address",
            HitKind::Token => "a token shaped string",
            HitKind::Machine => "a machine or account name",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            HitKind::UserPath => "<path>",
            HitKind::Email => "<email>",
            HitKind::Token => "<token>",
            HitKind::Machine => "<machine>",
        }
    }
}

/// One private span of a text. `start` and `end` are byte offsets, `end` is exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub kind: HitKind,
    pub start: usize,
    pub end: usize,
}

struct Pattern {
    kind: HitKind,
    re: Regex,
    /// Which capture group is the private span. 0 is the whole match.
    group: usize,
    /// Extra test on the span, for patterns that would hit ordinary words.
    accept: fn(&str) -> bool,
}

fn any(_: &str) -> bool {
    true
}

fn has_digit(s: &str) -> bool {
    s.bytes().any(|b| b.is_ascii_digit())
}

fn mixed_case_and_digit(s: &str) -> bool {
    s.bytes().any(|b| b.is_ascii_lowercase())
        && s.bytes().any(|b| b.is_ascii_uppercase())
        && s.bytes().any(|b| b.is_ascii_digit())
}

fn routable_v4(s: &str) -> bool {
    !(s == "0.0.0.0" || s.starts_with("127."))
}

fn plausible_v6(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    if lower == "::1" || lower == "::" {
        return false;
    }
    s.matches(':').count() >= 3
        || lower.starts_with("fe80:")
        || lower.starts_with("fc")
        || lower.starts_with("fd")
        || (lower.starts_with('2') && lower.as_bytes().get(4) == Some(&b':'))
}

fn pattern(kind: HitKind, re: &str, group: usize, accept: fn(&str) -> bool) -> Pattern {
    Pattern {
        kind,
        re: Regex::new(re).expect("privacy pattern is valid"),
        group,
        accept,
    }
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    vec![
        // C:\Users\name\..., C:/Users/name, C:\Documents and Settings\name, and the Wine form
        // Z:\home\name. The user folder may hold spaces (`John Smith`), up to four words.
        pattern(
            HitKind::UserPath,
            r#"(?i)\b[a-z]:[\\/]+(?:users|documents and settings|home)[\\/]+(?:[^\\/\s"'<>|*?]+ ){0,3}[^\\/\s"'<>|*?]+(?:[\\/][^\s"'<>|*?]*)?"#,
            0,
            any,
        ),
        // A cloud folder named after an employer or an account: `OneDrive - Employer`.
        pattern(
            HitKind::UserPath,
            r#"(?i)\bonedrive\s+-\s+(?:[^\s\\/"'<>|*?]+ ){0,3}[^\s\\/"'<>|*?]+"#,
            0,
            any,
        ),
        // \home\name without a drive letter.
        pattern(
            HitKind::UserPath,
            r#"(?i)(?:^|[^A-Za-z0-9_.-])(\\home\\[^\s"'<>|:*?]+)"#,
            1,
            any,
        ),
        // /home/name, /Users/name, /mnt/c/Users/name, /c/Users/name
        pattern(
            HitKind::UserPath,
            r#"(?i)(?:^|[^A-Za-z0-9_.-])(/(?:home|users|(?:mnt/)?[a-z]/users)/[^\s"'<>|:*?]+)"#,
            1,
            any,
        ),
        pattern(
            HitKind::Email,
            r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+",
            0,
            any,
        ),
        // Well known token prefixes.
        pattern(
            HitKind::Token,
            r"\b(?:gh[pousr]_|github_pat_|sk-|pk-|xox[abprs]-|AKIA|AIza)[A-Za-z0-9_-]{16,}",
            0,
            any,
        ),
        pattern(
            HitKind::Token,
            r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]*",
            0,
            any,
        ),
        pattern(
            HitKind::Token,
            r"(?i)\bbearer\s+([A-Za-z0-9._~+/=-]{16,})",
            1,
            any,
        ),
        // token=..., password: ..., api_key=...
        pattern(
            HitKind::Token,
            r#"(?i)(?:token|secret|password|passwd|pwd|api[_-]?key|access[_-]?key|authorization)["']?\s*[=:]\s*["']?([A-Za-z0-9._~+/=-]{12,})"#,
            1,
            has_digit,
        ),
        // A long string of mixed case letters and digits.
        pattern(
            HitKind::Token,
            r"[A-Za-z0-9_-]{32,}",
            0,
            mixed_case_and_digit,
        ),
        pattern(
            HitKind::Machine,
            r"(?i)\b(?:desktop|laptop)-[a-z0-9]{5,}\b",
            0,
            any,
        ),
        pattern(
            HitKind::Machine,
            r#"(?i)\b(?:computername|hostname|machinename|machine|host|computer|node|username|user)\s*[=:]\s*([^\s,;"'<>]+)"#,
            1,
            any,
        ),
        // A Windows security identifier, the account behind a profile.
        pattern(HitKind::Machine, r"(?i)\bS-1-\d+(?:-\d+){2,}\b", 0, any),
        // A GUID: machine ids, adapter ids, profile ids.
        pattern(
            HitKind::Machine,
            r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b",
            0,
            any,
        ),
        // A MAC address.
        pattern(
            HitKind::Machine,
            r"(?i)\b(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}\b",
            0,
            any,
        ),
        // An IPv4 address. Loopback and the empty address are not private.
        pattern(
            HitKind::Machine,
            r"\b(?:(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\b",
            0,
            routable_v4,
        ),
        // An IPv6 address, full or with `::`. Short forms are left alone unless they look like a link
        // local, unique local or global address, so `std::vec` and times survive.
        pattern(
            HitKind::Machine,
            r"(?i)(?:^|[^0-9A-Za-z:])((?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}|(?:[0-9a-f]{1,4}:){1,6}:(?:[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){0,5})?|::(?:[0-9a-f]{1,4}:){0,5}[0-9a-f]{1,4})(?:[^0-9A-Za-z:]|$)",
            1,
            plausible_v6,
        ),
        // The server part of a UNC path.
        pattern(
            HitKind::Machine,
            r"(?:^|[^\\])(\\\\[A-Za-z0-9_-]{2,}\\)",
            1,
            any,
        ),
    ]
});

/// All private spans of `text`, ordered by position. Spans of different kinds may overlap.
pub fn find_private(text: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    for p in PATTERNS.iter() {
        for caps in p.re.captures_iter(text) {
            let Some(m) = caps.get(p.group) else {
                continue;
            };
            if m.start() == m.end() || !(p.accept)(m.as_str()) {
                continue;
            }
            hits.push(Hit {
                kind: p.kind,
                start: m.start(),
                end: m.end(),
            });
        }
    }
    hits.sort_by_key(|h| (h.start, h.end, h.kind));
    hits
}

/// `text` with every private span replaced by a placeholder such as `<path>`. Text without a hit
/// comes back unchanged. After `scrub`, [`find_private`] finds nothing.
pub fn scrub(text: &str) -> String {
    let mut cur = text.to_string();
    // A replacement can only shrink the text, so a few rounds always reach a fixed point.
    for _ in 0..4 {
        let hits = find_private(&cur);
        if hits.is_empty() {
            return cur;
        }
        let mut out = String::with_capacity(cur.len());
        let mut pos = 0;
        let mut i = 0;
        while i < hits.len() {
            let first = hits[i];
            let mut end = first.end;
            i += 1;
            while i < hits.len() && hits[i].start < end {
                end = end.max(hits[i].end);
                i += 1;
            }
            out.push_str(&cur[pos..first.start]);
            out.push_str(first.kind.placeholder());
            pos = end;
        }
        out.push_str(&cur[pos..]);
        cur = out;
    }
    // Last resort: nothing readable is better than a leak.
    if find_private(&cur).is_empty() {
        cur
    } else {
        "<scrubbed>".to_string()
    }
}
