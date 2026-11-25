//! `sommelier test`: runs a test order on this machine and sends the signed report.
//!
//! The work is in [`crate::tester::round`]. This file reads the flags, finds the identity and prints
//! one line per round.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::client::Opts;
use crate::exit;
use crate::key::client as kc;
use crate::tester::profile::Profile;
use crate::tester::round::{RoundConfig, RoundOutcome, SystemEffects, run_loop, run_round};

const HELP: &str = "\
usage: sommelier test --profile FILE --title EXE --build FILE --repo OWNER/NAME
                      [--gpu TEXT] [--run-script PATH] [--loop] [--pause-file PATH] [--interval SECS]
                      [--url URL] [--data DIR] [--token-file FILE]
Asks the hub for a run order that fits this machine, runs it and sends a signed report. One round, or
with --loop rounds until the pause file appears. Prints one line per round.
  --profile FILE     the title profile (a public TOML file)
  --title EXE        your own copy of the title. It is checked against the profile first
  --build FILE       the build archive (.tar, .tar.gz or .zip), one file. It is checked with
                     `gh attestation verify` against the commit of the order, the main branch and the
                     release workflow, then unpacked into a fresh folder, and the run uses that folder
                     only. When a check fails nothing is run and nothing is sent. There is no way to
                     switch the check off
  --repo OWNER/NAME  the repository that builds and attests the build (env SOMMELIER_BUILD_REPO)
  --gpu TEXT         state your GPU when it cannot be detected, for example \"rtx 4080\"
  --run-script PATH  the run script, default swarm/run-title.sh. It is called as
                     `bash PATH <folder of the unpacked build> <seconds> <label> SOMMELIER_TITLE_EXE=...`
                     and prints `run folder: <path>` at the end
  --loop             go on while the pause file does not exist
  --pause-file PATH  default: .pause in SOMMELIER_RUNS, else in the client data folder
  --interval SECS    wait between rounds of a loop, default 300. Nothing to test waits ten times that
  --url URL          hub address, default ws://127.0.0.1:7300 (env SOMMELIER_URL)
  --data DIR         client data folder that holds member.key (as for `sommelier join`)
  --token-file FILE  sign in with a token file instead of your key
Exit code 6 means there is nothing to test now.
";

/// What a tester is told once, before the first report of this machine is sent.
pub const NOTICE: &str = "About the reports of `sommelier test`
A report holds your member id, the class of this machine (operating system and build, graphics card
model and driver, memory), the time of the run, the result of the run and a scrubbed excerpt of the
title log of up to 2048 bytes. It holds no title data, no path with a user folder and no account name.
Reports are public by design. Everybody who can read the hub, and its public matrix, can read them,
and they stay in its log. Do not run `sommelier test` if you do not want that.
";

/// The file in the data folder that says the notice was shown.
const NOTICE_SEEN: &str = ".test-notice-seen";

/// Prints [`NOTICE`] when this data folder has not shown it yet, and notes that it was shown.
fn tell_once(data: &std::path::Path) {
    let seen = data.join(NOTICE_SEEN);
    if seen.exists() {
        return;
    }
    print!("{NOTICE}");
    let _ = std::fs::create_dir_all(data);
    let _ = std::fs::write(
        &seen, b"shown
",
    );
}

/// The help text of this command.
pub fn help() -> &'static str {
    HELP
}

#[derive(Default)]
struct Flags {
    profile: Option<String>,
    title: Option<String>,
    build: Option<String>,
    repo: Option<String>,
    gpu: Option<String>,
    run_script: Option<String>,
    pause_file: Option<String>,
    interval: Option<String>,
    url: Option<String>,
    data: Option<String>,
    token_file: Option<String>,
    looping: bool,
}

fn parse(args: &[String]) -> Result<Flags, String> {
    let mut f = Flags::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (name, inline) = match a.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_owned())),
            _ => (a.as_str(), None),
        };
        let mut value = |what: &str| {
            inline
                .clone()
                .or_else(|| it.next().cloned())
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match name {
            "--profile" => f.profile = Some(value("--profile")?),
            "--title" => f.title = Some(value("--title")?),
            "--build" => f.build = Some(value("--build")?),
            "--repo" => f.repo = Some(value("--repo")?),
            "--gpu" => f.gpu = Some(value("--gpu")?),
            "--run-script" => f.run_script = Some(value("--run-script")?),
            "--pause-file" => f.pause_file = Some(value("--pause-file")?),
            "--interval" => f.interval = Some(value("--interval")?),
            "--url" => f.url = Some(value("--url")?),
            "--data" => f.data = Some(value("--data")?),
            "--token-file" => f.token_file = Some(value("--token-file")?),
            "--loop" => f.looping = true,
            other => {
                return Err(format!(
                    "unknown argument {other}, see `sommelier test --help`"
                ));
            }
        }
    }
    Ok(f)
}

fn need(v: &Option<String>, flag: &str) -> Result<String, String> {
    v.clone()
        .ok_or_else(|| format!("{flag} is required, see `sommelier test --help`"))
}

/// Entry point of `sommelier test`.
pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }
    let f = parse(args)?;
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let profile = Profile::load(&PathBuf::from(need(&f.profile, "--profile")?))?;
    let title = PathBuf::from(need(&f.title, "--title")?);
    let build = PathBuf::from(need(&f.build, "--build")?);
    let repo = f
        .repo
        .clone()
        .or_else(|| env("SOMMELIER_BUILD_REPO"))
        .ok_or("--repo is required: the repository that builds and attests the build")?;
    let url = f
        .url
        .clone()
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
    let data = f
        .data
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(crate::default_data_dir);
    let interval = match &f.interval {
        None => 300,
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| format!("--interval is a number of seconds, not `{s}`"))?,
    };

    // The identity: a token when one is given, else the key of this machine.
    let token = match (env("SOMMELIER_TOKEN"), &f.token_file) {
        (_, Some(path)) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read token file {path}: {e}"))?
            .trim()
            .to_owned(),
        (Some(t), None) => t,
        (None, None) => kc::marker_for(&kc::key_path(&data)),
    };
    let key = match kc::marker_path(&token) {
        Some(path) => Some(kc::load(&path)?.key),
        None => None,
    };
    let pause_file =
        f.pause_file
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| match env("SOMMELIER_RUNS") {
                Some(runs) => PathBuf::from(runs).join(".pause"),
                None => data.join(".pause"),
            });
    let cfg = RoundConfig {
        opts: Opts { url, token },
        key,
        profile,
        title,
        build,
        repo,
        gpu: f.gpu.clone(),
        interval_secs: interval,
    };
    tell_once(&data);
    let fx = SystemEffects {
        run_script: PathBuf::from(
            f.run_script
                .clone()
                .unwrap_or_else(|| "swarm/run-title.sh".into()),
        ),
        pause_file,
        build_root: std::env::temp_dir().join("sommelier-builds"),
    };
    if f.looping {
        run_loop(&cfg, &fx, &mut |o| println!("{}", o.line())).await?;
        return Ok(exit::code(exit::OK));
    }
    let o = run_round(&cfg, &fx).await?;
    println!("{}", o.line());
    Ok(exit::code(if o == RoundOutcome::NothingToTest {
        exit::NO_WORK
    } else {
        exit::OK
    }))
}
