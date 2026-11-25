//! Provenance check of a build with the GitHub command line tool.
//!
//! The client runs only builds whose origin it could verify (`docs/factory.md` section 9.2). The check
//! fails closed: anything other than a clean success is an error that says what to install or do.

use std::path::Path;

use super::cmd::{CommandRunner, RunError};

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

/// Checks the attestation of `artifact` against the repository `repo` (`owner/name`).
///
/// Runs `gh attestation verify <file> --repo <owner/name>`. Returns `Ok` only when the tool exits with
/// code 0 and prints its success line. Every other outcome is an error.
pub fn verify_build(artifact: &Path, repo: &str, runner: &dyn CommandRunner) -> Result<(), String> {
    if !plain_repo(repo) {
        return Err(format!(
            "repo `{repo}` is not of the form owner/name: pass the repository that builds Decant"
        ));
    }
    let file = artifact.to_string_lossy().into_owned();
    if file.is_empty() || file.starts_with('-') {
        return Err(
            "the build file name is empty or starts with a dash: pass a path to the file".into(),
        );
    }
    let args: Vec<String> = ["attestation", "verify", file.as_str(), "--repo", repo]
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
            "the attestation of the build does not verify against {repo} (gh exit code {code}): download the build again from the release page. If it fails again, do not run it"
        )),
        None => Err(
            "`gh attestation verify` was ended by a signal: run again. The build is not run".into(),
        ),
    }
}
