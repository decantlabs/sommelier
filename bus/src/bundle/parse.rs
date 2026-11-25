//! Pure parsers for the bundle header and for git output. No file access beyond the header reader.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use super::BundleError;
use crate::gate::diff::{ChangeKind, CommitInfo, FileMode, Line};

/// The header of a bundle: prerequisites and refs.
#[derive(Debug, PartialEq, Eq)]
pub struct Header {
    pub prerequisites: Vec<String>,
    /// Pairs of object id and ref name.
    pub refs: Vec<(String, String)>,
}

const MAX_HEADER_LINES: usize = 1024;
const MAX_LINE: u64 = 4096;

/// True for a lower case hex object id of 40 or 64 digits.
pub fn is_object_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn plain_ref(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/@+=-".contains(&b))
}

/// Read the text header of a bundle file. Reads at most a few kilobytes and never the pack.
pub fn read_header(path: &Path) -> Result<Header, BundleError> {
    let f =
        File::open(path).map_err(|e| BundleError::Io(format!("cannot open the bundle: {e}")))?;
    let mut r = BufReader::new(f);
    let mut first = Vec::new();
    r.by_ref()
        .take(MAX_LINE)
        .read_until(b'\n', &mut first)
        .map_err(|e| BundleError::Io(e.to_string()))?;
    let first = String::from_utf8_lossy(&first);
    if first.trim_end() != "# v2 git bundle" && first.trim_end() != "# v3 git bundle" {
        return Err(BundleError::NotABundle(
            "the first line is not a git bundle signature".into(),
        ));
    }
    let mut h = Header {
        prerequisites: vec![],
        refs: vec![],
    };
    for _ in 0..MAX_HEADER_LINES {
        let mut raw = Vec::new();
        let n = r
            .by_ref()
            .take(MAX_LINE)
            .read_until(b'\n', &mut raw)
            .map_err(|e| BundleError::Io(e.to_string()))?;
        if n == 0 || !raw.ends_with(b"\n") {
            return Err(BundleError::NotABundle(
                "the bundle header ends too early".into(),
            ));
        }
        let line = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
        if line.is_empty() {
            return Ok(h);
        }
        if let Some(cap) = line.strip_prefix('@') {
            if cap == "object-format=sha1" || cap == "object-format=sha256" {
                continue;
            }
            return Err(BundleError::NotABundle(format!(
                "unsupported bundle capability {cap:?}"
            )));
        }
        if let Some(rest) = line.strip_prefix('-') {
            let id = rest.split(' ').next().unwrap_or("");
            if !is_object_id(id) {
                return Err(BundleError::NotABundle(
                    "a prerequisite is not an object id".into(),
                ));
            }
            h.prerequisites.push(id.to_string());
            continue;
        }
        let Some((id, name)) = line.split_once(' ') else {
            return Err(BundleError::NotABundle("a ref line has no name".into()));
        };
        if !is_object_id(id) {
            return Err(BundleError::NotABundle(
                "a ref line has no object id".into(),
            ));
        }
        if !plain_ref(name) {
            return Err(BundleError::NotABundle(format!(
                "the ref name {name:?} has unusual characters"
            )));
        }
        h.refs.push((id.to_string(), name.to_string()));
    }
    Err(BundleError::NotABundle(
        "the bundle header is too long".into(),
    ))
}

/// One entry of `git diff --raw -z`.
#[derive(Debug, PartialEq, Eq)]
pub struct RawEntry {
    pub old_mode: String,
    pub new_mode: String,
    pub new_sha: String,
    pub status: char,
    pub path: String,
    pub old_path: Option<String>,
}

fn bad(what: &str) -> BundleError {
    BundleError::Git {
        step: "parse".into(),
        message: format!("unexpected git output: {what}"),
    }
}

/// A path as git prints it. Backslashes stay: a file named `src\evil.rs` is one file name on Linux and
/// the path gate blocks it. Rewriting it to `src/evil.rs` would let it pass a scope of `src/**`.
fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Parse `git diff-tree --name-only -z` output into the list of paths.
pub fn parse_paths(out: &[u8]) -> Vec<String> {
    out.split(|b| *b == 0)
        .filter(|t| !t.is_empty())
        .map(text)
        .collect()
}

/// Parse `git diff --raw -z` output.
pub fn parse_raw(out: &[u8]) -> Result<Vec<RawEntry>, BundleError> {
    let mut toks = out.split(|b| *b == 0).filter(|t| !t.is_empty());
    let mut v = Vec::new();
    while let Some(meta) = toks.next() {
        let meta = String::from_utf8_lossy(meta);
        let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if f.len() != 5 {
            return Err(bad("raw entry"));
        }
        let status = f[4].chars().next().ok_or_else(|| bad("status"))?;
        let first = toks.next().ok_or_else(|| bad("missing path"))?;
        let (path, old_path) = if status == 'R' || status == 'C' {
            let second = toks.next().ok_or_else(|| bad("missing second path"))?;
            (text(second), Some(text(first)))
        } else {
            (text(first), None)
        };
        v.push(RawEntry {
            old_mode: f[0].into(),
            new_mode: f[1].into(),
            new_sha: f[3].into(),
            status,
            path,
            old_path,
        });
    }
    Ok(v)
}

/// The change kind of a raw status letter.
pub fn change_kind(status: char) -> ChangeKind {
    match status {
        'A' => ChangeKind::Added,
        'D' => ChangeKind::Deleted,
        'R' | 'C' => ChangeKind::Renamed,
        _ => ChangeKind::Modified,
    }
}

/// The tree entry kind before a change, `None` for the all zero mode of a new path.
pub fn old_file_mode(mode: &str) -> Option<FileMode> {
    (u32::from_str_radix(mode, 8).unwrap_or(0) != 0).then(|| file_mode(mode))
}

/// The tree entry kind of an octal mode string.
pub fn file_mode(mode: &str) -> FileMode {
    match u32::from_str_radix(mode, 8).unwrap_or(0o100644) & 0o170000 {
        0o120000 => FileMode::Symlink,
        0o160000 => FileMode::Submodule,
        _ if u32::from_str_radix(mode, 8).unwrap_or(0) & 0o111 != 0 => FileMode::Executable,
        _ => FileMode::Regular,
    }
}

/// One file section of a zero context patch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Section {
    pub binary: bool,
    pub added: Vec<Line>,
    pub removed: Vec<Line>,
}

fn hunk_start(h: &str) -> Option<(u32, u32)> {
    // "@@ -a[,b] +c[,d] @@ ..."
    let rest = h.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(' ')?;
    let new = rest.strip_prefix('+')?.split(' ').next()?;
    let num = |s: &str| s.split(',').next()?.parse::<u32>().ok();
    Some((num(old)?, num(new)?))
}

/// Split a zero context patch into one [`Section`] per file. Paths in the headers are ignored on purpose:
/// the caller pairs sections with the raw entries by position.
pub fn parse_patch(out: &[u8]) -> Vec<Section> {
    let mut secs: Vec<Section> = Vec::new();
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let mut in_hunk = false;
    for line in out.split(|b| *b == b'\n') {
        if line.starts_with(b"diff --git ") {
            secs.push(Section::default());
            in_hunk = false;
            continue;
        }
        let Some(sec) = secs.last_mut() else { continue };
        if !in_hunk {
            if line.starts_with(b"Binary files ") {
                sec.binary = true;
            } else if line.starts_with(b"@@ ") {
                in_hunk = true;
            }
        }
        if line.starts_with(b"@@ ") {
            if let Some((a, c)) = hunk_start(&String::from_utf8_lossy(line)) {
                old_no = a;
                new_no = c;
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        match line.first() {
            Some(b'+') => {
                sec.added.push(Line {
                    no: new_no,
                    text: String::from_utf8_lossy(&line[1..]).into_owned(),
                });
                new_no += 1;
            }
            Some(b'-') => {
                sec.removed.push(Line {
                    no: old_no,
                    text: String::from_utf8_lossy(&line[1..]).into_owned(),
                });
                old_no += 1;
            }
            _ => {}
        }
    }
    secs
}

/// Number of NUL separated fields per commit in the log format of the bundle module.
pub const LOG_FIELDS: usize = 7;

/// The `--format` of the commit log: id, parents, author name and email, committer name and email, message.
pub const LOG_FORMAT: &str = "--format=%H%x00%P%x00%an%x00%ae%x00%cn%x00%ce%x00%B%x00";

/// Parse the commit log written with fields separated by NUL, [`LOG_FIELDS`] fields per commit. The
/// `paths` of each commit stay empty, the caller fills them.
pub fn parse_log(out: &[u8]) -> Result<Vec<CommitInfo>, BundleError> {
    let mut toks: Vec<&[u8]> = out.split(|b| *b == 0).collect();
    if toks
        .last()
        .is_some_and(|t| t.iter().all(u8::is_ascii_whitespace))
    {
        toks.pop();
    }
    if !toks.len().is_multiple_of(LOG_FIELDS) {
        return Err(bad("commit log"));
    }
    Ok(toks
        .chunks(LOG_FIELDS)
        .map(|c| {
            let s = |b: &[u8]| {
                String::from_utf8_lossy(b)
                    .trim_start_matches('\n')
                    .to_string()
            };
            CommitInfo {
                sha: s(c[0]),
                parents: s(c[1]).split_whitespace().map(str::to_string).collect(),
                author_name: s(c[2]),
                author_email: s(c[3]),
                committer_name: s(c[4]),
                committer_email: s(c[5]),
                message: s(c[6]).trim_end_matches('\n').to_string(),
                paths: Vec::new(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_numbers_follow_the_new_and_old_side() {
        let p = b"diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -3,2 +3,1 @@\n-a\n-b\n+c\n@@ -9,0 +9,2 @@\n+d\n+e\n\\ No newline at end of file\n";
        let s = parse_patch(p);
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].removed.iter().map(|l| l.no).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(
            s[0].added.iter().map(|l| l.no).collect::<Vec<_>>(),
            vec![3, 9, 10]
        );
    }

    #[test]
    fn content_that_looks_like_a_header_stays_content() {
        let p = b"diff --git a/x b/x\n@@ -0,0 +1,2 @@\n+diff --git a/y b/y\n+Binary files differ\n";
        let s = parse_patch(p);
        assert_eq!(s.len(), 1);
        assert!(!s[0].binary);
        assert_eq!(s[0].added.len(), 2);
    }

    #[test]
    fn modes_map_to_kinds() {
        assert_eq!(file_mode("100644"), FileMode::Regular);
        assert_eq!(file_mode("100755"), FileMode::Executable);
        assert_eq!(file_mode("120000"), FileMode::Symlink);
        assert_eq!(file_mode("160000"), FileMode::Submodule);
    }
}
