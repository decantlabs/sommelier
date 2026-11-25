# Worker sandbox

A member's machine runs an agent on a repository that other people wrote. This folder holds what sandbox
level `container` needs, and says plainly what each level does and does not protect.
The design is in `docs/factory.md` section 7.5.

`sommelier doctor` prints one line, for example `sandbox: container (can offer: container via podman, user, none)`.
The hub records the level as declared. It does not verify it.

## The three levels

Set the level with the variable `SOMMELIER_SANDBOX` (`container`, `user` or `none`). Unset means `none`.
Any other value is refused when the hook starts, with one line that says what to fix.

### container

The launcher hook runs inside a rootless container (podman first, docker second).

The worker sees:
* its own PLAIN CLONE of the project and the output folder (read and write). The supervisor makes the clone
  in a folder next to the output folder of the task (`<out folder>.clone`), with hooks off and no template,
  without a remote, and mounts it as `/work`. Only the container uses that folder.
* the prompt file and the hooks folder (read only)
* the toolchain cache folders and the vendor tool login folders that the member lists, READ ONLY unless the
  member writes `:rw` after the target
* a memory only `/tmp` and a memory only home folder

The git folder of the member's own clone is never mounted. A writable git folder would let a hostile brief or
patch write `hooks/post-checkout` or set `core.fsmonitor`, and the member's next git command outside the
container would run it. The supervisor takes the result back itself: it reads the commit id the clone stands
on, fetches exactly that commit out of the clone with hooks off and object checks on, keeps it under
`refs/sommelier/result/<task>-<attempt>` and moves the task branch to it (`launch::fetch_container_result`).
The path of the clone comes from the record of the supervisor (the output folder of the task), never from a
file inside a worktree, and a clone whose `.git` was replaced by a pointer or a link is refused. Work that the
worker did not commit does not come back, so a hook must commit what it wants to keep.

The worker does not see: the home folder of the member, the SSH agent, SSH keys, browser profiles, other
repositories, the sommelier member key and the bus tokens. The container runs with all capabilities dropped,
without the right to gain new privileges, with a read only root, and its environment is exactly the hook
contract (`SOMMELIER_TASK`, `SOMMELIER_ATTEMPT`, `SOMMELIER_BRANCH`, `SOMMELIER_WORKTREE`, `SOMMELIER_PROMPT`,
`SOMMELIER_MODEL`, `SOMMELIER_EFFORT`, `SOMMELIER_OUT`, `CARGO_BUILD_JOBS`). The supervisor holds the member key
outside the container.

What it does not protect:
* **The network is not limited.** Design 7.5 wants egress limited to the vendor, the git remote, the crate
  registry and the hub. A plain rootless runtime cannot do that, because it has no list of allowed hosts. The
  worker can reach every address the machine can reach, including the local network. A hostile patch can send
  anything the container can read to the outside. What it can read is the worktree and the mounted vendor login.
* **The vendor login is mounted and readable.** The vendor tool needs it, so a hostile build can read it and
  send it away. Use an account and a plan you can afford to lose, or an API key with a spending limit.
* **Use a login folder and a cache folder that the host never uses.** A vendor folder that the worker can write
  lets it add a hook or a command to that tool's settings, and your own tool then runs it on the host. So
  mounts are read only by default. If the tool must refresh its login, give it a DEDICATED login folder (a
  folder that only the sandbox uses, with its own login) and write `:rw` for that folder only. Never mount
  your own `~/.claude`, `~/.codex` or `~/.gemini`, and never mount your own `~/.cargo/registry` writable: a
  worker could change the extracted crate sources that your own builds use. `sommelier doctor` warns when a
  mounted vendor folder is one of the folders your own tools use, and when a writable cache folder is.
* **The clone is writable.** A hostile change can damage the clone and the mounted caches, which is the point
  of the clone: nothing of the member's own repository is writable. The cache folders are shared between
  tasks, so use a separate cache folder for this sandbox.
* The container shares the kernel of the host. A kernel flaw is a way out.
* With rootless docker or podman the mounted files must be writable by the container user. The image uses
  user id 1000 and podman maps the member's id to it with `--userns=keep-id`. Docker has no such option, so
  the ownership of mounted folders may need `chown` by hand.
* On a host with SELinux, bind mounts may be unreadable. Label the folders for the container first.
* A path with a comma or a quote cannot be mounted. `wrap` refuses it and says so.
* The clone holds every branch of the member's clone at the time it was made (a full copy of the history, no
  link back).

### user

A separate standard user account on the same machine, whose home folder holds nothing else (no SSH keys, no
browser profile, no cloud login). The supervisor runs as that account.

`sommelier doctor` checks what a program can honestly observe: the process is not elevated and the home folder
holds none of a list of well known secret folders. It cannot see whether the account is really separate from
the member's own, and it cannot see what the other accounts allow this account to read. So level `user` is a
claim of the member that doctor supports with these two checks. It never proves it.

What it protects: the files of the member's own account, by the access rules of the operating system.
What it does not protect: anything the account can read in shared places (other drives, public folders), the
network, and the vendor login of this account, which must exist in it and is readable by the worker.
Hooks are not wrapped at this level.

### none

The member's own session. Allowed, shown by doctor, not advised. The worker sees what the member sees.

## Level user on Windows by hand

Windows without WSL2 has no rootless container, so use a separate standard account.

1. Open Settings, Accounts, Other users, and add a local account (for example `decant-worker`). Choose the
   type Standard user. Do not make it an administrator.
2. Sign in once as that account so Windows creates its home folder. Leave the home folder empty. Do not copy
   SSH keys, browser profiles or cloud logins into it.
3. Install the vendor tool for that account only, and log in with the plan or the API key you chose for the
   worker.
4. Remove the right to read your own folders from that account. In the properties of your own user folder, on
   the Security tab, check that the group Users and the worker account are not listed. Do the same for every
   other drive folder that holds private data. This step is the one that gives the protection. Doctor cannot
   check it.
5. Give the worker account write access to the one folder that holds the worktrees and the sommelier data
   folder of the supervisor, and to nothing else.
6. Start the supervisor from that account (Switch user, or `runas /user:decant-worker`), with
   `SOMMELIER_SANDBOX=user` set in its environment, and run `sommelier doctor`. It must print
   `sandbox: user (...)`. If it prints `none` and a fix line, the account is elevated or its home folder holds
   a secret folder.
7. The member key stays with the supervisor of the member's own account only if the supervisor runs there.
   If the whole supervisor runs as the worker account, the key file is in that account. Keep it out of the
   worktrees and the output folders.

## Setting up level container

1. Install rootless podman (or rootless docker) and check `podman info` says `rootless: true`.
2. Build the image: `swarm/sandbox/build-image.sh`. It reads the Rust version from `bus/rust-toolchain.toml`.
   For a build you can repeat, pin the base image with a digest in `SOMMELIER_BASE_IMAGE`. Put the vendor tool
   in the image with `SOMMELIER_TOOL_PACKAGES` (for example the npm package of your tool), or build your own
   image on top of this one. The image holds no secret and no login.
3. Tell the supervisor what to mount, by environment:
   * `SOMMELIER_SANDBOX=container`
   * `SOMMELIER_SANDBOX_IMAGE` (default `sommelier-worker:latest`)
   * `SOMMELIER_SANDBOX_RUNTIME` (`podman` or `docker`, default the first one that reports rootless mode.
     A runtime that does not report rootless mode is never picked on its own. If you name one, doctor warns
     when it does not report rootless mode)
   * `SOMMELIER_SANDBOX_VENDOR_DIRS`, the login folder of each vendor tool, as `HOST=/home/worker/.claude`
     entries separated by `;`. Use a login folder that the host never uses. Read only unless you write `:rw`
     after the target
   * `SOMMELIER_SANDBOX_CACHE_DIRS`, the toolchain caches, as `HOST=/home/worker/.cargo/registry` entries
     separated by `;`. Read only unless you write `:rw` after the target, and then use a cache folder that the
     host never uses (cargo needs a writable registry to fetch, so give the sandbox its own)
4. Run `sommelier doctor`. It must print `sandbox: container (...)`.

If `SOMMELIER_SANDBOX=container` is set and no rootless runtime is found, doctor prints `sandbox: none` with a
fix line and exits with code 3, and a hook start fails with one line instead of running unprotected.

## Rules that hold at every level

A coding worker builds main plus its own change and nothing else. A review order is read only. Building
someone else's unreviewed patch (kind `reproduce`) is offered only to level `container`.

## Limits found while building this

* Only shell hooks (`hook-*.sh`) run in the container. A `.cmd` or `.bat` hook is refused at level `container`.
* A Windows host reaches level `container` only through WSL2 or a podman machine. Host paths are then passed
  to the runtime as they are, and the runtime must be able to see them.
* A supervisor that already runs inside a container is reported as level `container` by doctor, but doctor
  cannot see what that container mounts. The hook is still wrapped when `container` is requested, which needs a
  runtime inside the container.
* `home_secret_folders` looks for a short list of well known folder names. A secret in another place is not seen.
* The image pins the Rust version but not the operating system packages. Pin the base image by digest if you
  need a build you can repeat.
