//! Submissions as git bundles: packing on the member side, checking and safe unpacking on the hub side.
//!
//! Part of the public factory, see `docs/factory.md` sections 8.1 and 8.2. Unpacking produces a
//! [`crate::gate::diff::ParsedDiff`] and never checks out, never builds and never runs anything from the
//! bundle. The bundle is hostile input: it is size checked before git sees it, its header is read by our
//! own parser, and every git call runs with hooks, external diff, textconv and all config files off.

pub mod parse;
pub mod runner;

use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::gate::diff::{FileChange, ParsedDiff};
use parse::{Header, is_object_id};
use runner::{Run, RunError, diff_flags, git_command, run_limited};

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

fn run_err(step: &str, secs: u64, e: RunError) -> BundleError {
    match e {
        RunError::TimedOut => BundleError::TimedOut {
            step: step.into(),
            secs,
        },
        RunError::OutputTooLarge => BundleError::LimitExceeded {
            what: format!("bytes of git output in step {step}"),
            found: u64::MAX,
            max: 0,
        },
        RunError::Spawn(m) => BundleError::Git {
            step: step.into(),
            message: format!("cannot start git: {m}"),
        },
    }
}

/// The git sandbox of one inspection: a temporary bare repository that borrows objects.
struct Sandbox {
    git_dir: PathBuf,
    hooks: PathBuf,
    home: PathBuf,
    secs: u64,
}

impl Sandbox {
    fn run(
        &self,
        step: &str,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        max_out: usize,
    ) -> Result<Run, BundleError> {
        let mut cmd = git_command(Some(&self.git_dir), Some(&self.hooks), &self.home);
        cmd.args(args);
        let out =
            run_limited(cmd, stdin, self.secs, max_out).map_err(|e| run_err(step, self.secs, e))?;
        Ok(out)
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
                message: r.stderr.lines().next().unwrap_or("no message").to_string(),
            })
        }
    }
}

fn sha256_file(path: &Path) -> Result<(String, u64), BundleError> {
    let mut f = fs::File::open(path)
        .map_err(|e| BundleError::Io(format!("cannot open {}: {e}", path.display())))?;
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
    let sb = make_sandbox(tmp.path(), reference_repo, limits)?;
    let bundle_abs = fs::canonicalize(bundle).map_err(|e| BundleError::Io(e.to_string()))?;
    let bundle_arg = bundle_abs
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();

    let base_ref = format!("{expected_base}^{{commit}}");
    if !sb
        .run("base", &["cat-file", "-e", &base_ref], None, 1 << 16)?
        .ok
    {
        return Err(BundleError::UnknownBase(expected_base.to_string()));
    }
    let v = sb.run("verify", &["bundle", "verify", &bundle_arg], None, 1 << 16)?;
    if !v.ok {
        return Err(BundleError::Corrupt(
            v.stderr
                .lines()
                .next()
                .unwrap_or("verify failed")
                .to_string(),
        ));
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
        return Err(BundleError::Corrupt(
            f.stderr
                .lines()
                .next()
                .unwrap_or("fetch failed")
                .to_string(),
        ));
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
    let sb = Sandbox {
        git_dir: git_dir.clone(),
        hooks,
        home,
        secs: limits.git_seconds,
    };
    let mut cmd = git_command(None, Some(&sb.hooks), &sb.home);
    cmd.args(["init", "--quiet", "--bare", "--template="])
        .arg(&git_dir);
    let r = run_limited(cmd, None, limits.git_seconds, 1 << 16)
        .map_err(|e| run_err("init", limits.git_seconds, e))?;
    if !r.ok {
        return Err(BundleError::Git {
            step: "init".into(),
            message: r.stderr,
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
        return Err(BundleError::Io(format!(
            "{} is not a git repository",
            reference.display()
        )));
    }
    let objects = fs::canonicalize(&objects).map_err(io)?;
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
    let mut log_args = vec![
        "log",
        "--reverse",
        "--topo-order",
        "--no-color",
        "--encoding=UTF-8",
        "--format=%H%x00%P%x00%an%x00%ae%x00%B%x00",
    ];
    log_args.push(&range);
    let log = sb.ok("log", &log_args, None, limits.max_diff_bytes)?;
    let commits = parse::parse_log(&log)?;

    let head_ref = "refs/submission/head";
    let mut raw_args = vec!["diff", "--raw", "-z", "-M", "--full-index"];
    raw_args.extend(diff_flags());
    raw_args.extend([base, head_ref]);
    let raw = parse::parse_raw(&sb.ok("diff-raw", &raw_args, None, limits.max_diff_bytes)?)?;
    if raw.len() > limits.max_files {
        return Err(BundleError::LimitExceeded {
            what: "changed files".into(),
            found: raw.len() as u64,
            max: limits.max_files as u64,
        });
    }

    let sizes = blob_sizes(sb, &raw, limits)?;

    let mut patch_args = vec!["diff", "-U0", "-M", "--full-index"];
    patch_args.extend(diff_flags());
    patch_args.extend([base, head_ref]);
    let patch = sb.run("diff-patch", &patch_args, None, limits.max_diff_bytes)?;
    if !patch.ok {
        return Err(BundleError::Git {
            step: "diff-patch".into(),
            message: patch.stderr,
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
        files.push(FileChange {
            path: e.path.clone(),
            old_path: e.old_path.clone(),
            change: parse::change_kind(e.status),
            mode,
            binary,
            size_after: size,
            added: if binary { vec![] } else { sec.added },
            removed: if binary { vec![] } else { sec.removed },
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
    })
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
                what: format!("bytes in the blob of {}", raw[*i].path),
                found: n,
                max: limits.max_blob_bytes,
            });
        }
        sizes[*i] = n;
    }
    Ok(sizes)
}
