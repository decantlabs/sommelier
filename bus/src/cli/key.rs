//! `sommelier join` and `sommelier key show|rotate|revoke`: the key identity of a public member.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::json;

use crate::exit;
use crate::key::client;

const JOIN_HELP: &str = "\
usage: sommelier join [--handle H] [--url URL] [--data DIR] [--json]
Makes your key if you have none, solves the proof of work and joins the hub. No token and no
account are needed. Prints your member id and where the key file is. Run it again any time:
a key the hub knows gets its member id back and nothing is written.
  --handle H   display name, 1 to 32 characters: letters, digits, `.`, `-`, `_` (default: your member id)
  --url URL    hub address, default ws://127.0.0.1:7300 (env SOMMELIER_URL)
  --data DIR   client data folder that holds member.key, default: SOMMELIER_DATA, else
               %LOCALAPPDATA%\\sommelier (Windows) or ~/.local/share/sommelier
The private key never leaves the key file. The hub must have joining switched on.
";

const KEY_HELP: &str = "\
usage: sommelier key show [--json] | key rotate | key revoke --yes [--reason TEXT]   [--url URL] [--data DIR]
  show     prints your member id, your public key and the key file path (never the private key)
  rotate   makes a new key, the old key signs the change. The member id stays the same
  revoke   revokes your own key for good, a new identity needs a new key file. Needs --yes
  --url URL    hub address, default ws://127.0.0.1:7300 (env SOMMELIER_URL)
  --data DIR   client data folder that holds member.key
";

struct Flags {
    url: String,
    data: PathBuf,
    handle: Option<String>,
    reason: Option<String>,
    yes: bool,
    json: bool,
    rest: Vec<String>,
}

fn parse(args: &[String]) -> Result<Flags, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let mut f = Flags {
        url: env("SOMMELIER_URL")
            .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT)),
        data: crate::default_data_dir(),
        handle: None,
        reason: None,
        yes: false,
        json: false,
        rest: vec![],
    };
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
            "--url" => f.url = value("--url")?,
            "--data" => f.data = PathBuf::from(value("--data")?),
            "--handle" => f.handle = Some(value("--handle")?),
            "--reason" => f.reason = Some(value("--reason")?),
            "--yes" => f.yes = true,
            "--json" => f.json = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown flag {other}, see `sommelier join --help`"));
            }
            _ => f.rest.push(a.clone()),
        }
    }
    Ok(f)
}

/// The full help text of `join` or `key`.
pub fn help(cmd: &str) -> &'static str {
    if cmd == "join" { JOIN_HELP } else { KEY_HELP }
}

/// Entry point of `join` and `key`.
pub async fn run(cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", help(cmd));
        return Ok(exit::code(exit::OK));
    }
    let f = parse(args)?;
    if cmd == "join" {
        return join(&f).await;
    }
    match f.rest.first().map(String::as_str) {
        Some("show") => show(&f),
        Some("rotate") => rotate(&f).await,
        Some("revoke") => revoke(&f).await,
        _ => Err(
            "usage: sommelier key show | rotate | revoke --yes, see `sommelier key --help`".into(),
        ),
    }
}

async fn join(f: &Flags) -> Result<ExitCode, String> {
    let out = client::join(&f.url, &f.data, f.handle.as_deref()).await?;
    if f.json {
        println!(
            "{}",
            json!({"member": out.member, "key_file": out.key_path.display().to_string(),
                   "new_key": out.new_key, "registered": out.registered})
        );
        return Ok(exit::code(exit::OK));
    }
    println!("member {}", out.member);
    println!(
        "key file {} ({})",
        out.key_path.display(),
        if out.new_key {
            "made now"
        } else {
            "already there"
        }
    );
    if !out.registered {
        println!("the hub knew this key already, nothing was written");
    }
    println!("keep the key file private and back it up: a lost key is a lost identity");
    println!(
        "you can connect as tier 0 now: with no token set, every command signs in with this key"
    );
    Ok(exit::code(exit::OK))
}

fn show(f: &Flags) -> Result<ExitCode, String> {
    let path = client::key_path(&f.data);
    let file = client::load(&path)?;
    if f.json {
        println!(
            "{}",
            json!({"member": file.member, "pub": file.key.public_b64(),
                   "key_file": path.display().to_string()})
        );
    } else {
        println!("member {}", file.member);
        println!("public key {}", file.key.public_b64());
        println!("key file {}", path.display());
    }
    Ok(exit::code(exit::OK))
}

async fn rotate(f: &Flags) -> Result<ExitCode, String> {
    let public = client::rotate(&f.url, &f.data).await?;
    println!("rotated: the new public key is {public}, the member id is the same");
    println!("the old key is refused from now on, keep a backup of the new key file");
    Ok(exit::code(exit::OK))
}

async fn revoke(f: &Flags) -> Result<ExitCode, String> {
    if !f.yes {
        return Err(
            "revoking cannot be undone: your key and its standing are gone. Run again with --yes"
                .into(),
        );
    }
    let reason = f
        .reason
        .clone()
        .unwrap_or_else(|| "revoked by the member".into());
    client::revoke(&f.url, &f.data, reason.trim()).await?;
    println!("revoked: the hub refuses this key from now on");
    Ok(exit::code(exit::OK))
}
