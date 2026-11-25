//! The thirteen gates, one small pure function each. See `docs/factory.md` section 8.2.
//!
//! Gate names in findings: `base`, `history`, `authors`, `paths`, `scope`, `hold_paths`,
//! `entry_kinds`, `size`, `manifests`, `capability`, `tests`, `licence`, `secrets`, and `rules` for a
//! pattern in the rules that does not compile.

use super::diff::{ChangeKind, FileChange, FileMode, ParsedDiff};
use super::glob::any_match;
use super::{DeltaEntry, Finding, GateRules, NamedPattern, OrderView, Risk, Severity};
use regex::Regex;
use std::collections::BTreeMap;

const SHOWN_CHARS: usize = 120;

/// Shortens a hostile string for a finding and replaces control characters.
fn clip(s: &str) -> String {
    s.chars()
        .take(SHOWN_CHARS)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn finding(
    gate: &str,
    severity: Severity,
    path: Option<&str>,
    line: Option<u32>,
    text: String,
) -> Finding {
    Finding {
        gate: gate.into(),
        severity,
        path: path.map(clip),
        line,
        text,
    }
}

fn block(gate: &str, path: Option<&str>, text: String) -> Finding {
    finding(gate, Severity::Block, path, None, text)
}

/// Cuts one gate's findings to `max` and adds a note that says how many were left out.
pub fn limit(mut list: Vec<Finding>, max: usize) -> Vec<Finding> {
    if list.len() <= max {
        return list;
    }
    let gate = list[0].gate.clone();
    let more = list.len() - max;
    list.truncate(max);
    list.push(finding(
        &gate,
        Severity::Note,
        None,
        None,
        format!("{more} more findings of this gate are left out"),
    ));
    list
}

/// Compiles patterns. A pattern that does not compile becomes a blocking `rules` finding.
fn compile(patterns: &[NamedPattern], out: &mut Vec<Finding>) -> Vec<(String, Regex)> {
    let mut ok = Vec::new();
    for p in patterns {
        match Regex::new(&p.pattern) {
            Ok(re) => ok.push((p.name.clone(), re)),
            Err(_) => out.push(block(
                "rules",
                None,
                format!(
                    "pattern {} does not compile, fix the gate rules",
                    clip(&p.name)
                ),
            )),
        }
    }
    ok
}

fn text_files(diff: &ParsedDiff) -> impl Iterator<Item = &FileChange> {
    diff.files.iter().filter(|f| !f.binary)
}

fn paths_of(f: &FileChange) -> impl Iterator<Item = &str> {
    std::iter::once(f.path.as_str()).chain(f.old_path.as_deref())
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Checks that every pattern of the rules compiles. A bad pattern blocks, so a broken rule never
/// lets a patch through by silence.
pub fn rules_problems(rules: &GateRules) -> Vec<Finding> {
    let mut out = Vec::new();
    for list in [
        &rules.capabilities,
        &rules.removed_test_patterns,
        &rules.added_test_patterns,
        &rules.licence_markers,
        &rules.secrets,
    ] {
        compile(list, &mut out);
    }
    out
}

/// Gate 1: the diff is based on the commit of the lease.
pub fn base(diff: &ParsedDiff, order: &OrderView) -> Vec<Finding> {
    if diff.base == order.base {
        return Vec::new();
    }
    vec![block(
        "base",
        None,
        "the submission is not based on the commit of the lease, rebase on the lease base".into(),
    )]
}

/// Gate 2: every commit has one parent and the commits form one chain from base to head.
pub fn history(diff: &ParsedDiff) -> Vec<Finding> {
    let mut out = Vec::new();
    if diff.commits.is_empty() {
        out.push(block(
            "history",
            None,
            "the submission has no commits".into(),
        ));
        return out;
    }
    let mut expected = diff.base.as_str();
    for (i, c) in diff.commits.iter().enumerate() {
        if c.parents.len() != 1 {
            out.push(block(
                "history",
                None,
                format!(
                    "commit {} has {} parents, send a linear history",
                    i + 1,
                    c.parents.len()
                ),
            ));
        } else if c.parents[0] != expected {
            out.push(block(
                "history",
                None,
                format!(
                    "commit {} does not follow the one before it, send one chain from the base",
                    i + 1
                ),
            ));
        }
        expected = c.sha.as_str();
    }
    if expected != diff.head {
        out.push(block(
            "history",
            None,
            "the last commit is not the head of the submission".into(),
        ));
    }
    out
}

fn plain(s: &str, max: usize) -> Result<(), &'static str> {
    if s.trim().is_empty() {
        return Err("is empty");
    }
    if s.chars().count() > max {
        return Err("is too long");
    }
    if s.chars().any(|c| c.is_control()) {
        return Err("has a control character");
    }
    if s.chars().any(|c| c == '<' || c == '>') {
        return Err("has an angle bracket");
    }
    Ok(())
}

/// Gate 3: author name and email are plain.
pub fn authors(diff: &ParsedDiff, rules: &GateRules) -> Vec<Finding> {
    let mut out = Vec::new();
    for (i, c) in diff.commits.iter().enumerate() {
        if let Err(why) = plain(&c.author_name, rules.max_name_chars) {
            out.push(block(
                "authors",
                None,
                format!("the author name of commit {} {why}", i + 1),
            ));
        }
        let email = c.author_email.as_str();
        let mut problem = plain(email, rules.max_email_chars).err();
        if problem.is_none() {
            let parts: Vec<&str> = email.split('@').collect();
            if email.chars().any(char::is_whitespace) {
                problem = Some("has a space");
            } else if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
                problem = Some("is not of the form name@host");
            }
        }
        if let Some(why) = problem {
            out.push(block(
                "authors",
                None,
                format!("the author email of commit {} {why}", i + 1),
            ));
        }
    }
    out
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The first reason a path is not acceptable, or `None`.
pub fn path_problem(p: &str) -> Option<&'static str> {
    if p.is_empty() {
        return Some("is empty");
    }
    if p.starts_with('/') {
        return Some("is absolute");
    }
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Some("is absolute");
    }
    if p.contains('\\') {
        return Some("has a backslash");
    }
    if p.chars().any(|c| c.is_control()) {
        return Some("has a control character");
    }
    for comp in p.split('/') {
        if comp.is_empty() {
            return Some("has an empty part");
        }
        if comp == ".." {
            return Some("climbs out with ..");
        }
        if comp == "." {
            return Some("has a . part");
        }
        if comp.eq_ignore_ascii_case(".git") {
            return Some("is inside the .git folder");
        }
        if comp.ends_with('.') || comp.ends_with(' ') {
            return Some("has a part that ends with a dot or a space");
        }
        if comp
            .chars()
            .any(|c| matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
        {
            return Some("has a character that Windows does not accept");
        }
        let stem = comp.split('.').next().unwrap_or(comp).to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            return Some("has a name that Windows reserves");
        }
    }
    None
}

/// Gate 4: paths are safe on every platform.
pub fn paths(diff: &ParsedDiff) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut folded: BTreeMap<String, &str> = BTreeMap::new();
    for f in &diff.files {
        for p in paths_of(f) {
            if let Some(why) = path_problem(p) {
                out.push(block("paths", Some(p), format!("the path {why}")));
            }
        }
        if f.change == ChangeKind::Deleted {
            continue;
        }
        let key = f.path.to_lowercase();
        match folded.get(&key) {
            Some(first) if *first != f.path => out.push(block(
                "paths",
                Some(&f.path),
                "the path differs only in case from another changed path".into(),
            )),
            Some(_) => {}
            None => {
                folded.insert(key, f.path.as_str());
            }
        }
    }
    out
}

/// Gate 5: every changed path, old and new, matches a scope glob.
pub fn scope(diff: &ParsedDiff, order: &OrderView) -> Vec<Finding> {
    let mut out = Vec::new();
    for f in &diff.files {
        for p in paths_of(f) {
            if !any_match(&order.scope, p, false) {
                out.push(block(
                    "scope",
                    Some(p),
                    "the path is outside the scope of the order".into(),
                ));
            }
        }
    }
    out
}

/// Gate 6: a hold path needs a guarded order.
pub fn hold_paths(diff: &ParsedDiff, order: &OrderView, rules: &GateRules) -> Vec<Finding> {
    let mut out = Vec::new();
    if order.risk == Risk::Guarded {
        return out;
    }
    for f in &diff.files {
        for p in paths_of(f) {
            if any_match(&rules.hold_paths, p, true) {
                out.push(block(
                    "hold_paths",
                    Some(p),
                    "the path is a hold path, it needs a guarded order".into(),
                ));
            }
        }
    }
    out
}

/// Gate 7: no binary file, symlink, submodule or change of `.gitattributes`.
pub fn entry_kinds(diff: &ParsedDiff) -> Vec<Finding> {
    let mut out = Vec::new();
    for f in &diff.files {
        let p = Some(f.path.as_str());
        if f.binary {
            out.push(block(
                "entry_kinds",
                p,
                "binary files are not accepted".into(),
            ));
        }
        match f.mode {
            FileMode::Symlink => {
                out.push(block("entry_kinds", p, "symlinks are not accepted".into()))
            }
            FileMode::Submodule => out.push(block(
                "entry_kinds",
                p,
                "submodules are not accepted".into(),
            )),
            FileMode::Regular | FileMode::Executable => {}
        }
        if paths_of(f).any(|q| file_name(q).eq_ignore_ascii_case(".gitattributes")) {
            out.push(block(
                "entry_kinds",
                p,
                "a change of .gitattributes is not accepted".into(),
            ));
        }
    }
    out
}

/// Gate 8: the caps of the author's tier.
pub fn size(diff: &ParsedDiff, order: &OrderView, rules: &GateRules) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(caps) = rules
        .caps
        .get(usize::from(order.tier))
        .or_else(|| rules.caps.last())
    else {
        return out;
    };
    let mut over = |what: &str, got: u64, cap: u64| {
        if got > cap {
            out.push(block(
                "size",
                None,
                format!(
                    "{what} is {got}, the cap for tier {} is {cap}, split the work",
                    order.tier
                ),
            ));
        }
    };
    over(
        "the bundle size in bytes",
        diff.bundle_bytes,
        caps.bundle_bytes,
    );
    let lines: u64 = diff
        .files
        .iter()
        .map(|f| (f.added.len() as u64).saturating_add(f.removed.len() as u64))
        .fold(0, u64::saturating_add);
    over("the number of changed lines", lines, caps.changed_lines);
    over(
        "the number of changed files",
        diff.files.len() as u64,
        caps.files,
    );
    let largest = diff.files.iter().map(|f| f.size_after).max().unwrap_or(0);
    over("the largest file in bytes", largest, caps.largest_file);
    out
}

/// Gate 9: a changed manifest or lock file makes the entry guarded.
pub fn manifests(diff: &ParsedDiff, order: &OrderView, rules: &GateRules) -> Vec<Finding> {
    let mut out = Vec::new();
    let severity = if order.risk == Risk::Guarded {
        Severity::Raise
    } else {
        Severity::Block
    };
    for f in &diff.files {
        if let Some(p) = paths_of(f).find(|p| any_match(&rules.manifests, p, true)) {
            let text = if severity == Severity::Block {
                "a manifest or lock file changed, the order must be guarded"
            } else {
                "a manifest or lock file changed"
            };
            out.push(finding("manifests", severity, Some(p), None, text.into()));
        }
    }
    out
}

/// The hits of gate 10: new capabilities on added lines.
pub fn capability_hits(diff: &ParsedDiff, rules: &GateRules) -> Vec<DeltaEntry> {
    let mut sink = Vec::new();
    let pats = compile(&rules.capabilities, &mut sink);
    let mut out = Vec::new();
    for f in &diff.files {
        if file_name(&f.path) == "build.rs" && f.change != ChangeKind::Deleted {
            out.push(DeltaEntry {
                kind: "build_script".into(),
                path: clip(&f.path),
                line: f.added.first().map_or(0, |l| l.no),
            });
        }
    }
    for f in text_files(diff) {
        for l in &f.added {
            for (name, re) in &pats {
                if re.is_match(&l.text) {
                    out.push(DeltaEntry {
                        kind: name.clone(),
                        path: clip(&f.path),
                        line: l.no,
                    });
                }
            }
        }
    }
    out
}

/// Gate 10: every capability hit raises the risk one step.
pub fn capability(hits: &[DeltaEntry]) -> Vec<Finding> {
    hits.iter()
        .map(|h| {
            finding(
                "capability",
                Severity::Raise,
                Some(&h.path),
                (h.line > 0).then_some(h.line),
                format!("new use of {}", h.kind),
            )
        })
        .collect()
}

/// The hits of gate 11: removed or weakened tests.
pub fn test_hits(diff: &ParsedDiff, rules: &GateRules) -> Vec<DeltaEntry> {
    let mut sink = Vec::new();
    let removed = compile(&rules.removed_test_patterns, &mut sink);
    let added = compile(&rules.added_test_patterns, &mut sink);
    let mut out = Vec::new();
    for f in &diff.files {
        if f.change == ChangeKind::Deleted && any_match(&rules.test_files, &f.path, true) {
            out.push(DeltaEntry {
                kind: "deleted_test_file".into(),
                path: clip(&f.path),
                line: 0,
            });
            continue;
        }
        if f.binary {
            continue;
        }
        for l in &f.removed {
            if let Some((name, _)) = removed.iter().find(|(_, re)| re.is_match(&l.text)) {
                out.push(DeltaEntry {
                    kind: format!("removed_{name}"),
                    path: clip(&f.path),
                    line: l.no,
                });
            }
        }
        for l in &f.added {
            if let Some((name, _)) = added.iter().find(|(_, re)| re.is_match(&l.text)) {
                out.push(DeltaEntry {
                    kind: format!("added_{name}"),
                    path: clip(&f.path),
                    line: l.no,
                });
            }
        }
    }
    out
}

/// Gate 11: lists the test delta for the reviewer.
pub fn tests(hits: &[DeltaEntry]) -> Vec<Finding> {
    hits.iter()
        .map(|h| {
            finding(
                "tests",
                Severity::Note,
                Some(&h.path),
                (h.line > 0).then_some(h.line),
                format!("test change: {}", h.kind),
            )
        })
        .collect()
}

/// The identifiers of an SPDX expression, without the operators and exception names.
fn spdx_ids(expr: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut after_with = false;
    for tok in expr.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))) {
        if tok.is_empty() {
            continue;
        }
        match tok {
            "OR" | "AND" => after_with = false,
            "WITH" => after_with = true,
            _ if after_with => after_with = false,
            _ => ids.push(tok.to_string()),
        }
    }
    ids
}

/// Gate 12: licence markers on added lines.
pub fn licence(diff: &ParsedDiff, rules: &GateRules) -> Vec<Finding> {
    let mut out = Vec::new();
    let pats = compile(&rules.licence_markers, &mut Vec::new());
    let spdx = Regex::new(r"SPDX-License-Identifier:(.*)$").ok();
    for f in text_files(diff) {
        for l in &f.added {
            let hit = pats.iter().find(|(_, re)| re.is_match(&l.text));
            let text = if let Some((name, _)) = hit {
                Some(format!("licence marker {name}"))
            } else {
                spdx.as_ref()
                    .and_then(|re| re.captures(&l.text))
                    .and_then(|c| c.get(1))
                    .filter(|m| {
                        spdx_ids(m.as_str()).iter().any(|id| {
                            !rules
                                .allowed_spdx
                                .iter()
                                .any(|a| a.eq_ignore_ascii_case(id))
                        })
                    })
                    .map(|_| "SPDX line with a licence that is not the project's".to_string())
            };
            if let Some(text) = text {
                out.push(finding(
                    "licence",
                    Severity::Block,
                    Some(&f.path),
                    Some(l.no),
                    text,
                ));
            }
        }
    }
    out
}

/// Gate 13: secrets and personal data on added lines. The finding names the kind, never the text.
pub fn secrets(diff: &ParsedDiff, rules: &GateRules) -> Vec<Finding> {
    let mut out = Vec::new();
    let pats = compile(&rules.secrets, &mut Vec::new());
    for f in text_files(diff) {
        for l in &f.added {
            for (name, re) in &pats {
                let hit = re.find_iter(&l.text).any(|m| {
                    if name != "email_address" {
                        return true;
                    }
                    let found = m.as_str().to_lowercase();
                    !rules
                        .email_allow
                        .iter()
                        .any(|a| found.ends_with(&a.to_lowercase()))
                });
                if hit {
                    out.push(finding(
                        "secrets",
                        Severity::Block,
                        Some(&f.path),
                        Some(l.no),
                        format!("added line holds {name}, remove it"),
                    ));
                }
            }
        }
    }
    out
}
