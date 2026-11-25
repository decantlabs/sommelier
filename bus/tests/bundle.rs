//! Tests of the bundle module. They build small throwaway repositories with git, no network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sommelier::bundle::runner::{RunError, diff_flags, git_args, git_env, run_limited};
use sommelier::bundle::{BundleError, Limits, inspect_in, pack};
use sommelier::gate::diff::{ChangeKind, FileMode, ParsedDiff};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.org",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["init", "-q", "-b", "main"]);
    fs::write(d.path().join("keep.txt"), "one\ntwo\nthree\n").unwrap();
    fs::write(
        d.path().join("old name.txt"),
        "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\n",
    )
    .unwrap();
    fs::write(d.path().join("gone.txt"), "bye\n").unwrap();
    git(d.path(), &["add", "-A"]);
    git(d.path(), &["commit", "-q", "-m", "base"]);
    d
}

fn rev(d: &Path, r: &str) -> String {
    git(d, &["rev-parse", r])
}

fn sandbox_root() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn is_empty(d: &Path) -> bool {
    fs::read_dir(d).unwrap().next().is_none()
}

fn inspect(b: &Path, r: &Path, base: &str, root: &Path) -> Result<ParsedDiff, BundleError> {
    inspect_in(b, r, base, &Limits::default(), root)
}

/// A repo with a rich change set on top of the base and its bundle.
fn rich() -> (
    tempfile::TempDir,
    String,
    String,
    PathBuf,
    tempfile::TempDir,
) {
    let r = repo();
    let p = r.path();
    let base = rev(p, "HEAD");
    fs::write(p.join("keep.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    fs::create_dir_all(p.join("dir")).unwrap();
    git(p, &["mv", "old name.txt", "dir/ünï cöde.txt"]);
    fs::remove_file(p.join("gone.txt")).unwrap();
    fs::write(p.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::write(p.join("blob.bin"), [0u8, 1, 2, 0, 255, 0, 7]).unwrap();
    git(p, &["add", "-A"]);
    let link_blob = {
        fs::write(p.join(".linktarget"), "keep.txt").unwrap();
        git(p, &["hash-object", "-w", ".linktarget"])
    };
    fs::remove_file(p.join(".linktarget")).unwrap();
    git(
        p,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100755,{},run.sh", git(p, &["hash-object", "-w", "run.sh"])),
        ],
    );
    git(
        p,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{link_blob},link"),
        ],
    );
    git(
        p,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},sub", "1".repeat(40)),
        ],
    );
    git(p, &["commit", "-q", "-m", "work\n\nbody line"]);
    let head = rev(p, "HEAD");
    let out_dir = sandbox_root();
    let out = out_dir.path().join("s.bundle");
    pack(p, &base, &head, &out).expect("pack");
    (r, base, head, out, out_dir)
}

#[test]
fn pack_then_inspect_reports_files_modes_lines_and_commits() {
    let (r, base, head, out, _o) = rich();
    let root = sandbox_root();
    let d = inspect(&out, r.path(), &base, root.path()).expect("inspect");
    assert!(is_empty(root.path()));
    assert_eq!(d.base, base);
    assert_eq!(d.head, head);
    assert_eq!(d.commits.len(), 1);
    assert_eq!(d.commits[0].parents, vec![base.clone()]);
    assert_eq!(d.commits[0].author_name, "Test");
    assert_eq!(d.commits[0].message, "work\n\nbody line");
    assert_eq!(d.bundle_bytes, fs::metadata(&out).unwrap().len());
    assert_eq!(d.bundle_sha256.len(), 64);
    let f = |p: &str| {
        d.files.iter().find(|f| f.path == p).unwrap_or_else(|| {
            panic!(
                "no {p}: {:?}",
                d.files.iter().map(|f| &f.path).collect::<Vec<_>>()
            )
        })
    };
    let keep = f("keep.txt");
    assert_eq!(keep.change, ChangeKind::Modified);
    assert_eq!(
        keep.removed
            .iter()
            .map(|l| (l.no, l.text.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "two")]
    );
    assert_eq!(
        keep.added
            .iter()
            .map(|l| (l.no, l.text.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "TWO"), (4, "four")]
    );
    assert_eq!(keep.size_after, 19);
    let ren = f("dir/ünï cöde.txt");
    assert_eq!(ren.change, ChangeKind::Renamed);
    assert_eq!(ren.old_path.as_deref(), Some("old name.txt"));
    let gone = f("gone.txt");
    assert_eq!(gone.change, ChangeKind::Deleted);
    assert_eq!(gone.size_after, 0);
    assert_eq!(gone.removed.len(), 1);
    assert_eq!(f("run.sh").mode, FileMode::Executable);
    assert_eq!(f("run.sh").change, ChangeKind::Added);
    assert_eq!(f("link").mode, FileMode::Symlink);
    assert_eq!(f("sub").mode, FileMode::Submodule);
    let bin = f("blob.bin");
    assert!(bin.binary);
    assert!(bin.added.is_empty());
    assert_eq!(bin.size_after, 7);
    let paths: Vec<_> = d.files.iter().map(|f| f.path.clone()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
}

#[test]
fn wrong_base_is_refused() {
    let (r, _base, _head, out, _o) = rich();
    let root = sandbox_root();
    let other = "2".repeat(40);
    let e = inspect(&out, r.path(), &other, root.path()).unwrap_err();
    assert!(matches!(e, BundleError::WrongBase { .. }), "{e}");
    assert!(is_empty(root.path()));
}

#[test]
fn two_refs_are_refused() {
    let r = repo();
    let p = r.path();
    let base = rev(p, "HEAD");
    fs::write(p.join("a"), "a\n").unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "a"]);
    git(p, &["branch", "second"]);
    let o = sandbox_root();
    let out = o.path().join("two.bundle");
    git(
        p,
        &[
            "bundle",
            "create",
            out.to_str().unwrap(),
            &format!("{base}..main"),
            &format!("{base}..second"),
        ],
    );
    let root = sandbox_root();
    let e = inspect(&out, p, &base, root.path()).unwrap_err();
    assert_eq!(e, BundleError::RefCount(2));
    assert!(is_empty(root.path()));
}

#[test]
fn a_file_that_is_not_a_bundle_is_refused() {
    let r = repo();
    let o = sandbox_root();
    let f = o.path().join("x.bundle");
    fs::write(&f, "hello, not a bundle\n").unwrap();
    let root = sandbox_root();
    let e = inspect(&f, r.path(), &rev(r.path(), "HEAD"), root.path()).unwrap_err();
    assert!(matches!(e, BundleError::NotABundle(_)), "{e}");
    assert!(is_empty(root.path()));
}

#[test]
fn a_truncated_bundle_is_refused() {
    let r = repo();
    let p = r.path();
    let base = rev(p, "HEAD");
    let reference = tempfile::tempdir().unwrap();
    git(
        reference.path(),
        &["clone", "-q", "--bare", p.to_str().unwrap(), "ref.git"],
    );
    let noise: String = (0..4000)
        .map(|i| format!("line {i} {}\n", i * 7919 % 1013))
        .collect();
    fs::write(p.join("big.txt"), noise).unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "big"]);
    let o = sandbox_root();
    let out = o.path().join("t.bundle");
    pack(p, &base, "HEAD", &out).unwrap();
    let bytes = fs::read(&out).unwrap();
    fs::write(&out, &bytes[..bytes.len() - bytes.len() / 3]).unwrap();
    let root = sandbox_root();
    let e = inspect(&out, &reference.path().join("ref.git"), &base, root.path()).unwrap_err();
    assert!(
        matches!(e, BundleError::Corrupt(_) | BundleError::Git { .. }),
        "{e}"
    );
    assert!(is_empty(root.path()));
}

#[test]
fn an_oversized_file_is_refused_without_starting_git() {
    let o = sandbox_root();
    let f = o.path().join("huge.bundle");
    fs::write(&f, vec![b'x'; 2048]).unwrap();
    let lim = Limits {
        max_bundle_bytes: 1024,
        ..Limits::default()
    };
    let root = sandbox_root();
    // The reference repo does not exist: only a check before git can answer TooLarge.
    let e = inspect_in(
        &f,
        &o.path().join("nope"),
        &"3".repeat(40),
        &lim,
        root.path(),
    )
    .unwrap_err();
    assert_eq!(
        e,
        BundleError::TooLarge {
            bytes: 2048,
            max: 1024
        }
    );
    assert!(is_empty(root.path()));
}

#[test]
fn limits_on_commits_and_blobs_apply() {
    let (r, base, _h, out, _o) = rich();
    let root = sandbox_root();
    let lim = Limits {
        max_files: 2,
        ..Limits::default()
    };
    let e = inspect_in(&out, r.path(), &base, &lim, root.path()).unwrap_err();
    assert!(matches!(e, BundleError::LimitExceeded { .. }), "{e}");
    let lim = Limits {
        max_blob_bytes: 5,
        ..Limits::default()
    };
    let e = inspect_in(&out, r.path(), &base, &lim, root.path()).unwrap_err();
    assert!(matches!(e, BundleError::LimitExceeded { .. }), "{e}");
    assert!(is_empty(root.path()));
}

#[test]
fn a_merge_commit_has_two_parents() {
    let r = repo();
    let p = r.path();
    let base = rev(p, "HEAD");
    git(p, &["checkout", "-q", "-b", "side"]);
    fs::write(p.join("side.txt"), "s\n").unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "side"]);
    git(p, &["checkout", "-q", "main"]);
    fs::write(p.join("main.txt"), "m\n").unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "mainline"]);
    git(p, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    let o = sandbox_root();
    let out = o.path().join("m.bundle");
    pack(p, &base, "main", &out).unwrap();
    let root = sandbox_root();
    let d = inspect(&out, p, &base, root.path()).unwrap();
    assert_eq!(d.commits.len(), 3);
    let last = d.commits.last().unwrap();
    assert_eq!(last.parents.len(), 2);
    assert_eq!(last.message, "merge");
    assert!(is_empty(root.path()));
}

#[test]
fn pack_refuses_a_head_that_is_not_above_the_base() {
    let r = repo();
    let p = r.path();
    let base = rev(p, "HEAD");
    let o = sandbox_root();
    let out = o.path().join("same.bundle");
    assert!(pack(p, &base, &base, &out).is_err());
    assert!(pack(p, "no-such-rev", &base, &out).is_err());
    assert!(!out.exists());
}

#[test]
fn fixed_arguments_turn_hooks_external_diff_and_textconv_off() {
    let args: Vec<String> = git_args(Some(Path::new("G")), Some(Path::new("H")))
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(args.contains(&"--no-pager".to_string()));
    assert!(args.contains(&"core.hooksPath=H".to_string()));
    assert!(args.contains(&"fetch.fsckObjects=true".to_string()));
    assert!(args.contains(&"transfer.fsckObjects=true".to_string()));
    assert!(args.contains(&"core.fsmonitor=false".to_string()));
    let flags = diff_flags();
    assert!(flags.contains(&"--no-ext-diff") && flags.contains(&"--no-textconv"));
    let env = git_env();
    let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
    assert_eq!(get("GIT_CONFIG_NOSYSTEM"), Some("1"));
    assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0"));
    assert!(get("GIT_CONFIG_GLOBAL").is_some());
    assert!(get("GIT_EXTERNAL_DIFF").is_none());
}

#[test]
fn a_config_hook_or_filter_in_the_reference_repo_never_runs() {
    let r = repo();
    let p = r.path();
    let base = rev(p, "HEAD");
    let marker = tempfile::tempdir().unwrap();
    let m = marker
        .path()
        .join("ran")
        .to_string_lossy()
        .replace('\\', "/");
    let script = format!("#!/bin/sh\necho x > '{m}'\n");
    for h in [
        "reference-transaction",
        "post-checkout",
        "post-merge",
        "pre-auto-gc",
        "post-index-change",
        "fsmonitor-watchman",
    ] {
        fs::write(p.join(".git/hooks").join(h), &script).unwrap();
    }
    fs::write(p.join("ext.sh"), &script).unwrap();
    let cfg = format!(
        "[diff]\n\texternal = sh {m}\n[diff \"x\"]\n\ttextconv = sh {m}\n[core]\n\tfsmonitor = sh {m}\n"
    );
    let mut c = fs::read_to_string(p.join(".git/config")).unwrap();
    c.push_str(&cfg);
    fs::write(p.join(".git/config"), c).unwrap();
    fs::write(p.join(".gitattributes"), "*.txt diff=x\n").unwrap();
    fs::write(p.join("new.txt"), "hi\n").unwrap();
    git(p, &["add", "-A"]);
    // The commit itself may run hooks of the test repo: only start measuring after it.
    git(
        p,
        &[
            "-c",
            "core.hooksPath=/nonexistent",
            "commit",
            "-q",
            "-m",
            "x",
        ],
    );
    let _ = fs::remove_file(marker.path().join("ran"));
    let o = sandbox_root();
    let out = o.path().join("h.bundle");
    pack(p, &base, "HEAD", &out).unwrap();
    let root = sandbox_root();
    let d = inspect(&out, p, &base, root.path()).unwrap();
    assert!(d.files.iter().any(|f| f.path == "new.txt"));
    assert!(
        !marker.path().join("ran").exists(),
        "something from the repo config ran"
    );
}

#[test]
fn paths_with_spaces_and_non_ascii_survive() {
    let r = repo();
    let p = r.path();
    let base = rev(p, "HEAD");
    fs::create_dir_all(p.join("a b/ç")).unwrap();
    fs::write(p.join("a b/ç/日本 語.txt"), "x\n").unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "paths"]);
    let o = sandbox_root();
    let out = o.path().join("p.bundle");
    pack(p, &base, "HEAD", &out).unwrap();
    let root = sandbox_root();
    let d = inspect(&out, p, &base, root.path()).unwrap();
    assert_eq!(d.files.len(), 1);
    assert_eq!(d.files[0].path, "a b/ç/日本 語.txt");
}

#[test]
fn the_time_limit_kills_a_hanging_child() {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("ping");
        c.args(["-n", "30", "127.0.0.1"]);
        c
    } else {
        let mut c = Command::new("sleep");
        c.arg("30");
        c
    };
    cmd.env_clear();
    if let Some(p) = std::env::var_os("SystemRoot") {
        cmd.env("SystemRoot", p);
    }
    let t = std::time::Instant::now();
    let r = run_limited(cmd, None, 1, 1024);
    assert_eq!(r.unwrap_err(), RunError::TimedOut);
    assert!(t.elapsed().as_secs() < 10);
}

#[test]
fn no_temp_folder_is_left_after_success_and_errors() {
    let (r, base, _h, out, _o) = rich();
    let root = sandbox_root();
    inspect(&out, r.path(), &base, root.path()).unwrap();
    assert!(is_empty(root.path()));
    inspect(&out, r.path(), &"4".repeat(40), root.path()).unwrap_err();
    assert!(is_empty(root.path()));
    // A base the reference repository does not know fails after the sandbox exists.
    let empty_ref = sandbox_root();
    git(empty_ref.path(), &["init", "-q"]);
    let e = inspect(&out, empty_ref.path(), &base, root.path()).unwrap_err();
    assert!(matches!(e, BundleError::UnknownBase(_)), "{e}");
    assert!(is_empty(root.path()));
}
