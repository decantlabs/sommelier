//! Provenance check of a build with the GitHub command line tool, and the unpacking of the build.
//!
//! A build is ONE archive file. The client verifies that file, unpacks it into a fresh folder and
//! the run uses that folder and nothing else (`docs/factory.md` section 9.2). The check fails closed:
//! anything other than a clean success is an error that says what to install or do, and nothing is
//! run and nothing is sent.
//!
//! The attestation must say three things, not only that the repository signed something: the build
//! came from the commit of the run order (`--source-digest`), from the main branch (`--source-ref`)
//! and from the release workflow of the repository (`--signer-workflow`). An older release of the
//! same repository fails the first.

use std::path::{Path, PathBuf};

use super::cmd::{CommandRunner, RunError};
use crate::report::orders::plain_commit;

/// The workflow file of the repository that builds and attests the release.
pub const RELEASE_WORKFLOW: &str = ".github/workflows/release.yml";

/// The ref a build must come from.
pub const SOURCE_REF: &str = "refs/heads/main";

/// Most files an unpacked build may hold.
const MAX_FILES: usize = 100_000;

/// Words `gh attestation verify` prints on a clean success.
const SUCCESS_MARK: &str = "Verification succeeded";

fn plain_repo(repo: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && !s.starts_with(['-', '.'])
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match repo.split_once('/') {
        Some((a, b)) => ok(a) && ok(b),
        None => false,
    }
}

/// Checks the attestation of `artifact` against the repository `repo` (`owner/name`) and the
/// commit `commit` of the run order.
///
/// Runs `gh attestation verify <file> --repo <owner/name> --source-digest <commit> --source-ref
/// refs/heads/main --signer-workflow <owner/name>/.github/workflows/release.yml`. Returns `Ok` only
/// when the tool exits with code 0 and prints its success line. Every other outcome is an error.
pub fn verify_build(
    artifact: &Path,
    repo: &str,
    commit: &str,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    if !plain_repo(repo) {
        return Err(format!(
            "repo `{repo}` is not of the form owner/name: pass the repository that builds Decant"
        ));
    }
    if !plain_commit(commit) {
        return Err("the commit of the order is not 7 to 64 lower case hex digits".into());
    }
    let file = artifact.to_string_lossy().into_owned();
    if file.is_empty() || file.starts_with('-') {
        return Err(
            "the build file name is empty or starts with a dash: pass a path to the file".into(),
        );
    }
    let workflow = format!("{repo}/{RELEASE_WORKFLOW}");
    let args: Vec<String> = [
        "attestation",
        "verify",
        file.as_str(),
        "--repo",
        repo,
        "--source-digest",
        commit,
        "--source-ref",
        SOURCE_REF,
        "--signer-workflow",
        workflow.as_str(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = match runner.run("gh", &args) {
        Ok(o) => o,
        Err(RunError::NotFound) => {
            return Err("the GitHub command line tool `gh` is not installed: install it from https://cli.github.com and run again. The build is not run".into());
        }
        Err(RunError::Failed(why)) => {
            return Err(format!(
                "the attestation check could not run ({why}): the build is not run"
            ));
        }
    };
    match out.code {
        Some(0) => {
            if out.stdout.contains(SUCCESS_MARK) || out.stderr.contains(SUCCESS_MARK) {
                Ok(())
            } else {
                Err("`gh attestation verify` exited with 0 but did not report a successful verification: update `gh` and run again. The build is not run".into())
            }
        }
        Some(code) => Err(format!(
            "the attestation of the build does not verify against {repo} for this commit, the main branch and the release workflow (gh exit code {code}): download the build of this commit again from the release page. If it fails again, do not run it"
        )),
        None => Err(
            "`gh attestation verify` was ended by a signal: run again. The build is not run".into(),
        ),
    }
}

/// Checks and unpacks the build archive of an order. Returns the fresh folder that holds it. The
/// run uses that folder and nothing else. Nothing stays behind when a step fails.
///
/// `base` is the folder under which fresh folders are made. The archive is unpacked with the
/// system `tar` (it reads `.tar`, `.tar.gz` and, where it is `bsdtar`, `.zip`), without owners.
/// Afterwards no link may be inside, so nothing points out of the folder.
pub fn prepare_build(
    archive: &Path,
    repo: &str,
    commit: &str,
    base: &Path,
    runner: &dyn CommandRunner,
) -> Result<PathBuf, String> {
    verify_build(archive, repo, commit, runner)?;
    std::fs::create_dir_all(base)
        .map_err(|e| format!("cannot make the folder for builds: {}", e.kind()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let folder = base.join(format!("build-{}-{nanos}", std::process::id()));
    // `create_dir` fails when the folder exists, so the folder is fresh.
    std::fs::create_dir(&folder)
        .map_err(|e| format!("cannot make a fresh folder for the build: {}", e.kind()))?;
    if let Err(e) = unpack_into(archive, &folder, runner) {
        let _ = std::fs::remove_dir_all(&folder);
        return Err(e);
    }
    Ok(folder)
}

fn unpack_into(archive: &Path, folder: &Path, runner: &dyn CommandRunner) -> Result<(), String> {
    let args: Vec<String> = vec![
        "-x".to_owned(),
        "--no-same-owner".to_owned(),
        "-f".to_owned(),
        archive.to_string_lossy().into_owned(),
        "-C".to_owned(),
        folder.to_string_lossy().into_owned(),
    ];
    match runner.run("tar", &args) {
        Ok(out) if out.code == Some(0) => {}
        Ok(out) => {
            return Err(format!(
                "the build archive could not be unpacked (tar exit code {}): download it again. The build is not run",
                out.code.map(|c| c.to_string()).unwrap_or("none".into())
            ));
        }
        Err(RunError::NotFound) => {
            return Err(
                "the program `tar` is not installed: it unpacks the build. The build is not run"
                    .into(),
            );
        }
        Err(RunError::Failed(why)) => {
            return Err(format!(
                "the build archive could not be unpacked ({why}). The build is not run"
            ));
        }
    }
    let read = |e: std::io::Error| format!("cannot read the unpacked build: {}", e.kind());
    let mut files = 0usize;
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(read)? {
            let entry = entry.map_err(read)?;
            let meta = std::fs::symlink_metadata(entry.path()).map_err(read)?;
            if meta.file_type().is_symlink() {
                return Err(
                    "the build archive holds a link and is refused. The build is not run".into(),
                );
            }
            files += 1;
            if files > MAX_FILES {
                return Err("the build archive holds too many files. The build is not run".into());
            }
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    if files == 0 {
        return Err("the build archive is empty. The build is not run".into());
    }
    Ok(())
}
