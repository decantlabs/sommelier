//! Submissions as git bundles: packing on the member side, checking and safe unpacking on the hub side.
//!
//! Part of the public factory, see `docs/factory.md` sections 8.1 and 8.2. Unpacking produces a
//! [`crate::gate::diff::ParsedDiff`] and never checks out, never builds and never runs anything from the
//! bundle. The bundle is hostile input: it is size checked before git sees it, its header is read by our
//! own parser, and every git call runs with hooks, external diff, textconv and all config files off.
//!
//! What is diffed. The [`ParsedDiff`] holds the diff of the two END trees, base and head. The commits
//! between them are not diffed one by one. Instead each commit carries the list of paths it changes, and
//! the gates read those lists too. This is enough because of a decision of the project: a submission
//! lands as ONE squashed commit made from the gated end tree, so a middle commit never reaches main.
//! Commit messages, author and committer fields are read and gated like any other text.
//!
//! Limits and what they cover. Every limit is checked as early as a program without `unsafe` can check
//! it. The object sizes of the bundle are listed and checked BEFORE any diff runs (one object over
//! `max_blob_bytes`, too many objects or too many bytes in total refuse the submission). Rename
//! detection is bounded and switched off for a large file count. Git runs with bounded caches and one
//! thread, with a memory cap on Unix, with a time limit, an output cap and a disk cap for its folder.
//! A limit that is hit ends the whole process tree, see [`runner`]. Not covered: the unpacking of a
//! pack bomb itself runs before any size is known, so it is bounded by the disk cap, the memory cap and
//! the time limit rather than refused up front. On Windows there is no memory cap and the tree kill
//! follows parent links only. The hub server of the project is Linux.
//!
//! The memory cap is per process. `fetch` and the `index-pack` it starts each get the cap, and two gate
//! runs may go at once, so the sum can reach several times the cap. The real bound for the whole
//! process group of the hub is a service limit of the operating system (a cgroup limit such as
//! `MemoryMax` in the unit of the hub). Run the hub under one. Objects that no ref reaches are not
//! listed by `rev-list`, so after the fetch every object of the sandbox is compared with the ones the
//! range reaches, and a pack that holds any other object is refused (an object the reference
//! repository holds already is fine, a thin pack brings those as delta bases).

pub mod parse;
pub mod runner;

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::gate::diff::{FileChange, FileMode, Line, ParsedDiff};
use parse::{Header, is_object_id};
use runner::{
    Run, RunError, RunLimits, diff_flags, git_command, git_command_capped, run_guarded, run_limited,
};

/// Hard limits of one inspection.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_bundle_bytes: u64,
    pub max_commits: usize,
    pub max_files: usize,
    /// Largest new blob in bytes.
    pub max_blob_bytes: u64,
    /// Total size of the patch text git may produce.
    pub max_diff_bytes: usize,
    /// Seconds per git call.
    pub git_seconds: u64,
    /// Most objects (commits, trees, blobs) the submission may bring.
    pub max_objects: usize,
    /// Total size of all new objects in bytes, before compression.
    pub max_total_object_bytes: u64,
    /// Bytes the temporary folder of one inspection may hold while git works.
    pub max_sandbox_bytes: u64,
    /// Memory cap per git call in megabytes, Unix only, 0 for none. No object is over `max_blob_bytes`
    /// (2 MiB), so 768 is ample. The cap is per process (`ulimit -v`), see the module docs.
    pub max_memory_mb: u64,
    /// Rename detection runs only up to this many changed files.
    pub rename_max_files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bundle_bytes: 20 * 1024 * 1024,
            max_commits: 200,
            max_files: 500,
            max_blob_bytes: 2 * 1024 * 1024,
            max_diff_bytes: 8 * 1024 * 1024,
            git_seconds: 60,
            max_objects: 50_000,
            max_total_object_bytes: 64 * 1024 * 1024,
            max_sandbox_bytes: 256 * 1024 * 1024,
            max_memory_mb: 768,
            rename_max_files: 200,
        }
    }
}

/// Facts about a packed bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleInfo {
    /// Lower case hex SHA 256 of the file.
    pub sha256: String,
    pub bytes: u64,
    pub base: String,
    pub head: String,
}

/// Why a bundle was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleError {
    TooLarge {
        bytes: u64,
        max: u64,
    },
    NotABundle(String),
    WrongBase {
        expected: String,
        found: String,
    },
    RefCount(usize),
    /// The base is not an object of the reference repository.
    UnknownBase(String),
    /// The header was fine but git could not read the objects.
    Corrupt(String),
    Git {
        step: String,
        message: String,
    },
    TimedOut {
        step: String,
        secs: u64,
    },
    LimitExceeded {
        what: String,
        found: u64,
        max: u64,
    },
    Io(String),
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes, max } => write!(
                f,
                "the bundle has {bytes} bytes, the limit is {max}: send a smaller change"
            ),
            Self::NotABundle(why) => write!(f, "this is not a usable git bundle: {why}"),
            Self::WrongBase { expected, found } => write!(
                f,
                "the bundle is based on {found}, the lease base is {expected}: rebase onto the lease base and pack again"
            ),
            Self::RefCount(n) => write!(
                f,
                "a bundle must hold exactly one ref, this one holds {n}: pack one head only"
            ),
            Self::UnknownBase(b) => write!(
                f,
                "the base {b} is not in the reference repository: refresh the mirror"
            ),
            Self::Corrupt(why) => write!(f, "the bundle objects are damaged or unsafe: {why}"),
            Self::Git { step, message } => write!(f, "git failed in step {step}: {message}"),
            Self::TimedOut { step, secs } => write!(
                f,
                "git did not finish step {step} in {secs} seconds and was killed"
            ),
            Self::LimitExceeded { what, found, max } => write!(
                f,
                "the submission has {found} {what}, the limit is {max}: split the work into smaller orders"
            ),
            Self::Io(why) => write!(f, "file problem: {why}"),
        }
    }
}

impl std::error::Error for BundleError {}

const MAX_ERROR_CHARS: usize = 200;

/// A line of git's text that is safe to put in an error: the first line, short, without control or
/// invisible characters. `scrub` pairs name folders that must not appear in it.
pub fn clip_text(s: &str, scrub: &[(String, &'static str)]) -> String {
    let mut t = s.to_string();
    for (from, to) in scrub {
        if !from.is_empty() {
            t = t.replace(from.as_str(), to);
        }
    }
    t.lines()
        .next()
        .unwrap_or("no message")
        .chars()
        .take(MAX_ERROR_CHARS)
        .map(|c| {
            if c.is_control() || crate::gate::gates::is_invisible(c) {
                '?'
            } else {
                c
            }
        })
        .collect()
}

fn run_err(step: &str, lim: &RunLimits, e: RunError) -> BundleError {
    match e {
        RunError::TimedOut => BundleError::TimedOut {
            step: step.into(),
            secs: lim.secs,
        },
        RunError::OutputTooLarge => BundleError::LimitExceeded {
            what: format!("bytes of git output in step {step}"),
            found: lim.max_out as u64 + 1,
            max: lim.max_out as u64,
        },
        RunError::DiskTooLarge => BundleError::LimitExceeded {
            what: format!("bytes of disk use in step {step}"),
            found: lim.max_disk.saturating_add(1),
            max: lim.max_disk,
        },
        RunError::Spawn(m) => BundleError::Git {
            step: step.into(),
            message: format!("cannot start git: {}", clip_text(&m, &[])),
        },
    }
}

fn plain_limits(secs: u64, max_out: usize) -> RunLimits {
    RunLimits {
        secs,
        max_out,
        watch_dir: None,
        max_disk: 0,
    }
}

/// The git sandbox of one inspection: a temporary bare repository that borrows objects.
struct Sandbox {
    root: PathBuf,
    git_dir: PathBuf,
    hooks: PathBuf,
    home: PathBuf,
    /// The git folder of the reference repository, for questions about objects it holds.
    reference_git: PathBuf,
    limits: Limits,
    /// Folder names that git's messages may hold and that must not reach an error.
    scrub: Vec<(String, &'static str)>,
}

impl Sandbox {
    fn run(
        &self,
        step: &str,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        max_out: usize,
    ) -> Result<Run, BundleError> {
        self.run_at(&self.git_dir, step, args, stdin, max_out)
    }

    fn run_at(
        &self,
        git_dir: &Path,
        step: &str,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        max_out: usize,
    ) -> Result<Run, BundleError> {
        let mut cmd = git_command_capped(
            Some(git_dir),
            Some(&self.hooks),
            &self.home,
            self.limits.max_memory_mb,
        );
        // Objects over the blob limit are refused before any diff. Raising nothing here keeps git's
        // own threshold in step with that limit: bigger files would be streamed and shown as binary.
        cmd.arg("-c").arg(format!(
            "core.bigFileThreshold={}",
            self.limits.max_blob_bytes.max(1)
        ));
        cmd.args(args);
        let lim = RunLimits {
            secs: self.limits.git_seconds,
            max_out,
            watch_dir: Some(self.root.clone()),
            max_disk: self.limits.max_sandbox_bytes,
        };
        run_guarded(cmd, stdin, &lim).map_err(|e| run_err(step, &lim, e))
    }

    /// The first line of git's text, cut, cleaned and without the folders of this inspection.
    fn clip(&self, s: &str) -> String {
        clip_text(s, &self.scrub)
    }

    /// Like `run` but a non zero exit is an error with the first line of stderr.
    fn ok(
        &self,
        step: &str,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        max_out: usize,
    ) -> Result<Vec<u8>, BundleError> {
        let r = self.run(step, args, stdin, max_out)?;
        if r.ok {
            Ok(r.stdout)
        } else {
            Err(BundleError::Git {
                step: step.into(),
                message: self.clip(&r.stderr),
            })
        }
    }
}

fn sha256_file(path: &Path) -> Result<(String, u64), BundleError> {
    let mut f = fs::File::open(path)
        .map_err(|e| BundleError::Io(format!("cannot open the bundle file: {e}")))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    let mut n = 0u64;
    loop {
        let k = f
            .read(&mut buf)
            .map_err(|e| BundleError::Io(e.to_string()))?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((h.finalize().iter().map(|b| format!("{b:02x}")).collect(), n))
}

/// Pack the commits `base..head` of `repo` into one bundle file with exactly one ref.
///
/// `base` and `head` may be any revision names. The bundle is written to `out`. A temporary ref in the
/// repository names the head during the call and is removed again.
pub fn pack(repo: &Path, base: &str, head: &str, out: &Path) -> Result<BundleInfo, String> {
    let home = tempfile::tempdir().map_err(|e| format!("cannot make a temporary folder: {e}"))?;
    let hooks = home.path().join("nohooks");
    fs::create_dir_all(&hooks).map_err(|e| format!("cannot make a temporary folder: {e}"))?;
    let git = |args: &[&str]| -> Result<Run, String> {
        let mut cmd = git_command(None, Some(&hooks), home.path());
        cmd.arg("-C").arg(repo).args(args);
        run_limited(cmd, None, 120, 1 << 20)
            .map_err(|e| format!("git {}: {e:?}", args.first().unwrap_or(&"")))
    };
    let resolve = |rev: &str| -> Result<String, String> {
        if rev.starts_with('-') {
            return Err(format!("the revision {rev:?} must not start with a dash"));
        }
        let r = git(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ])?;
        let s = String::from_utf8_lossy(&r.stdout).trim().to_string();
        if r.ok && is_object_id(&s) {
            Ok(s)
        } else {
            Err(format!(
                "{rev:?} is not a commit of this repository: check the name"
            ))
        }
    };
    let base = resolve(base)?;
    let head = resolve(head)?;
    if base == head {
        return Err("base and head are the same commit: there is nothing to submit".into());
    }
    if !git(&["merge-base", "--is-ancestor", &base, &head])?.ok {
        return Err("the base is not an ancestor of the head: rebase onto the base first".into());
    }
    const TMP_REF: &str = "refs/sommelier/submit";
    let set = git(&["update-ref", TMP_REF, &head])?;
    if !set.ok {
        return Err(format!("cannot name the head: {}", set.stderr));
    }
    let range = format!("{base}..{TMP_REF}");
    let made = git(&["bundle", "create", &out.to_string_lossy(), &range]);
    let _ = git(&["update-ref", "-d", TMP_REF]);
    let made = made?;
    if !made.ok {
        return Err(format!(
            "git bundle create failed: {}",
            made.stderr.lines().next().unwrap_or("")
        ));
    }
    let check = || -> Result<(), String> {
        let h = parse::read_header(out).map_err(|e| e.to_string())?;
        if h.refs.len() != 1 || h.prerequisites != [base.clone()] {
            return Err("the history between base and head is not one line from the base: rebase so that the base is the only parent below the first commit".into());
        }
        Ok(())
    };
    if let Err(e) = check() {
        let _ = fs::remove_file(out);
        return Err(e);
    }
    let (sha256, bytes) = sha256_file(out).map_err(|e| e.to_string())?;
    Ok(BundleInfo {
        sha256,
        bytes,
        base,
        head,
    })
}

/// Check and unpack a bundle in a temporary folder under the system temp directory.
pub fn inspect(
    bundle: &Path,
    reference_repo: &Path,
    expected_base: &str,
    limits: &Limits,
) -> Result<ParsedDiff, BundleError> {
    inspect_in(
        bundle,
        reference_repo,
        expected_base,
        limits,
        &std::env::temp_dir(),
    )
}

/// Same as [`inspect`] with the parent folder of the temporary folder given. The temporary folder is
/// removed on every path.
pub fn inspect_in(
    bundle: &Path,
    reference_repo: &Path,
    expected_base: &str,
    limits: &Limits,
    temp_root: &Path,
) -> Result<ParsedDiff, BundleError> {
    // 1. Size first, before any parser or git sees the file.
    let meta = fs::metadata(bundle)
        .map_err(|e| BundleError::Io(format!("cannot read the bundle: {e}")))?;
    if meta.len() > limits.max_bundle_bytes {
        return Err(BundleError::TooLarge {
            bytes: meta.len(),
            max: limits.max_bundle_bytes,
        });
    }
    if !is_object_id(expected_base) {
        return Err(BundleError::Io(format!(
            "the expected base {expected_base:?} is not a full object id"
        )));
    }
    // 2. Our own header check.
    let header = parse::read_header(bundle)?;
    check_header(&header, expected_base)?;
    let head = header.refs[0].0.clone();
    let ref_name = header.refs[0].1.clone();

    // 3. The sandbox.
    let tmp = tempfile::Builder::new()
        .prefix("sommelier-bundle-")
        .tempdir_in(temp_root)
        .map_err(|e| BundleError::Io(format!("cannot make a temporary folder: {e}")))?;
    let mut sb = make_sandbox(tmp.path(), reference_repo, limits)?;
    let bundle_abs = fs::canonicalize(bundle).map_err(|e| BundleError::Io(e.to_string()))?;
    let bundle_arg = bundle_abs
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();
    sb.scrub.push((bundle_arg.replace('\\', "/"), "<bundle>"));
    sb.scrub.push((bundle_arg.clone(), "<bundle>"));

    let base_ref = format!("{expected_base}^{{commit}}");
    if !sb
        .run("base", &["cat-file", "-e", &base_ref], None, 1 << 16)?
        .ok
    {
        return Err(BundleError::UnknownBase(expected_base.to_string()));
    }
    let v = sb.run("verify", &["bundle", "verify", &bundle_arg], None, 1 << 16)?;
    if !v.ok {
        return Err(BundleError::Corrupt(sb.clip(&v.stderr)));
    }
    let spec = format!("+{ref_name}:refs/submission/head");
    let f = sb.run(
        "fetch",
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            &bundle_arg,
            &spec,
        ],
        None,
        1 << 16,
    )?;
    if !f.ok {
        return Err(BundleError::Corrupt(sb.clip(&f.stderr)));
    }
    let got = sb.ok(
        "rev-parse",
        &["rev-parse", "--verify", "refs/submission/head^{commit}"],
        None,
        1 << 12,
    )?;
    if String::from_utf8_lossy(&got).trim() != head {
        return Err(BundleError::Corrupt(
            "the fetched head differs from the header".into(),
        ));
    }
    build(&sb, &bundle_abs, expected_base, &head, limits)
}

fn check_header(h: &Header, expected_base: &str) -> Result<(), BundleError> {
    if h.refs.len() != 1 {
        return Err(BundleError::RefCount(h.refs.len()));
    }
    if h.prerequisites.len() != 1 || h.prerequisites[0] != expected_base {
        let found = if h.prerequisites.is_empty() {
            "no prerequisite".to_string()
        } else {
            h.prerequisites.join(",")
        };
        return Err(BundleError::WrongBase {
            expected: expected_base.into(),
            found,
        });
    }
    Ok(())
}

fn make_sandbox(root: &Path, reference: &Path, limits: &Limits) -> Result<Sandbox, BundleError> {
    let io = |e: std::io::Error| BundleError::Io(e.to_string());
    let (git_dir, hooks, home) = (
        root.join("repo.git"),
        root.join("nohooks"),
        root.join("home"),
    );
    fs::create_dir_all(&hooks).map_err(io)?;
    fs::create_dir_all(&home).map_err(io)?;
    let mut sb = Sandbox {
        root: root.to_path_buf(),
        git_dir: git_dir.clone(),
        hooks,
        home,
        reference_git: reference.to_path_buf(),
        limits: limits.clone(),
        scrub: Vec::new(),
    };
    for dir in [root, reference] {
        let name = if dir == root {
            "<sandbox>"
        } else {
            "<reference>"
        };
        let plain = dir
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string();
        sb.scrub.push((plain.replace('\\', "/"), name));
        sb.scrub.push((plain, name));
    }
    let mut cmd = git_command(None, Some(&sb.hooks), &sb.home);
    cmd.args(["init", "--quiet", "--bare", "--template="])
        .arg(&git_dir);
    let lim = plain_limits(limits.git_seconds, 1 << 16);
    let r = run_limited(cmd, None, lim.secs, lim.max_out).map_err(|e| run_err("init", &lim, e))?;
    if !r.ok {
        return Err(BundleError::Git {
            step: "init".into(),
            message: sb.clip(&r.stderr),
        });
    }
    let objects = {
        let dot = reference.join(".git").join("objects");
        if dot.is_dir() {
            dot
        } else {
            reference.join("objects")
        }
    };
    if !objects.is_dir() {
        return Err(BundleError::Io(
            "the reference repository is not a git repository".into(),
        ));
    }
    let objects = fs::canonicalize(&objects).map_err(io)?;
    sb.reference_git = objects
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| objects.clone());
    let line = objects
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/");
    fs::write(
        git_dir.join("objects").join("info").join("alternates"),
        format!("{line}\n"),
    )
    .map_err(io)?;
    Ok(sb)
}

fn build(
    sb: &Sandbox,
    bundle: &Path,
    base: &str,
    head: &str,
    limits: &Limits,
) -> Result<ParsedDiff, BundleError> {
    let range = format!("{base}..refs/submission/head");
    let count = sb.ok("count", &["rev-list", "--count", &range], None, 1 << 12)?;
    let n: usize = String::from_utf8_lossy(&count)
        .trim()
        .parse()
        .unwrap_or(usize::MAX);
    if n > limits.max_commits {
        return Err(BundleError::LimitExceeded {
            what: "commits".into(),
            found: n as u64,
            max: limits.max_commits as u64,
        });
    }
    // Object sizes first. Nothing below diffs or reads a blob before this passed.
    check_objects(sb, &range, limits)?;

    let mut log_args = vec![
        "log",
        "--reverse",
        "--topo-order",
        "--no-color",
        "--encoding=UTF-8",
        parse::LOG_FORMAT,
    ];
    log_args.push(&range);
    let log = sb.ok("log", &log_args, None, limits.max_diff_bytes)?;
    let mut commits = parse::parse_log(&log)?;
    for c in &mut commits {
        c.paths = commit_paths(sb, &c.sha, limits)?;
    }
    let base_paths = base_paths(sb, base, limits)?;

    let head_ref = "refs/submission/head";
    // The count comes from a diff without rename detection, which is cheap. Rename detection runs only
    // for a small number of files, its cost grows with the square of that number.
    let mut count_args = vec!["diff", "--raw", "-z", "--no-renames", "--full-index"];
    count_args.extend(diff_flags());
    count_args.extend([base, head_ref]);
    let plain =
        parse::parse_raw(&sb.ok("diff-count", &count_args, None, limits.max_diff_bytes)?)?;
    if plain.len() > limits.max_files {
        return Err(BundleError::LimitExceeded {
            what: "changed files".into(),
            found: plain.len() as u64,
            max: limits.max_files as u64,
        });
    }
    let rename_flag = format!("-l{}", limits.rename_max_files);
    let rename = plain.len() <= limits.rename_max_files;
    let detect: Vec<&str> = if rename {
        vec!["-M", &rename_flag]
    } else {
        vec!["--no-renames"]
    };
    let raw = if rename {
        let mut raw_args = vec!["diff", "--raw", "-z", "--full-index"];
        raw_args.extend(&detect);
        raw_args.extend(diff_flags());
        raw_args.extend([base, head_ref]);
        parse::parse_raw(&sb.ok("diff-raw", &raw_args, None, limits.max_diff_bytes)?)?
    } else {
        plain
    };

    let sizes = blob_sizes(sb, &raw, limits)?;

    let mut patch_args = vec!["diff", "-U0", "--full-index"];
    patch_args.extend(&detect);
    patch_args.extend(diff_flags());
    patch_args.extend([base, head_ref]);
    let patch = sb.run("diff-patch", &patch_args, None, limits.max_diff_bytes)?;
    if !patch.ok {
        return Err(BundleError::Git {
            step: "diff-patch".into(),
            message: sb.clip(&patch.stderr),
        });
    }
    let mut sections = parse::parse_patch(&patch.stdout).into_iter();
    let mut files = Vec::with_capacity(raw.len());
    for (e, size) in raw.iter().zip(sizes) {
        // A change of type is shown as a delete and a create in the patch but is one raw entry.
        let mut sec = sections.next().ok_or_else(mismatch)?;
        if e.status == 'T' {
            let second = sections.next().ok_or_else(mismatch)?;
            sec.added = second.added;
            sec.binary |= second.binary;
        }
        let deleted = e.status == 'D';
        let mode = parse::file_mode(if deleted { &e.old_mode } else { &e.new_mode });
        let binary = sec.binary;
        // A pure rename has no added lines, so the capability gate reads the whole new content.
        let content = if matches!(e.status, 'R' | 'C') && !binary && mode != FileMode::Submodule {
            blob_lines(sb, &e.new_sha, limits)?
        } else {
            Vec::new()
        };
        files.push(FileChange {
            path: e.path.clone(),
            old_path: e.old_path.clone(),
            change: parse::change_kind(e.status),
            mode,
            binary,
            size_after: size,
            added: if binary { vec![] } else { sec.added },
            removed: if binary { vec![] } else { sec.removed },
            old_mode: parse::old_file_mode(&e.old_mode),
            content,
        });
    }
    if sections.next().is_some() {
        return Err(mismatch());
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let (sha, bytes) = sha256_file(bundle)?;
    Ok(ParsedDiff {
        base: base.into(),
        head: head.into(),
        commits,
        files,
        bundle_sha256: sha,
        bundle_bytes: bytes,
        base_paths,
    })
}

/// List the objects the submission brings and refuse on the first one over the blob cap, on too many
/// objects and on too many bytes in total. Runs before any diff.
fn check_objects(sb: &Sandbox, range: &str, limits: &Limits) -> Result<(), BundleError> {
    let listing = sb.ok(
        "objects",
        &["rev-list", "--objects", range],
        None,
        limits.max_diff_bytes,
    )?;
    let mut ids = String::new();
    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut n = 0usize;
    for line in String::from_utf8_lossy(&listing).lines() {
        // A path with a line break in it makes extra lines. Only lines that start with an id count.
        let id = line.split(' ').next().unwrap_or("");
        if !is_object_id(id) {
            continue;
        }
        n += 1;
        if n > limits.max_objects {
            return Err(BundleError::LimitExceeded {
                what: "objects".into(),
                found: n as u64,
                max: limits.max_objects as u64,
            });
        }
        ids.push_str(id);
        ids.push('\n');
        reachable.insert(id.to_string());
    }
    check_unreachable(sb, &reachable, limits)?;
    if n == 0 {
        return Ok(());
    }
    let out = sb.ok(
        "object-sizes",
        &["cat-file", "--batch-check=%(objecttype) %(objectsize)"],
        Some(ids.into_bytes()),
        limits.max_diff_bytes,
    )?;
    let text = String::from_utf8_lossy(&out);
    let mut total = 0u64;
    let mut seen = 0usize;
    for line in text.lines() {
        seen += 1;
        let size = line
            .split_once(' ')
            .and_then(|(_, s)| s.trim().parse::<u64>().ok())
            .ok_or_else(|| BundleError::Corrupt("an object of the bundle is missing".into()))?;
        let kind = line.split(' ').next().unwrap_or("object");
        if size > limits.max_blob_bytes {
            return Err(BundleError::LimitExceeded {
                what: format!("bytes in one {} object", clip_text(kind, &[])),
                found: size,
                max: limits.max_blob_bytes,
            });
        }
        total = total.saturating_add(size);
        if total > limits.max_total_object_bytes {
            return Err(BundleError::LimitExceeded {
                what: "bytes in all objects".into(),
                found: total,
                max: limits.max_total_object_bytes,
            });
        }
    }
    if seen != n {
        return Err(BundleError::Corrupt(
            "unexpected answer for the object sizes".into(),
        ));
    }
    Ok(())
}

/// Refuse a pack that holds an object the range does not reach. The objects of the sandbox itself
/// (not the borrowed ones) are listed from the index of each pack and from the loose folders. An object
/// that is not reachable and that the reference repository does not hold is a hidden object, and
/// nothing counted it before.
fn check_unreachable(
    sb: &Sandbox,
    reachable: &BTreeSet<String>,
    limits: &Limits,
) -> Result<(), BundleError> {
    let io = |e: std::io::Error| BundleError::Io(e.to_string());
    let objects = sb.git_dir.join("objects");
    let mut own: BTreeSet<String> = BTreeSet::new();
    let packs = objects.join("pack");
    if packs.is_dir() {
        for e in fs::read_dir(&packs).map_err(io)? {
            let path = e.map_err(io)?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("idx") {
                continue;
            }
            let idx = fs::read(&path).map_err(io)?;
            let out = sb.ok(
                "show-index",
                &["show-index"],
                Some(idx),
                limits.max_diff_bytes,
            )?;
            for line in String::from_utf8_lossy(&out).lines() {
                let id = line.split(' ').nth(1).unwrap_or("");
                if !is_object_id(id) {
                    return Err(BundleError::Corrupt("unexpected pack index".into()));
                }
                own.insert(id.to_string());
            }
        }
    }
    for e in fs::read_dir(&objects).map_err(io)? {
        let dir = e.map_err(io)?;
        let name = dir.file_name().to_string_lossy().to_string();
        if name.len() != 2 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        for f in fs::read_dir(dir.path()).map_err(io)? {
            let rest = f.map_err(io)?.file_name().to_string_lossy().to_string();
            own.insert(format!("{name}{rest}"));
        }
    }
    let extra: Vec<&String> = own.iter().filter(|id| !reachable.contains(*id)).collect();
    if extra.is_empty() {
        return Ok(());
    }
    // A thin pack is completed with the bases it needs, and the reference repository holds those.
    let mut ids = String::new();
    for id in &extra {
        ids.push_str(id);
        ids.push('\n');
    }
    let reference = sb.reference_git.clone();
    let r = sb.run_at(
        &reference,
        "reference-objects",
        &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        Some(ids.into_bytes()),
        limits.max_diff_bytes,
    )?;
    let text = String::from_utf8_lossy(&r.stdout);
    let known = text.lines().filter(|l| !l.ends_with(" missing")).count();
    if !r.ok || known != extra.len() {
        return Err(BundleError::Corrupt(format!(
            "the bundle holds {} objects that no ref reaches",
            extra.len() - known.min(extra.len())
        )));
    }
    Ok(())
}

/// The paths one commit changes against its parent, as git prints them.
fn commit_paths(sb: &Sandbox, sha: &str, limits: &Limits) -> Result<Vec<String>, BundleError> {
    if !is_object_id(sha) {
        return Err(BundleError::Corrupt(
            "a commit id is not an object id".into(),
        ));
    }
    let mut args = vec![
        "diff-tree",
        "-r",
        "--no-renames",
        "--name-only",
        "-z",
        "--no-commit-id",
    ];
    args.extend(diff_flags());
    args.push(sha);
    let paths = parse::parse_paths(&sb.ok("commit-paths", &args, None, limits.max_diff_bytes)?);
    if paths.len() > limits.max_files {
        return Err(BundleError::LimitExceeded {
            what: "changed files in one commit".into(),
            found: paths.len() as u64,
            max: limits.max_files as u64,
        });
    }
    Ok(paths)
}

/// Every file path of the base tree.
fn base_paths(sb: &Sandbox, base: &str, limits: &Limits) -> Result<Vec<String>, BundleError> {
    let out = sb.ok(
        "base-paths",
        &["ls-tree", "-r", "--name-only", "-z", "--full-tree", base],
        None,
        limits.max_diff_bytes,
    )?;
    Ok(parse::parse_paths(&out))
}

/// All lines of one text blob, for a renamed file.
fn blob_lines(sb: &Sandbox, sha: &str, limits: &Limits) -> Result<Vec<Line>, BundleError> {
    if !is_object_id(sha) {
        return Err(BundleError::Corrupt("a blob id is not an object id".into()));
    }
    let max = usize::try_from(limits.max_blob_bytes).unwrap_or(usize::MAX);
    let out = sb.ok("blob", &["cat-file", "blob", sha], None, max)?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .enumerate()
        .map(|(i, t)| Line {
            no: u32::try_from(i + 1).unwrap_or(u32::MAX),
            text: t.to_string(),
        })
        .collect())
}

fn mismatch() -> BundleError {
    BundleError::Git {
        step: "parse".into(),
        message: "the patch and the raw diff do not list the same files".into(),
    }
}

/// Sizes of the new blobs in raw order, 0 for deleted paths and submodules. Checks the blob limit.
fn blob_sizes(
    sb: &Sandbox,
    raw: &[parse::RawEntry],
    limits: &Limits,
) -> Result<Vec<u64>, BundleError> {
    let mut input = String::new();
    let mut asked = Vec::new();
    for (i, e) in raw.iter().enumerate() {
        if e.status != 'D'
            && parse::file_mode(&e.new_mode) != crate::gate::diff::FileMode::Submodule
        {
            input.push_str(&e.new_sha);
            input.push('\n');
            asked.push(i);
        }
    }
    let mut sizes = vec![0u64; raw.len()];
    if asked.is_empty() {
        return Ok(sizes);
    }
    let out = sb.ok(
        "blob-sizes",
        &["cat-file", "--batch-check=%(objectsize)"],
        Some(input.into_bytes()),
        limits.max_diff_bytes,
    )?;
    let text = String::from_utf8_lossy(&out);
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != asked.len() {
        return Err(BundleError::Git {
            step: "blob-sizes".into(),
            message: "unexpected answer".into(),
        });
    }
    for (i, l) in asked.iter().zip(lines) {
        let n: u64 = l.trim().parse().map_err(|_| BundleError::Git {
            step: "blob-sizes".into(),
            message: format!("object missing: {l}"),
        })?;
        if n > limits.max_blob_bytes {
            return Err(BundleError::LimitExceeded {
                what: format!(
                    "bytes in the blob of {}",
                    crate::gate::gates::clip(&raw[*i].path)
                ),
                found: n,
                max: limits.max_blob_bytes,
            });
        }
        sizes[*i] = n;
    }
    Ok(sizes)
}
