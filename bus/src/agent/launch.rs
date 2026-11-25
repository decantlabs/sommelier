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

use crate::agent::sandbox::{self, ContractEnv, SandboxLevel, SandboxProbe, SandboxSpec};
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
/// `SOMMELIER_SANDBOX_RUNTIME` (podman or docker, default the first one that reports rootless mode),
/// `SOMMELIER_SANDBOX_VENDOR_DIRS` and `SOMMELIER_SANDBOX_CACHE_DIRS` (lists of `HOST=/path/in/container`,
/// separated by `;`, read only unless the entry ends with `:rw`).
///
/// At level `container` `env.worktree` must be the plain clone made by [`prepare_container_clone`]. `spawn_hook_with`
/// does that. No git folder of the member's own clone is mounted.
pub fn plan_hook_command(
    hook_script: &Path,
    env: &HookEnv,
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<String>, String> {
    plan_hook_command_with(hook_script, env, get, &sandbox::SystemProbe)
}

/// [`plan_hook_command`] with the machine behind a probe. A runtime that the member did not name is picked only
/// when it reports rootless mode: podman first, docker second. A rootful docker is never picked on its own.
pub fn plan_hook_command_with(
    hook_script: &Path,
    env: &HookEnv,
    get: &dyn Fn(&str) -> Option<String>,
    probe: &dyn SandboxProbe,
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
        None => sandbox::pick_runtime(probe).ok_or_else(|| {
            "sandbox level container needs rootless podman or docker on PATH (a runtime that does not report rootless mode is not used): install one or set SOMMELIER_SANDBOX=none"
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

/// The folder of the plain clone that one container run works in. It sits next to the output folder, outside
/// every worktree of the member, and only the container uses it. The path comes from the record the supervisor
/// keeps (`out_dir` of the task), never from a file inside a worktree.
pub fn container_clone_dir(env: &HookEnv) -> PathBuf {
    let name = env
        .out_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "run".to_owned());
    env.out_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}.clone"))
}

/// An empty folder next to the clone. It is the hooks folder and the template of every git call that touches
/// the clone, so no hook of the clone runs on the host.
fn empty_hooks_dir(env: &HookEnv) -> PathBuf {
    let mut p = container_clone_dir(env).into_os_string();
    p.push(".nohooks");
    PathBuf::from(p)
}

/// The name of one run, safe in a file name and in a ref name.
fn run_slug(env: &HookEnv) -> String {
    format!("{}-{}", env.task_id, env.attempt)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A git call on the host. Hooks are off, the file system monitor is off and no prompt is possible.
fn host_git(dir: Option<&Path>, hooks: &Path) -> Command {
    let mut c = Command::new("git");
    c.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::null());
    c.arg("-c")
        .arg(format!("core.hooksPath={}", hooks.display()))
        .args(["-c", "core.fsmonitor=false"]);
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    c
}

fn git_ok(mut c: Command, what: &str) -> Result<String, String> {
    let out = c
        .output()
        .map_err(|e| format!("{what}: cannot run git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line: String = err.trim().chars().take(200).collect();
        return Err(format!("{what} failed: {line}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Makes the folder the container works in: a PLAIN clone (its own `.git` folder inside it) of the worktree
/// of the task, made with hooks off and no template, without a remote and without any link back to the clone
/// of the member. The container never sees the git folder of the member's clone, so a hook or a
/// `core.fsmonitor` that a hostile worker writes lands in a folder that only the container uses.
/// The small files of `.sommelier` (brief, task id) are copied in. A worktree that is no git repository
/// (a role seat) gets an empty folder.
pub fn prepare_container_clone(env: &HookEnv) -> Result<PathBuf, String> {
    let clone = container_clone_dir(env);
    let hooks = empty_hooks_dir(env);
    if clone.exists() {
        fs::remove_dir_all(&clone).map_err(|e| format!("remove old container clone: {e}"))?;
    }
    fs::create_dir_all(&hooks).map_err(|e| format!("create empty hooks folder: {e}"))?;
    if let Some(parent) = clone.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create clone parent: {e}"))?;
    }
    if env.worktree.join(".git").exists() {
        let mut c = host_git(None, &hooks);
        c.args(["clone", "-q", "--no-local", "--no-tags"])
            .arg(format!("--template={}", hooks.display()))
            .arg("--")
            .arg(&env.worktree)
            .arg(&clone);
        git_ok(c, "clone for the container")?;
        let mut c = host_git(Some(&clone), &hooks);
        c.args(["remote", "remove", "origin"]);
        git_ok(c, "remove the remote of the container clone")?;
        for (k, v) in [
            ("user.name", "sommelier-worker"),
            ("user.email", "worker@sommelier.invalid"),
        ] {
            let mut c = host_git(Some(&clone), &hooks);
            c.args(["config", k, v]);
            git_ok(c, "configure the container clone")?;
        }
    } else {
        fs::create_dir_all(&clone).map_err(|e| format!("create container folder: {e}"))?;
    }
    let to = clone.join(".sommelier");
    fs::create_dir_all(&to).map_err(|e| format!("create .sommelier in the clone: {e}"))?;
    if let Ok(rd) = fs::read_dir(env.worktree.join(".sommelier")) {
        for e in rd.flatten() {
            // A plain file only, no link, nothing large.
            let plain = e.file_type().is_ok_and(|t| t.is_file());
            if plain && e.metadata().is_ok_and(|m| m.len() <= 1 << 20) {
                let _ = fs::copy(e.path(), to.join(e.file_name()));
            }
        }
    }
    Ok(clone)
}

/// Takes the result of a container run back into the worktree of the task. The supervisor reads the commit id
/// the clone stands on, fetches exactly that commit out of the clone with hooks off and object checks on,
/// keeps it under `refs/sommelier/result/<task>-<attempt>` and moves the task branch to it. Nothing of the
/// clone is run: no hook, no file system monitor, no filter of the clone's own configuration.
///
/// Returns the commit id, or `None` when the worker made no commit. Work that was not committed is not taken.
/// Call it once after the hook has exited. A clone whose `.git` is no plain folder (a worker that replaced it by a
/// pointer or a link) is refused.
pub fn fetch_container_result(env: &HookEnv) -> Result<Option<String>, String> {
    let clone = container_clone_dir(env);
    let hooks = empty_hooks_dir(env);
    if !env.worktree.join(".git").exists() {
        return Ok(None);
    }
    fs::create_dir_all(&hooks).map_err(|e| format!("create empty hooks folder: {e}"))?;
    let git_dir = clone.join(".git");
    let meta = fs::symlink_metadata(&git_dir)
        .map_err(|e| format!("the container clone has no .git folder: {e}"))?;
    if !meta.is_dir() {
        return Err("the git entry of the container clone is not a plain folder".to_owned());
    }
    let mut c = host_git(None, &hooks);
    c.arg("--git-dir")
        .arg(&git_dir)
        .args(["rev-parse", "--verify", "-q", "HEAD^{commit}"]);
    let sha = git_ok(c, "read the commit of the container clone")?;
    let hex = sha.len() == 40 || sha.len() == 64;
    if !hex
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("the commit id of the container clone is not a hash".to_owned());
    }
    let mut c = host_git(Some(&env.worktree), &hooks);
    c.args(["rev-parse", "--verify", "-q", "HEAD^{commit}"]);
    if git_ok(c, "read the start commit").is_ok_and(|start| start == sha) {
        return Ok(None);
    }
    let target = format!("refs/sommelier/result/{}", run_slug(env));
    let mut c = host_git(Some(&env.worktree), &hooks);
    c.args([
        "-c",
        "transfer.fsckObjects=true",
        "-c",
        "fetch.fsckObjects=true",
        "fetch",
        "-q",
        "--no-tags",
        "--no-recurse-submodules",
    ])
    .arg(&git_dir)
    .arg(format!("+{sha}:{target}"));
    git_ok(c, "fetch the result of the container clone")?;
    let mut c = host_git(Some(&env.worktree), &hooks);
    c.args([
        "rev-parse",
        "--verify",
        "-q",
        &format!("{target}^{{commit}}"),
    ]);
    if git_ok(c, "check the fetched result")? != sha {
        return Err("the fetched result is not the commit that was asked for".to_owned());
    }
    let mut c = host_git(Some(&env.worktree), &hooks);
    c.args(["checkout", "-q", "-f", "-B", &env.branch, &sha]);
    git_ok(c, "move the task branch to the result")?;
    Ok(Some(sha))
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

    let get = |k: &str| match (k, sandbox_level) {
        ("SOMMELIER_SANDBOX", Some(level)) => Some(level.to_owned()),
        _ => std::env::var(k).ok(),
    };
    // At level container the worker gets its own plain clone. The result comes back with
    // `fetch_container_result` after the hook has exited.
    let launch_env = if sandbox::level_from_lookup(&get)? == SandboxLevel::Container {
        HookEnv {
            worktree: prepare_container_clone(env)?,
            ..env.clone()
        }
    } else {
        env.clone()
    };
    let env = &launch_env;
    let argv = plan_hook_command(hook_script, env, &get)?;
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
