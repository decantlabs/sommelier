//! The thirteen gates, one small pure function each. See `docs/factory.md` section 8.2.
//!
//! Gate names in findings: `base`, `history`, `authors`, `paths`, `scope`, `hold_paths`,
//! `entry_kinds`, `size`, `manifests`, `capability`, `tests`, `licence`, `secrets`, and `rules` for a
//! pattern in the rules that does not compile.

use super::diff::{ChangeKind, FileChange, FileMode, ParsedDiff};
use super::glob::any_match;
use super::{
    DeltaEntry, FeatureScope, Finding, GateRules, NamedPattern, OrderView, Risk, Severity,
};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

const SHOWN_CHARS: usize = 120;

/// True for the Unicode format characters that draw nothing and so hide text: zero width characters,
/// bidirectional controls, line and paragraph separators, the soft hyphen, the byte order mark and
/// the tag characters. `char::is_control` does not know them.
pub fn is_invisible(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x206F | 0xFEFF
        | 0xFFF9..=0xFFFB | 0x110BD | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        | 0xE0001 | 0xE0020..=0xE007F)
}

/// True for a control or an invisible character.
fn unprintable(c: char) -> bool {
    c.is_control() || is_invisible(c)
}

/// Shortens a hostile string for a finding and replaces control and invisible characters.
pub fn clip(s: &str) -> String {
    s.chars()
        .take(SHOWN_CHARS)
        .map(|c| if unprintable(c) { '?' } else { c })
        .collect()
}

/// Folds a path the way a case insensitive file system may: upper case, then lower case, so that
/// `ſ` and `ı` meet `s` and `i`.
pub fn fold_path(p: &str) -> String {
    p.to_uppercase().to_lowercase()
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

/// Cuts one gate's findings to `max` and adds a note that says how many were left out. For the gates
/// that only list (`capability`, `tests`) the first entries of every kind survive the cut, so padding
/// with one kind cannot hide another.
pub fn limit(list: Vec<Finding>, max: usize) -> Vec<Finding> {
    if list.len() <= max {
        return list;
    }
    let gate = list[0].gate.clone();
    let by_kind = gate == "capability" || gate == "tests";
    let total = list.len();
    let mut list: Vec<Finding> = if by_kind {
        let keep = super::keep_flags(&list, max, |f| f.text.clone());
        list.into_iter()
            .zip(keep)
            .filter_map(|(f, k)| k.then_some(f))
            .collect()
    } else {
        list.into_iter().take(max).collect()
    };
    let more = total - list.len();
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
    if s.chars().any(unprintable) {
        return Err("has a control or invisible character");
    }
    if s.chars().any(|c| c == '<' || c == '>') {
        return Err("has an angle bracket");
    }
    Ok(())
}

fn email_problem(email: &str, max: usize) -> Option<&'static str> {
    let mut problem = plain(email, max).err();
    if problem.is_none() {
        let parts: Vec<&str> = email.split('@').collect();
        if email.chars().any(char::is_whitespace) {
            problem = Some("has a space");
        } else if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            problem = Some("is not of the form name@host");
        }
    }
    problem
}

/// A commit message may hold line breaks and tabs, nothing else that is unprintable.
fn message_problem(s: &str, max: usize) -> Option<&'static str> {
    if s.chars().count() > max {
        return Some("is too long");
    }
    s.chars()
        .any(|c| unprintable(c) && !matches!(c, '\n' | '\r' | '\t'))
        .then_some("has a control or invisible character")
}

/// Gate 3: author and committer name and email are plain, the message holds no hidden characters.
/// Committer fields that are both empty belong to a record from before they were read and pass.
pub fn authors(diff: &ParsedDiff, rules: &GateRules) -> Vec<Finding> {
    let mut out = Vec::new();
    for (i, c) in diff.commits.iter().enumerate() {
        let n = i + 1;
        let mut bad = |what: &str, why: &str| {
            out.push(block(
                "authors",
                None,
                format!("the {what} of commit {n} {why}"),
            ));
        };
        if let Err(why) = plain(&c.author_name, rules.max_name_chars) {
            bad("author name", why);
        }
        if let Some(why) = email_problem(&c.author_email, rules.max_email_chars) {
            bad("author email", why);
        }
        if !(c.committer_name.is_empty() && c.committer_email.is_empty()) {
            if let Err(why) = plain(&c.committer_name, rules.max_name_chars) {
                bad("committer name", why);
            }
            if let Some(why) = email_problem(&c.committer_email, rules.max_email_chars) {
                bad("committer email", why);
            }
        }
        if let Some(why) = message_problem(&c.message, rules.max_message_chars) {
            bad("message", why);
        }
    }
    out
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Longest path in bytes that any gate looks at.
pub const MAX_PATH_BYTES: usize = 1024;
/// Most folders and the file name together in one path.
pub const MAX_PATH_DEPTH: usize = 32;

/// The path is too long or too deep. Linear in the length, so it is safe to run on any input.
fn shape_problem(p: &str) -> Option<&'static str> {
    if p.len() > MAX_PATH_BYTES {
        return Some("is longer than 1024 bytes");
    }
    if p.bytes().filter(|b| *b == b'/').count() >= MAX_PATH_DEPTH {
        return Some("is deeper than 32 levels");
    }
    None
}

/// Gate 4, first part: findings for every path of the diff that is too long or too deep. The caller
/// runs no other gate when this list is not empty, so no later gate does quadratic work on a deep path.
pub fn path_limits(diff: &ParsedDiff) -> Vec<Finding> {
    let mut out = Vec::new();
    let all = diff.files.iter().flat_map(paths_of).chain(
        diff.commits
            .iter()
            .flat_map(|c| c.paths.iter().map(String::as_str)),
    );
    for p in all {
        if let Some(why) = shape_problem(p) {
            out.push(block("paths", Some(&clip(p)), format!("the path {why}")));
        }
    }
    out
}

/// The first reason a path is not acceptable, or `None`.
pub fn path_problem(p: &str) -> Option<&'static str> {
    if let Some(why) = shape_problem(p) {
        return Some(why);
    }
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
    if p.chars().any(unprintable) {
        return Some("has a control or invisible character");
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

/// The characters a new path may hold: `A-Z a-z 0-9 . _ / -` and the space, plus the extra ones of the
/// rules. This keeps out `~` (the 8.3 short names of Windows such as `CARGO~1.TOM`) and every character
/// that a file system may fold into another.
fn charset_problem(p: &str, rules: &GateRules) -> bool {
    p.chars().any(|c| {
        !(c.is_ascii_alphanumeric()
            || matches!(c, '.' | '_' | '/' | '-' | ' ')
            || rules.extra_path_chars.contains(c))
    })
}

/// The paths that commits before the head touch and the end diff does not list, each once with the
/// number of the first commit that touches it.
fn commit_only_paths(diff: &ParsedDiff) -> Vec<(usize, &str)> {
    let end: BTreeSet<&str> = diff.files.iter().flat_map(paths_of).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::new();
    for (i, c) in diff.commits.iter().enumerate() {
        for p in &c.paths {
            if !end.contains(p.as_str()) && seen.insert(p.as_str()) {
                out.push((i + 1, p.as_str()));
            }
        }
    }
    out
}

/// Gate 4: paths are safe on every platform. Same as [`paths_with`] with the default rules.
pub fn paths(diff: &ParsedDiff) -> Vec<Finding> {
    paths_with(diff, &GateRules::default())
}

/// Gate 4: paths are safe on every platform, also the paths of the middle commits. A new path uses
/// the safe character set only, and it must not differ only in case from another changed path or from
/// a path of the base tree.
pub fn paths_with(diff: &ParsedDiff, rules: &GateRules) -> Vec<Finding> {
    let limits = path_limits(diff);
    if !limits.is_empty() {
        return limits;
    }
    let mut out = Vec::new();
    let base: BTreeSet<&str> = diff.base_paths.iter().map(String::as_str).collect();
    for f in &diff.files {
        for p in paths_of(f) {
            if let Some(why) = path_problem(p) {
                out.push(block("paths", Some(p), format!("the path {why}")));
            }
        }
        if matches!(f.change, ChangeKind::Added | ChangeKind::Renamed)
            && charset_problem(&f.path, rules)
        {
            out.push(block(
                "paths",
                Some(&f.path),
                "a new path may hold letters, digits, space and . _ - / only".into(),
            ));
        }
    }
    for (n, p) in commit_only_paths(diff) {
        if let Some(why) = path_problem(p) {
            out.push(block(
                "paths",
                Some(p),
                format!("the path in commit {n} {why}"),
            ));
        } else if !base.contains(p) && charset_problem(p, rules) {
            out.push(block(
                "paths",
                Some(p),
                format!(
                    "a new path in commit {n} may hold letters, digits, space and . _ - / only"
                ),
            ));
        }
    }
    out.extend(case_collisions(diff));
    out
}

/// New paths that differ only in case from another changed path or from a path of the base tree.
fn case_collisions(diff: &ParsedDiff) -> Vec<Finding> {
    let mut out = Vec::new();
    // Paths of the base tree that this diff removes may be reused in another case.
    let gone: BTreeSet<&str> = diff
        .files
        .iter()
        .filter(|f| matches!(f.change, ChangeKind::Deleted | ChangeKind::Renamed))
        .flat_map(|f| {
            let own = (f.change == ChangeKind::Deleted).then_some(f.path.as_str());
            own.into_iter().chain(f.old_path.as_deref())
        })
        .collect();
    // Every folded base path and every folded folder of it, with the spelling of the base.
    let mut base: BTreeMap<String, &str> = BTreeMap::new();
    for b in diff
        .base_paths
        .iter()
        .filter(|b| !gone.contains(b.as_str()))
    {
        if shape_problem(b).is_some() {
            continue;
        }
        for (folded, prefix) in folded_prefixes(b) {
            base.entry(folded).or_insert(prefix);
        }
    }
    let mut folded: BTreeMap<String, &str> = BTreeMap::new();
    for f in &diff.files {
        if f.change == ChangeKind::Deleted {
            continue;
        }
        let key = fold_path(&f.path);
        let new_here = matches!(f.change, ChangeKind::Added | ChangeKind::Renamed);
        let clash = new_here
            && folded_prefixes(&f.path)
                .iter()
                .any(|(fp, p)| base.get(fp).is_some_and(|b| b != p));
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
        if clash {
            out.push(block(
                "paths",
                Some(&f.path),
                "the path differs only in case from a path of the base tree".into(),
            ));
        }
    }
    out
}

/// The path and each of its folders, each with its folded form: `a/b/c` gives `a`, `a/b` and `a/b/c`.
/// The path is folded once. A folded prefix is cut from the folded path at the same `/`, because no
/// character folds into or out of a `/`. If the number of parts differs after folding, only the whole
/// path is listed.
fn folded_prefixes(path: &str) -> Vec<(String, &str)> {
    let folded = fold_path(path);
    let cuts: Vec<usize> = path.match_indices('/').map(|(i, _)| i).collect();
    let fcuts: Vec<usize> = folded.match_indices('/').map(|(i, _)| i).collect();
    let mut out = Vec::with_capacity(cuts.len() + 1);
    if cuts.len() == fcuts.len() {
        for (c, fc) in cuts.iter().zip(&fcuts) {
            out.push((folded[..*fc].to_string(), &path[..*c]));
        }
    }
    out.push((folded, path));
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
    for (n, p) in commit_only_paths(diff) {
        if !any_match(&order.scope, p, false) {
            out.push(block(
                "scope",
                Some(p),
                format!("commit {n} touches a path outside the scope of the order"),
            ));
        }
    }
    out
}

/// Gate `feature_scope`: the order names a feature. A changed path outside the paths of that feature
/// and inside the paths of another feature raises the risk and names the other feature. A changed path
/// outside every feature is a note. The scope gate stays the hard limit.
pub fn feature_scope(diff: &ParsedDiff, fs: &FeatureScope) -> Vec<Finding> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut paths: Vec<&str> = Vec::new();
    for f in &diff.files {
        for p in paths_of(f) {
            if seen.insert(p) {
                paths.push(p);
            }
        }
    }
    for (_, p) in commit_only_paths(diff) {
        if seen.insert(p) {
            paths.push(p);
        }
    }
    let mut out = Vec::new();
    for p in paths {
        if any_match(&fs.paths, p, false) {
            continue;
        }
        let owners: Vec<&str> = fs
            .others
            .iter()
            .filter(|(_, globs)| any_match(globs, p, false))
            .map(|(id, _)| id.as_str())
            .collect();
        if owners.is_empty() {
            out.push(finding(
                "feature_scope",
                Severity::Note,
                Some(p),
                None,
                "the path belongs to no feature of the map".into(),
            ));
        } else {
            let shown: Vec<String> = owners.iter().take(3).map(|o| clip(o)).collect();
            let more = owners.len().saturating_sub(3);
            let tail = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            out.push(finding(
                "feature_scope",
                Severity::Raise,
                Some(p),
                None,
                format!(
                    "the path is outside feature {} and inside feature {}{tail}",
                    clip(&fs.feature),
                    shown.join(", ")
                ),
            ));
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
    for (n, p) in commit_only_paths(diff) {
        if any_match(&rules.hold_paths, p, true) {
            out.push(block(
                "hold_paths",
                Some(p),
                format!("commit {n} touches a hold path, it needs a guarded order"),
            ));
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
    for (n, q) in commit_only_paths(diff) {
        if file_name(q).eq_ignore_ascii_case(".gitattributes") {
            out.push(block(
                "entry_kinds",
                Some(q),
                format!("commit {n} changes .gitattributes, which is not accepted"),
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
    for f in &diff.files {
        // A file that becomes executable runs without a build. The mode before the change is the proof.
        if f.mode == FileMode::Executable
            && f.old_mode != Some(FileMode::Executable)
            && f.change != ChangeKind::Deleted
        {
            out.push(DeltaEntry {
                kind: "executable_bit".into(),
                path: clip(&f.path),
                line: 0,
            });
        }
    }
    for f in text_files(diff) {
        // A pure rename has no added lines. For this gate the whole new content counts as added.
        let lines = if f.change == ChangeKind::Renamed && !f.content.is_empty() {
            &f.content
        } else {
            &f.added
        };
        for l in lines {
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

/// Gate 10: capability hits raise the risk. One hit or a hundred raise it ONE step in total, the
/// order's risk goes up once (`Low` to `Normal`, `Normal` to `Guarded`). Every hit stays listed.
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
    // Names, emails and messages of the commits. Email addresses are expected there.
    for (i, c) in diff.commits.iter().enumerate() {
        let parts = [
            ("author name", &c.author_name),
            ("author email", &c.author_email),
            ("committer name", &c.committer_name),
            ("committer email", &c.committer_email),
            ("message", &c.message),
        ];
        for (what, text) in parts {
            for (name, re) in pats.iter().filter(|(n, _)| n != "email_address") {
                if re.is_match(text) {
                    out.push(finding(
                        "secrets",
                        Severity::Block,
                        None,
                        None,
                        format!("the {what} of commit {} holds {name}, remove it", i + 1),
                    ));
                }
            }
        }
    }
    out
}
