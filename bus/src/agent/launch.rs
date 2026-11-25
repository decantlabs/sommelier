//! Launcher hook execution, worktree setup, and prompt assembly (sections 5.4, 8.1, 8.2, 13.3, 13.4, step 12a).
//!
//! Section 5.4 launcher hook contract:
//! The supervisor starts one worker by running the hook of its class: `hook-<vendor>.sh`.
//! Input by environment:
//! - `SOMMELIER_TASK`, `SOMMELIER_ATTEMPT`: task id and attempt number
//! - `SOMMELIER_BRANCH`, `SOMMELIER_WORKTREE`: the branch and the worktree, both already made
//! - `SOMMELIER_PROMPT`: file with guide, brief and resume note, in that order, already assembled
//! - `SOMMELIER_MODEL`, `SOMMELIER_EFFORT`: chosen by the scheduler from the class
//! - `SOMMELIER_OUT`: folder for `last.txt`, `events.jsonl`, `err.txt`
//!
//! The hook gets no bus token (`SOMMELIER_TOKEN`, `SOMMELIER_TOKEN_FILE`, `SOMMELIER_ORCH_TOKEN` are stripped).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::sandbox::{self, ContractEnv, SandboxLevel, SandboxSpec};
use crate::guide::handover;
use crate::guide::templates;
use crate::guide::{GuideRole, PolicyInfo, Vendor};

/// Resolves the vendor name (`codex`, `agy`, `claude`) for a class or model string.
pub fn vendor_for_class(class: &str, model: &str) -> &'static str {
    let c = class.trim().to_ascii_lowercase();
    let m = model.trim().to_ascii_lowercase();
    if c.contains("codex") || m.contains("codex") {
        "codex"
    } else if c.contains("claude") || m.contains("claude") {
        "claude"
    } else if c.contains("gemini") || c.contains("agy") || m.contains("gemini") || m.contains("agy")
    {
        "agy"
    } else {
        "codex"
    }
}

/// Maps a hook vendor string (`codex`, `agy`, `gemini`, `claude`) to `GuideRole` / `Vendor`.
pub fn guide_vendor(vendor: &str) -> Vendor {
    match vendor.trim().to_ascii_lowercase().as_str() {
        "claude" => Vendor::Claude,
        "codex" => Vendor::Codex,
        "agy" | "gemini" => Vendor::Gemini,
        _ => Vendor::Other,
    }
}

/// Resolves the hook script path for a given class/vendor inside `hooks_dir`
/// (or an explicit override script path).
pub fn resolve_hook_script(
    hooks_dir: &Path,
    class: &str,
    model: &str,
    override_hook: Option<&Path>,
) -> PathBuf {
    if let Some(custom) = override_hook {
        return custom.to_path_buf();
    }
    let class_hook = hooks_dir.join(format!("hook-{}.sh", class.trim().to_ascii_lowercase()));
    if class_hook.is_file() {
        return class_hook;
    }
    let v = vendor_for_class(class, model);
    hooks_dir.join(format!("hook-{v}.sh"))
}

/// Assembles the worker prompt file content in strict section 5.4 / 8.2 order:
/// 1. `guide` (from `sommelier guide --role worker --vendor <v>`)
/// 2. `brief` (task brief text)
/// 3. `resume note` (when `attempt > 1` or `resume_note` / `wip_ref` / `resume_sha` is present,
///    prefixed with a `RESUME` section per section 7.3)
pub fn assemble_worker_prompt(
    policy: &PolicyInfo,
    vendor: Vendor,
    brief_text: &str,
    attempt: u32,
    resume_note: &str,
    resume_sha: &str,
    wip_ref: &str,
) -> String {
    let guide = templates::render(GuideRole::Worker, vendor, policy);
    let mut out = String::new();
    out.push_str(guide.text.trim_end());
    out.push_str("\n\n");

    let has_resume = attempt > 1
        || !resume_note.trim().is_empty()
        || !resume_sha.trim().is_empty()
        || !wip_ref.trim().is_empty();
    if has_resume {
        out.push_str("RESUME (attempt ");
        out.push_str(&attempt.to_string());
        out.push_str(")\n");
        if !resume_sha.trim().is_empty() {
            out.push_str("COMMIT: ");
            out.push_str(resume_sha.trim());
            out.push('\n');
        }
        if !wip_ref.trim().is_empty() {
            out.push_str("WIP_REF: ");
            out.push_str(wip_ref.trim());
            out.push('\n');
        }
        if !resume_note.trim().is_empty() {
            out.push_str("DATA: ");
            out.push_str(resume_note.trim());
            out.push('\n');
        }
        out.push('\n');
    }

    if !brief_text.trim().is_empty() {
        out.push_str(brief_text.trim());
        out.push('\n');
    }

    out
}

/// Assembles the prompt for starting a role seat (section 10.3 step 12a & 13.3 & 13.4):
/// `guide for the role + takeover page (13.4)`.
pub fn assemble_role_seat_prompt(
    role_name: &str,
    vendor: Vendor,
    policy: &PolicyInfo,
    api_state: &Value,
) -> String {
    let g_role = GuideRole::parse(role_name).unwrap_or(GuideRole::Coordinator);
    let guide = templates::render(g_role, vendor, policy);
    let page = handover::build(role_name, api_state, policy);
    format!("{}\n{}", guide.text.trim_end(), page.text)
}

/// Creates or prepares a git worktree at `worktree_path` on branch `branch` from `repo_clone`.
///
/// - If `worktree_path` already exists, ensures `.sommelier` directory exists inside it.
/// - Otherwise runs `git worktree add -B <branch> <worktree_path> <base_ref>` in `repo_clone`.
/// - If `wip_ref` or `resume_sha` exists in the repository, checks out or resets appropriately.
pub fn prepare_worktree(
    repo_clone: &Path,
    worktree_path: &Path,
    branch: &str,
    base_ref: Option<&str>,
) -> Result<(), String> {
    if worktree_path.join(".git").exists() {
        let cur_branch = Command::new("git")
            .arg("-C")
            .arg(worktree_path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default();
        let branch_matches = cur_branch == branch;

        let base_matches = match base_ref.map(str::trim).filter(|s| !s.is_empty()) {
            Some(base) => {
                let expected_sha = Command::new("git")
                    .arg("-C")
                    .arg(worktree_path)
                    .args(["rev-parse", "--verify", base])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .or_else(|| {
                        Command::new("git")
                            .arg("-C")
                            .arg(repo_clone)
                            .args(["rev-parse", "--verify", base])
                            .output()
                            .ok()
                            .filter(|o| o.status.success())
                    })
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
                let cur_head = Command::new("git")
                    .arg("-C")
                    .arg(worktree_path)
                    .args(["rev-parse", "--verify", "HEAD"])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
                match (expected_sha, cur_head) {
                    (Some(exp), Some(cur)) => {
                        cur == exp
                            || cur.starts_with(&exp)
                            || exp.starts_with(&cur)
                            || Command::new("git")
                                .arg("-C")
                                .arg(worktree_path)
                                .args(["merge-base", "--is-ancestor", &exp, &cur])
                                .status()
                                .is_ok_and(|s| s.success())
                    }
                    _ => false,
                }
            }
            None => true,
        };

        if branch_matches && base_matches {
            fs::create_dir_all(worktree_path.join(".sommelier"))
                .map_err(|e| format!("create .sommelier in worktree: {e}"))?;
            return Ok(());
        }

        let target = base_ref
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("HEAD");
        let reset_ok = Command::new("git")
            .arg("-C")
            .arg(worktree_path)
            .args(["checkout", "-q", "-f", "-B", branch, target])
            .status()
            .is_ok_and(|s| s.success());
        if reset_ok {
            fs::create_dir_all(worktree_path.join(".sommelier"))
                .map_err(|e| format!("create .sommelier in worktree: {e}"))?;
            return Ok(());
        }

        let _ = Command::new("git")
            .arg("-C")
            .arg(repo_clone)
            .args(["worktree", "remove", "--force"])
            .arg(worktree_path)
            .output();
        let _ = fs::remove_dir_all(worktree_path);
    }

    if let Some(parent) = worktree_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create worktrees dir: {e}"))?;
    }

    if repo_clone.join(".git").exists() || repo_clone.join("HEAD").exists() {
        // Prune any stale worktree metadata first
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo_clone)
            .args(["worktree", "prune"])
            .output();

        // Check if branch already exists in repo_clone
        let branch_exists = Command::new("git")
            .arg("-C")
            .arg(repo_clone)
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .output()
            .is_ok_and(|o| o.status.success());

        let out = if branch_exists {
            Command::new("git")
                .arg("-C")
                .arg(repo_clone)
                .args(["worktree", "add", "-f"])
                .arg(worktree_path)
                .arg(branch)
                .output()
                .map_err(|e| format!("git worktree add failed: {e}"))?
        } else {
            let base = base_ref
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("HEAD");
            Command::new("git")
                .arg("-C")
                .arg(repo_clone)
                .args(["worktree", "add", "-f", "-B", branch])
                .arg(worktree_path)
                .arg(base)
                .output()
                .map_err(|e| format!("git worktree add -B failed: {e}"))?
        };

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "git worktree add {} on {} failed: {}",
                worktree_path.display(),
                branch,
                stderr.trim()
            ));
        }
    } else {
        // Fallback when repo_clone is a plain folder in synthetic unit tests: initialize a git repo
        fs::create_dir_all(worktree_path).map_err(|e| format!("create worktree dir: {e}"))?;
        let _ = Command::new("git")
            .arg("-C")
            .arg(worktree_path)
            .args(["init", "-q"])
            .output();
        let _ = Command::new("git")
            .arg("-C")
            .arg(worktree_path)
            .args(["checkout", "-q", "-B", branch])
            .output();
    }

    fs::create_dir_all(worktree_path.join(".sommelier"))
        .map_err(|e| format!("create .sommelier in worktree: {e}"))?;
    Ok(())
}

/// Complete environment contract for invoking `hook-<vendor>.sh` (section 5.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEnv {
    pub task_id: String,
    pub attempt: u32,
    pub branch: String,
    pub worktree: PathBuf,
    pub prompt_file: PathBuf,
    pub model: String,
    pub effort: String,
    pub out_dir: PathBuf,
}

/// The command line that runs a hook script directly on the host: today's behaviour, program first.
pub fn plain_hook_argv(hook_script: &Path) -> Vec<String> {
    let ext = hook_script
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let script = hook_script.to_string_lossy().into_owned();
    if cfg!(windows) && (ext == "cmd" || ext == "bat") {
        vec!["cmd".into(), "/C".into(), script]
    } else if ext == "sh" {
        vec!["sh".into(), script]
    } else {
        vec![script]
    }
}

/// The full command line of one hook start. Level `none` (the default) is `plain_hook_argv`. Level `container`
/// runs the hook through `sandbox::wrap`. An unknown level is refused with one clear line.
///
/// `get` reads settings: `SOMMELIER_SANDBOX` (container, user or none), `SOMMELIER_SANDBOX_IMAGE`,
/// `SOMMELIER_SANDBOX_RUNTIME` (podman or docker, default the first one found), `SOMMELIER_SANDBOX_VENDOR_DIRS`
/// and `SOMMELIER_SANDBOX_CACHE_DIRS` (lists of `HOST=/path/in/container`, separated by `;`).
pub fn plan_hook_command(
    hook_script: &Path,
    env: &HookEnv,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<String>, String> {
    let level = sandbox::level_from_lookup(get)?;
    if level != SandboxLevel::Container {
        // Level `user` is a property of the account the supervisor runs as, the hook is not wrapped.
        return Ok(plain_hook_argv(hook_script));
    }
    let ext = hook_script
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext != "sh" {
        return Err(format!(
            "sandbox level container runs shell hooks only, {} is not a .sh file",
            hook_script.display()
        ));
    }
    let name = hook_script
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "sandbox: the hook script has no file name".to_owned())?;
    let hooks_dir = hook_script
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let runtime = match get("SOMMELIER_SANDBOX_RUNTIME")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
    {
        Some(r) => r,
        None => ["podman", "docker"]
            .into_iter()
            .find(|r| sandbox::find_on_path(r).is_some())
            .map(str::to_owned)
            .ok_or_else(|| {
                "sandbox level container needs rootless podman or docker on PATH: install one or set SOMMELIER_SANDBOX=none"
                    .to_owned()
            })?,
    };
    let mounts = |key: &str| -> Result<Vec<sandbox::Mount>, String> {
        sandbox::parse_mounts(&get(key).unwrap_or_default())
    };
    let spec = SandboxSpec {
        runtime,
        image: get("SOMMELIER_SANDBOX_IMAGE")
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| sandbox::DEFAULT_IMAGE.to_owned()),
        worktree: env.worktree.clone(),
        out_dir: env.out_dir.clone(),
        prompt_file: env.prompt_file.clone(),
        hooks_dir,
        git_dir: worktree_git_dir(&env.worktree),
        cache_dirs: mounts("SOMMELIER_SANDBOX_CACHE_DIRS")?,
        vendor_dirs: mounts("SOMMELIER_SANDBOX_VENDOR_DIRS")?,
        env: ContractEnv {
            task: env.task_id.clone(),
            attempt: env.attempt,
            branch: env.branch.clone(),
            model: env.model.clone(),
            effort: env.effort.clone(),
            cargo_build_jobs: "4".into(),
        },
    };
    sandbox::wrap(
        &["sh".to_owned(), format!("{}/{name}", sandbox::HOOKS_DIR)],
        &spec,
    )
}

/// The common git folder a linked worktree points to, or `None` when `.git` is a folder or unreadable.
fn worktree_git_dir(worktree: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(worktree.join(".git")).ok()?;
    let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    // `<clone>/.git/worktrees/<name>` -> `<clone>/.git`
    let common = gitdir.parent()?.parent()?;
    Some(common.to_path_buf())
}

/// Spawns the launcher hook script in the foreground with the exact environment variables
/// of section 5.4 and with all bus token variables removed.
pub fn spawn_hook(hook_script: &Path, env: &HookEnv) -> Result<Child, String> {
    spawn_hook_with(hook_script, env, None)
}

/// [`spawn_hook`] with a sandbox level from the supervisor config. `Some` wins over `SOMMELIER_SANDBOX`.
pub fn spawn_hook_with(
    hook_script: &Path,
    env: &HookEnv,
    sandbox_level: Option<&str>,
) -> Result<Child, String> {
    if !hook_script.is_file() {
        return Err(format!("hook script not found: {}", hook_script.display()));
    }
    fs::create_dir_all(&env.out_dir)
        .map_err(|e| format!("create SOMMELIER_OUT {}: {e}", env.out_dir.display()))?;
    if let Some(parent) = env.prompt_file.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create prompt dir: {e}"))?;
    }

    // Remove stale output files from previous runs in the same out_dir
    for name in ["last.txt", "err.txt", "events.jsonl", "handover"] {
        let _ = fs::remove_file(env.out_dir.join(name));
    }

    let argv = plan_hook_command(hook_script, env, &|k| match (k, sandbox_level) {
        ("SOMMELIER_SANDBOX", Some(level)) => Some(level.to_owned()),
        _ => std::env::var(k).ok(),
    })?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);

    if env.worktree.is_dir() {
        cmd.current_dir(&env.worktree);
    }

    // Section 5.4 environment contract:
    cmd.env("SOMMELIER_TASK", &env.task_id)
        .env("SOMMELIER_ATTEMPT", env.attempt.to_string())
        .env("SOMMELIER_BRANCH", &env.branch)
        .env("SOMMELIER_WORKTREE", &env.worktree)
        .env("SOMMELIER_PROMPT", &env.prompt_file)
        .env("SOMMELIER_MODEL", &env.model)
        .env("SOMMELIER_EFFORT", &env.effort)
        .env("SOMMELIER_OUT", &env.out_dir)
        .env("CARGO_BUILD_JOBS", "4");

    // Section 5.4: "The hook gets no bus token. A worker never talks to the bus with the orchestrator's identity."
    cmd.env_remove("SOMMELIER_TOKEN")
        .env_remove("SOMMELIER_TOKEN_FILE")
        .env_remove("SOMMELIER_ORCH_TOKEN");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    cmd.spawn()
        .map_err(|e| format!("cannot spawn hook {}: {e}", hook_script.display()))
}
