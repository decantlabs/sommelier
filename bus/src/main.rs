//! The `sommelier` command line: hub, token admin and clients in one binary.

mod dispatch;

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};
use sommelier::client::{self, Opts};
use sommelier::hub::{self, HubConfig};
use sommelier::msg::{Kind, Message, Role, format_line, parse_names};
use sommelier::policy::{Change, Mode, Snapshot, Store};
use sommelier::proto::{ConnectParams, FilterSpec};
use sommelier::tokens::TokenFile;

const HELP: &str = "\
sommelier: coordination bus for coding agents on several machines
details for one command: sommelier help <command>  or  sommelier <command> --help

HUB OWNER
  sommelier serve [--listen 127.0.0.1:7300] [--data DIR] [--rate 2] [--burst 30]
               [--policy PATH] [--claim-ttl MINUTES] [--max-body BYTES] [--kind-limit KIND=CHARS] [--quiet]
  sommelier token add NAME [--role member|orchestrator|observer] [--data DIR]
  sommelier token rotate NAME [--data DIR]
  sommelier token revoke NAME [--data DIR]
  sommelier token list [--data DIR]
  sommelier policy publish FILE | policy rollback VERSION [--data DIR]

CLIENTS (need --url and --token, or SOMMELIER_URL and SOMMELIER_TOKEN)
  sommelier send KIND [BODY...] [--topic T] [--ref R]... [--re ID]... [--to NAME]...
                 body from the arguments, or from stdin with BODY `-` or --stdin
                 (stdin lines are joined with ` | `)
  sommelier tail [--since ID] [--last N] [--kind K,K] [--topic T|PREFIX*] [--from A,B] [--to me]
              [--json] [--max N] [--once]
  sommelier log  [--since ID] [--last N] [--limit N] [--kind ..] [--topic ..] [--from ..] [--to ..] [--json]
  sommelier policy show [--json]
  sommelier sync [--data DIR]
  sommelier claims [--json] | claims --check TOPIC [--files GLOB,GLOB --board PATH]
  sommelier who | whoami | heartbeat
  sommelier status set AGENT [--state S] [--model M] [--branch B] TEXT...
  sommelier status clear AGENT | list [--owner NAME|me] | sync --file FILE

GUIDE, DOCTOR & HANDOVER
  sommelier guide [--role worker|lead|coordinator|brain|guest-lead] [--vendor claude|codex|gemini|other] [--format md|json]
  sommelier guide --loop [--role R] [--shell sh|ps] | guide --oneshot <situation> [--json]
  sommelier doctor [--data DIR] [--policy FILE] [--hooks-dir DIR] [--json]
  sommelier handover --role <role> [--data DIR] [--state-file FILE] [--json]

ALERTS & EXCEPTIONS
  sommelier alerts [--open] [--code C1,C2] [--wait SECS] [--limit N] [--json]
  sommelier alert ack <KEY> [--epoch N] [--who WHO] [--role R] [--json]

BACKLOG, WORK & LANDING
  sommelier task add --kind K --title T --scope G --done-when D --brief FILE [--priority P] [--after ID] [--json]
  sommelier task list [--state S] [--mine] | show ID | cancel ID | requeue ID | import-board FILE [--json]
  sommelier next [--wait SECS] [--class C] [--peek] | work ID | checkpoint ID --file F | done ID ... | stuck ID --why TEXT
  sommelier decline ID --why TEXT | ask --task ID TEXT | offers [--top N]
  sommelier land enqueue REF SHA | next | result BATCH --result R | approve REF SHA | list | sweep [--repo DIR]
  sommelier scan REF [--repo DIR] [--base SHA]
  sommelier run request --sha S | register | result RUN --outcome O | list | sweep --dir DIR
  sommelier metric add FILE | compare [SHA]

QUOTA, SCHEDULER & SUPERVISOR
  sommelier quota show | probe codex|claude|gemini | ingest claude-statusline | set POOL WINDOW PCT --resets TIME
  sommelier quota hit --vendor V --text-file F [--task ID] | calibrate | wait-secs [--pool P]
  sommelier plan [--dry] [--explain ID] [--policy FILE] [--json]
  sommelier sim <LOG> [--policy FILE] | idle [--json] | pause <SCOPE> | resume <SCOPE>
  sommelier agent run [--config FILE] | exec --name N --class C --branch B -- COMMAND... | status | stop [NAME]

ROLES & HIGH AVAILABILITY
  sommelier role list | claim R | release R | handover R --note-file F | check R [--epoch N] | veto WHO
  sommelier standby --of <URL> [--listen ADDR] [--data DIR] [--epoch N]
  sommelier promote [--confirm CODE | --handback] [--primary URL] [--witness URL_OR_FILE]
  sommelier drill <coordinator-dies|hub-dies|quota-drain|flapping> [--data DIR] [--json]
  sommelier snapshot [--data DIR] [--out FILE] | replay [--verify] [--data DIR]

  --url URL          default ws://127.0.0.1:7300, use wss://... behind a tunnel
  --token TOKEN      or --token-file PATH, or SOMMELIER_TOKEN / SOMMELIER_TOKEN_FILE

LOCAL VALIDATION
  sommelier check-brief FILE --branch BRANCH --task NAME
  sommelier check-result FILE --branch BRANCH --task NAME [--remote-sha FILE]

KINDS
  READY LANDED FIX STUCK FINDING CLAIM RELEASE TASK ACCEPT DECLINE
  RUN_REQUEST RUN_RESULT HELLO CHAT STATUS OFFER TASK_UPDATE POLICY ALERT FACT
";

struct Args {
    pos: Vec<String>,
    vals: HashMap<String, Vec<String>>,
    bools: Vec<String>,
}

impl Args {
    fn parse(raw: &[String], value_flags: &[&str], bool_flags: &[&str]) -> Result<Args, String> {
        let mut a = Args {
            pos: Vec::new(),
            vals: HashMap::new(),
            bools: Vec::new(),
        };
        let mut it = raw.iter();
        while let Some(s) = it.next() {
            if s == "--" {
                a.pos.extend(it.by_ref().cloned());
                break;
            }
            let Some(flag) = s.strip_prefix("--") else {
                a.pos.push(s.clone());
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (flag, None),
            };
            if bool_flags.contains(&name) {
                a.bools.push(name.into());
            } else if value_flags.contains(&name) {
                let v = match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .cloned()
                        .ok_or_else(|| format!("--{name} needs a value"))?,
                };
                a.vals.entry(name.into()).or_default().push(v);
            } else {
                return Err(format!("unknown flag --{name}, see `sommelier help`"));
            }
        }
        Ok(a)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.vals
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.vals.get(name).cloned().unwrap_or_default()
    }

    fn flag(&self, name: &str) -> bool {
        self.bools.iter().any(|b| b == name)
    }

    fn num<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, String> {
        self.get(name)
            .map(|v| {
                v.parse()
                    .map_err(|_| format!("--{name} needs a number, got `{v}`"))
            })
            .transpose()
    }
}

const CLIENT_FLAGS: [&str; 3] = ["url", "token", "token-file"];

fn client_opts(a: &Args) -> Result<Opts, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = a
        .get("url")
        .map(str::to_owned)
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", sommelier::DEFAULT_PORT));
    Ok(Opts {
        url,
        token: client_token(a)?,
    })
}

fn client_token(a: &Args) -> Result<String, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let token = match (
        a.get("token")
            .map(str::to_owned)
            .or_else(|| env("SOMMELIER_TOKEN")),
        a.get("token-file"),
    ) {
        (Some(t), _) => t,
        (None, file) => {
            let path = file.map(str::to_owned).or_else(|| env("SOMMELIER_TOKEN_FILE")).ok_or(
                "no token: pass --token, --token-file, or set SOMMELIER_TOKEN (the hub owner issues it with `sommelier token add`)",
            )?;
            std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read token file {path}: {e}"))?
                .trim()
                .to_owned()
        }
    };
    Ok(token)
}

/// `--data`, else the shared default, so `serve` and `token` always meet in one folder.
fn data_dir(a: &Args) -> PathBuf {
    a.get("data")
        .map(PathBuf::from)
        .unwrap_or_else(sommelier::default_data_dir)
}

/// The dashboard folder when `--ui` is not given: `ui` next to the executable, or
/// `ui` two levels up (the project folder, for `target/release/sommelier`).
fn find_ui() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join("ui"), dir.join("..").join("..").join("ui")]
        .into_iter()
        .find(|d| d.join("index.html").is_file())
}

/// Without `--kind` STATUS is left out (agent rows would flood the stream), unless `--all`.
fn filter_spec(a: &Args) -> Result<FilterSpec, String> {
    let mut kinds = a
        .all("kind")
        .iter()
        .map(|k| Kind::parse_list(k))
        .collect::<Result<Vec<_>, _>>()?
        .concat();
    if kinds.is_empty() && !a.flag("all") {
        kinds = Kind::ALL
            .into_iter()
            .filter(|k| *k != Kind::Status)
            .collect();
    }
    Ok(FilterSpec {
        kinds,
        topic: a.get("topic").map(str::to_owned),
        from: a.all("from").iter().flat_map(|f| parse_names(f)).collect(),
        to: a.get("to").map(str::to_owned),
        worker: a.get("worker").map(str::to_owned),
    })
}

fn print_msg(m: &Message, raw: &str, json: bool) {
    if json {
        println!("{raw}");
    } else {
        println!("{}", format_line(m));
    }
}

fn with(base: &[&'static str], more: &[&'static str]) -> Vec<&'static str> {
    base.iter().chain(more).copied().collect()
}

async fn cmd_serve(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(
        raw,
        &[
            "listen",
            "policy",
            "data",
            "rate",
            "burst",
            "claim-ttl",
            "max-body",
            "kind-limit",
            "ui",
        ],
        &["quiet", "no-ui"],
    )?;
    let listen = a
        .get("listen")
        .map(str::to_owned)
        .unwrap_or_else(|| format!("127.0.0.1:{}", sommelier::DEFAULT_PORT));
    let mut cfg = HubConfig::new(data_dir(&a));
    cfg.policy_path = a.get("policy").map(PathBuf::from);
    if let Some(r) = a.num::<f64>("rate")? {
        cfg.rate_per_sec = r.max(0.01);
    }
    if let Some(b) = a.num::<f64>("burst")? {
        cfg.burst = b.max(1.0);
    }
    if let Some(m) = a.num::<u64>("claim-ttl")? {
        cfg.claim_ttl = Duration::from_secs(m.max(1) * 60);
    }
    if let Some(b) = a.num::<usize>("max-body")? {
        cfg.limits.max_body = b.min(cfg.limits.max_frame - 512);
    }
    for spec in a.all("kind-limit") {
        for (kind, n) in sommelier::msg::parse_kind_limits(&spec)? {
            cfg.limits.kind_chars.insert(kind, n);
        }
    }
    cfg.verbose = !a.flag("quiet");
    cfg.ui_dir = if a.flag("no-ui") {
        None
    } else {
        match a.get("ui") {
            Some(d) => {
                let d = PathBuf::from(d);
                if !d.join("index.html").is_file() {
                    return Err(format!("--ui {}: no index.html there", d.display()));
                }
                Some(d)
            }
            None => find_ui(),
        }
    };
    let data = cfg.data_dir.clone();
    let ui = cfg.ui_dir.clone();
    let hub = hub::Hub::open(cfg).map_err(|e| format!("cannot open {}: {e}", data.display()))?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("cannot listen on {listen}: {e}"))?;
    let members = TokenFile::load(&data.join("tokens.json"))
        .map(|t| t.members.len())
        .unwrap_or(0);
    eprintln!(
        "sommelier hub on ws://{} (data in {}, {members} members, last message #{})",
        listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or(listen),
        data.display(),
        hub.last_id()
    );
    match &ui {
        Some(d) => eprintln!("dashboard from {}", d.display()),
        None => eprintln!("no dashboard: no ui folder next to the executable, see --ui"),
    }
    if members == 0 {
        eprintln!("no tokens yet: sommelier token add <name> --role orchestrator");
    }
    let h = hub.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        eprintln!("sommelier: shutting down, closing connections");
        h.shutdown();
    });
    hub::serve(hub, listener).await.map_err(|e| e.to_string())?;
    // Let the connections send their close frames (1012, reconnect later). Every
    // message is already on disk: the log is written before it is broadcast.
    tokio::time::sleep(Duration::from_millis(500)).await;
    eprintln!("sommelier: stopped");
    Ok(())
}

/// Ctrl+C everywhere, and SIGTERM on Unix (systemd, docker stop).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn cmd_token(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw, &["data", "role"], &[])?;
    let dir = data_dir(&a);
    let path = dir.join("tokens.json");
    eprintln!("token file {}", path.display());
    let mut t = TokenFile::load(&path).map_err(|e| e.to_string())?;
    let save = |t: &TokenFile| {
        t.save(&path)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    };
    if t.converted > 0 {
        save(&t)?;
        eprintln!(
            "converted {} plain tokens to salted hashes, the tokens themselves still work",
            t.converted
        );
    }
    match a
        .pos
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["add", name] => {
            let role_s = a.get("role").unwrap_or("member");
            let role = Role::parse_issuable(role_s).ok_or_else(|| {
                format!("unknown role `{role_s}`: orchestrator, member or observer")
            })?;
            let token = t.add(name, role)?;
            save(&t)?;
            eprintln!(
                "issued a {} token for {}, shown only now, hand it over privately:",
                role.as_str(),
                name.to_ascii_lowercase()
            );
            println!("{token}");
        }
        ["rotate", name] => {
            let (token, role) = t.rotate(name)?;
            save(&t)?;
            eprintln!(
                "new {} token for {}, the old one stops working within seconds, shown only now:",
                role.as_str(),
                name.to_ascii_lowercase()
            );
            println!("{token}");
        }
        ["revoke", name] => {
            if !t.revoke(name) {
                return Err(format!("no token for `{name}`"));
            }
            save(&t)?;
            eprintln!("revoked {name}, a running hub cuts its connections within seconds");
        }
        ["list"] => {
            for m in &t.members {
                println!("{:<20} {:<13} {}", m.name, m.role.as_str(), m.created);
            }
        }
        _ => {
            return Err(
                "usage: sommelier token add NAME [--role R] | rotate NAME | revoke NAME | list"
                    .into(),
            );
        }
    }
    Ok(())
}

async fn cmd_policy(raw: &[String]) -> Result<(), String> {
    let (command, rest) = raw.split_first().ok_or("see sommelier help policy")?;
    match command.as_str() {
        "show" => {
            let a = Args::parse(rest, &with(&CLIENT_FLAGS, &["data"]), &["json"])?;
            if !a.pos.is_empty() {
                return Err("see sommelier help policy".into());
            }
            let snapshot = if let Some(dir) = a.get("data") {
                if CLIENT_FLAGS.iter().any(|flag| a.get(flag).is_some()) {
                    return Err("use either --data or connection options for policy show".into());
                }
                Store::new(std::path::Path::new(dir))
                    .current()
                    .map_err(|e| e.to_string())?
            } else {
                client::fetch_policy(&client_opts(&a)?).await?
            };
            if a.flag("json") {
                println!("{}", snapshot.summary());
            } else {
                let summary = snapshot.summary();
                println!(
                    "version {} mode {} hash {}",
                    snapshot.policy.version,
                    summary["mode"].as_str().unwrap_or_default(),
                    snapshot.hash
                );
            }
        }
        "publish" | "rollback" => {
            let a = Args::parse(rest, &["data", "token", "token-file"], &[])?;
            let [argument] = a.pos.as_slice() else {
                return Err("see sommelier help policy".into());
            };
            let change = if command == "publish" {
                Change::Publish(std::path::Path::new(argument))
            } else {
                Change::Rollback(argument.parse().map_err(|_| "invalid policy version")?)
            };
            let snapshot = Store::publish(&data_dir(&a), &client_token(&a)?, change)
                .map_err(|e| e.to_string())?;
            println!("{}", snapshot.summary());
        }
        _ => return Err("see sommelier help policy".into()),
    }
    Ok(())
}

async fn cmd_sync(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw, &with(&CLIENT_FLAGS, &["data"]), &[])?;
    if !a.pos.is_empty() {
        return Err("see sommelier help sync".into());
    }
    let snapshot = client::fetch_policy(&client_opts(&a)?).await?;
    snapshot
        .write_cache(&data_dir(&a))
        .map_err(|e| e.to_string())?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())?;
    if current.cmp_precedence(&snapshot.policy.min_client).is_lt() {
        let message = format!(
            "client {current} is older than policy minimum {}",
            snapshot.policy.min_client
        );
        if snapshot.policy.mode == Mode::Enforce {
            return Err(message);
        }
        eprintln!("warning: {message}");
    }
    println!("{}", snapshot.summary());
    Ok(())
}

async fn cmd_send(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(
        raw,
        &with(
            &CLIENT_FLAGS,
            &["topic", "ref", "re", "to", "data", "policy-hash"],
        ),
        &["stdin"],
    )?;
    let opts = client_opts(&a)?;
    let (kind_s, words) = a
        .pos
        .split_first()
        .ok_or("usage: sommelier send KIND [BODY...] [--topic T] [--ref R]")?;
    let kind = Kind::parse(kind_s)
        .ok_or_else(|| format!("unknown kind `{kind_s}`, known kinds: {}", Kind::list()))?;
    // stdin is read only when asked for: an agent harness may hand over a stdin that
    // never ends, and reading it would hang the agent.
    let from_stdin = a.flag("stdin") || words == ["-"];
    if a.flag("stdin") && !words.is_empty() {
        return Err("give the body either as arguments or with --stdin, not both".into());
    }
    if words.is_empty()
        && !from_stdin
        && !matches!(kind, Kind::Claim | Kind::Release | Kind::Accept)
    {
        return Err(format!(
            "no body for {}: give it as arguments, or pass `-` (or --stdin) to read it from stdin, for example `... | sommelier send {} -`",
            kind.as_str(),
            kind.as_str()
        ));
    }
    let body = if from_stdin {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("reading stdin: {e}"))?;
        // Windows PowerShell pipes a byte order mark in front of the text.
        s.trim_start_matches('\u{feff}')
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" | ")
    } else {
        words.join(" ")
    };
    let mut refs = a.all("ref");
    refs.extend(
        a.all("re")
            .iter()
            .map(|r| format!("#{}", r.trim_start_matches('#'))),
    );
    let to: Vec<String> = a.all("to").iter().flat_map(|t| parse_names(t)).collect();
    let mut req = json!({"op": "send", "kind": kind.as_str(), "topic": a.get("topic").unwrap_or(""), "body": body, "refs": refs});
    if !to.is_empty() {
        req["to"] = json!(to);
    }
    if kind == Kind::Claim {
        let hash = match a.get("policy-hash") {
            Some(hash) => Some(hash.to_owned()),
            None => Snapshot::cached_hash(&data_dir(&a))
                .map_err(|e| format!("cannot read cached policy ({e}), run sommelier sync"))?,
        };
        if let Some(hash) = hash {
            req["policy_hash"] = json!(hash);
        }
    }
    let v = client::request_ok(&opts, req).await?;
    if let Some(warning) = v["warning"].as_str() {
        eprintln!("warning: {warning}");
    }
    println!("#{}", v["id"]);
    Ok(())
}

async fn cmd_tail(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(
        raw,
        &with(
            &CLIENT_FLAGS,
            &[
                "since", "last", "kind", "topic", "from", "to", "worker", "max",
            ],
        ),
        &["json", "once", "all"],
    )?;
    let opts = client_opts(&a)?;
    let params = ConnectParams {
        token: None,
        since: a.num("since")?,
        last: a.num("last")?,
        live: true,
        filter: filter_spec(&a)?,
    };
    let max: Option<usize> = a.num("max")?;
    let json = a.flag("json");
    let mut n = 0;
    client::tail(&opts, params, a.flag("once"), |m, raw| {
        print_msg(m, raw, json);
        n += 1;
        max.is_none_or(|x| n < x)
    })
    .await
}

async fn cmd_log(raw: &[String]) -> Result<(), String> {
    let a = Args::parse(
        raw,
        &with(
            &CLIENT_FLAGS,
            &[
                "since", "last", "limit", "kind", "topic", "from", "to", "worker",
            ],
        ),
        &["json", "all"],
    )?;
    let opts = client_opts(&a)?;
    let f = filter_spec(&a)?;
    let kinds: Vec<_> = f.kinds.iter().map(|k| k.as_str()).collect();
    let req = json!({"op": "log", "since": a.num::<u64>("since")?, "last": a.num::<u64>("last")?,
                     "limit": a.num::<u64>("limit")?, "kinds": kinds, "topic": f.topic, "from": f.from, "to": f.to, "worker": f.worker});
    let v = client::request_ok(&opts, req).await?;
    for m in v["messages"].as_array().into_iter().flatten() {
        if let Ok(msg) = serde_json::from_value::<Message>(m.clone()) {
            // Reserialize from the struct to keep the wire field order.
            let raw = serde_json::to_string(&msg).unwrap_or_default();
            print_msg(&msg, &raw, a.flag("json"));
        }
    }
    Ok(())
}

/// One agent row for STATUS.
struct Row {
    agent: String,
    state: String,
    model: String,
    branch: String,
    text: String,
}

impl Row {
    fn request(&self) -> Value {
        let mut refs = vec![format!("state:{}", self.state)];
        if !self.model.is_empty() {
            refs.push(format!("model:{}", self.model));
        }
        if !self.branch.is_empty() {
            refs.push(format!("branch:{}", self.branch));
        }
        json!({"op": "send", "kind": "STATUS", "topic": format!("agent/{}", self.agent), "body": self.text, "refs": refs})
    }

    fn same_as(&self, v: &Value) -> bool {
        let s = |k: &str| v[k].as_str().unwrap_or("");
        s("state") == self.state
            && s("model") == self.model
            && s("branch") == self.branch
            && s("text") == self.text
    }
}

/// TSV rows: agent, state, model, branch, text. Empty lines and `#` lines are skipped.
fn parse_tsv(text: &str) -> Result<Vec<Row>, String> {
    let mut rows: Vec<Row> = Vec::new();
    for (n, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
        let col = |i: usize| cols.get(i).copied().unwrap_or("").to_owned();
        let agent = col(0);
        if agent.is_empty() || agent.contains(char::is_whitespace) {
            return Err(format!(
                "line {}: the first column must be an agent name without spaces",
                n + 1
            ));
        }
        if rows.iter().any(|r| r.agent == agent) {
            return Err(format!("line {}: agent `{agent}` appears twice", n + 1));
        }
        let state = Some(col(1))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "running".into());
        rows.push(Row {
            agent,
            state,
            model: col(2),
            branch: col(3),
            text: cols.get(4..).map(|c| c.join(" ")).unwrap_or_default(),
        });
    }
    Ok(rows)
}

/// Rows younger than this are not sent again by `status sync` when nothing changed.
const REFRESH_MS: u64 = 10 * 60 * 1000;

async fn cmd_status(raw: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: sommelier status set AGENT [--state S] [--model M] [--branch B] TEXT... | clear AGENT | list [--owner NAME|me] | sync --file FILE";
    let a = Args::parse(
        raw,
        &with(
            &CLIENT_FLAGS,
            &["state", "model", "branch", "file", "owner"],
        ),
        &["json"],
    )?;
    let opts = client_opts(&a)?;
    let (sub, rest) = a.pos.split_first().ok_or(USAGE)?;
    let mut c = client::Conn::open(&opts).await?;
    match (sub.as_str(), rest) {
        ("set", [agent, words @ ..]) => {
            let row = Row {
                agent: agent.clone(),
                state: a.get("state").unwrap_or("running").to_owned(),
                model: a.get("model").unwrap_or("").to_owned(),
                branch: a.get("branch").unwrap_or("").to_owned(),
                text: words.join(" "),
            };
            let v = c.call_ok(&row.request()).await?;
            println!("#{}", v["id"]);
        }
        ("clear", [agent]) => {
            let req = json!({"op": "send", "kind": "STATUS", "topic": format!("agent/{agent}"), "body": "cleared", "refs": ["state:done"]});
            let v = c.call_ok(&req).await?;
            println!("#{}", v["id"]);
        }
        ("list", []) => {
            let v = c
                .call_ok(&json!({"op": "roster", "owner": a.get("owner").unwrap_or("")}))
                .await?;
            for r in v["agents"].as_array().into_iter().flatten() {
                if a.flag("json") {
                    println!("{r}");
                    continue;
                }
                let s = |k: &str| r[k].as_str().unwrap_or("").to_owned();
                let stale = if r["stale"].as_bool() == Some(true) {
                    " stale"
                } else {
                    ""
                };
                println!(
                    "{}/{} {}{stale} {} {} #{} {} : {}",
                    s("owner"),
                    s("agent"),
                    s("state"),
                    Some(s("model"))
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| "-".into()),
                    Some(s("branch"))
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| "-".into()),
                    r["id"],
                    sommelier::time::clock(&s("ts")),
                    s("text")
                );
            }
        }
        ("sync", []) => {
            let file = a
                .get("file")
                .ok_or("status sync needs --file FILE (or --file - for stdin)")?;
            let text = if file == "-" {
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .map_err(|e| format!("reading stdin: {e}"))?;
                s
            } else {
                std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))?
            };
            let rows = parse_tsv(&text)?;
            let cur = c.call_ok(&json!({"op": "roster", "owner": "me"})).await?;
            let cur = cur["agents"].as_array().cloned().unwrap_or_default();
            let now = sommelier::time::now_ms();
            let (mut updated, mut cleared, mut kept) = (0, 0, 0);
            for row in &rows {
                let fresh = cur
                    .iter()
                    .find(|v| v["agent"].as_str() == Some(row.agent.as_str()))
                    .is_some_and(|v| {
                        let age = v["ts"]
                            .as_str()
                            .and_then(sommelier::time::parse_ms)
                            .map_or(u64::MAX, |t| now.saturating_sub(t));
                        row.same_as(v) && age < REFRESH_MS
                    });
                if fresh {
                    kept += 1;
                } else {
                    c.call_ok(&row.request()).await?;
                    updated += 1;
                }
            }
            for v in &cur {
                let agent = v["agent"].as_str().unwrap_or("");
                if !rows.iter().any(|r| r.agent == agent) {
                    let req = json!({"op": "send", "kind": "STATUS", "topic": format!("agent/{agent}"), "body": "cleared by sync", "refs": ["state:done"]});
                    c.call_ok(&req).await?;
                    cleared += 1;
                }
            }
            println!("sync: {updated} updated, {cleared} cleared, {kept} unchanged");
        }
        _ => return Err(USAGE.into()),
    }
    c.close().await;
    Ok(())
}

fn dispatch_exit(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("sommelier: {reason}");
            ExitCode::from(3)
        }
    }
}

fn cmd_validate(cmd: &str, raw: &[String]) -> Result<ExitCode, String> {
    let flags: &[_] = if cmd == "check-result" {
        &["branch", "task", "remote-sha"]
    } else {
        &["branch", "task"]
    };
    let a = Args::parse(raw, flags, &[])?;
    let [path] = a.pos.as_slice() else {
        return Err(format!("Expected one input file. See sommelier help {cmd}"));
    };
    let branch = a
        .get("branch")
        .filter(|v| !v.trim().is_empty())
        .ok_or("Expected --branch")?;
    let task = a
        .get("task")
        .filter(|v| !v.trim().is_empty())
        .ok_or("Expected --task")?;
    Ok(dispatch_exit(dispatch::read(path).and_then(|text| {
        if cmd == "check-result" {
            dispatch::check_result(&text, branch, task, a.get("remote-sha"))
        } else {
            dispatch::check_brief(&text, branch, task)
        }
    })))
}

async fn cmd_claims(raw: &[String]) -> Result<ExitCode, String> {
    let a = Args::parse(
        raw,
        &with(&CLIENT_FLAGS, &["check", "files", "board"]),
        &["json"],
    )?;
    let topic = a.get("check");
    let files = match (topic, a.get("files"), a.get("board")) {
        (Some(_), Some(files), Some(board)) => Some((files, board)),
        (_, None, None) => None,
        _ => return Err("--files and --board must be used together with --check".into()),
    };
    if !a.pos.is_empty() || topic == Some("") || (topic.is_some() && a.flag("json")) {
        return Err("Invalid claims arguments. See sommelier help claims".into());
    }
    let mut reply = client::request_ok(&client_opts(&a)?, json!({"op": "claims"})).await?;
    let claims = reply["claims"]
        .as_array_mut()
        .ok_or("Missing claims in hub reply")?;
    if let Some(topic) = topic {
        for claim in claims {
            let held = claim["topic"]
                .as_str()
                .ok_or("Missing claim topic in hub reply")?;
            if dispatch::prefix_overlaps(topic, held) {
                let holder = claim["holder"]
                    .as_str()
                    .ok_or("Missing claim holder in hub reply")?;
                return Ok(dispatch_exit(Err(format!("{held} is held by {holder}"))));
            }
        }
        return Ok(dispatch_exit(files.map_or(Ok(()), |(files, board)| {
            dispatch::check_board(board, files)
        })));
    }

    let now = sommelier::time::now_ms();
    for claim in claims.iter_mut() {
        let since = claim["since_ts"]
            .as_str()
            .and_then(sommelier::time::parse_ms)
            .ok_or("Invalid claim timestamp in hub reply")?;
        claim["age_secs"] = json!(now.saturating_sub(since) / 1000);
    }
    if a.flag("json") {
        println!("{reply}");
    } else if claims.is_empty() {
        println!("no claims");
    } else {
        for claim in claims {
            println!(
                "{} {} since #{} {} age {} s, idle {} min",
                claim["topic"].as_str().unwrap_or(""),
                claim["holder"].as_str().unwrap_or(""),
                claim["since_id"],
                sommelier::time::clock(claim["since_ts"].as_str().unwrap_or("")),
                claim["age_secs"],
                claim["idle_secs"].as_u64().unwrap_or(0) / 60
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn cmd_simple(op: &str, raw: &[String]) -> Result<(), String> {
    let a = Args::parse(raw, &CLIENT_FLAGS, &["json"])?;
    let opts = client_opts(&a)?;
    let v = client::request_ok(&opts, json!({"op": op})).await?;
    if a.flag("json") {
        println!("{v}");
        return Ok(());
    }
    let s = |v: &Value| v.as_str().unwrap_or("").to_owned();
    match op {
        "whoami" => println!(
            "{} {} (hub at #{})",
            s(&v["name"]),
            s(&v["role"]),
            v["last_id"]
        ),
        "heartbeat" => println!("ok, {} claims kept alive", v["claims"]),
        _ => {
            for c in v["connections"].as_array().into_iter().flatten() {
                let mode = if c["live"].as_bool() == Some(true) {
                    "listening"
                } else {
                    "request"
                };
                println!(
                    "{} {} {} since {}",
                    s(&c["name"]),
                    s(&c["role"]),
                    mode,
                    s(&c["since"])
                );
            }
        }
    }
    Ok(())
}

const CONN_HELP: &str = "\
  --url URL           hub address, default ws://127.0.0.1:7300 (env SOMMELIER_URL)
  --token TOKEN       your token (env SOMMELIER_TOKEN), sent in the Authorization header
  --token-file PATH   read the token from a file (env SOMMELIER_TOKEN_FILE)
";

fn sub_help(cmd: &str) -> Option<String> {
    let (usage, body, conn) = match cmd {
        "serve" => (
            "sommelier serve [flags]",
            "Runs the hub. Listens on localhost only by default: put a tunnel or VPN in front of it.
  --listen ADDR       default 127.0.0.1:7300
  --data DIR          log (bus.jsonl) and tokens (tokens.json), default: SOMMELIER_DATA,
                      else %LOCALAPPDATA%\\sommelier (Windows) or ~/.local/share/sommelier
  --rate N            requests per second per member, default 2
  --burst N           requests at once before the rate applies, default 30
  --policy PATH       initial policy file
  --claim-ttl MIN     release a claim after this many silent minutes, default 60
  --max-body BYTES    default 4096
  --kind-limit K=N    repeatable or K=N,K=N: body limit of a kind in characters. Defaults:
                      ACCEPT and DECLINE 200, CHAT TASK RUN_REQUEST CLAIM RELEASE HELLO 600,
                      READY FIX STUCK RUN_RESULT FINDING LANDED 1200. STATUS has none
  --quiet             do not print connects and disconnects
  --ui DIR            dashboard files served over plain HTTP on the same port, default:
                      a ui folder next to the executable or two levels up from it
  --no-ui             serve no dashboard
",
            false,
        ),
        "token" => (
            "sommelier token add NAME [--role ROLE] | rotate NAME | revoke NAME | list   [--data DIR]",
            "Hub owner only, works on the token file directly (a running hub picks changes up in 2 s).
  add NAME            prints the new token once on stdout (the file keeps only a
                      salted hash), ROLE is member (default), orchestrator or observer,
                      names are case insensitive and stored in lower case
  rotate NAME         new token for NAME with the same role, the old one stops working
  revoke NAME         removes the token, open connections are closed
  list                names, roles and dates
  --data DIR          same default as `sommelier serve`: SOMMELIER_DATA, else
                      %LOCALAPPDATA%\\sommelier (Windows) or ~/.local/share/sommelier
",
            false,
        ),
        "send" => (
            "sommelier send KIND [BODY...] [flags]",
            "Sends one message and prints its id (#123). BODY `-` (or --stdin) reads the body from
stdin, lines joined with ` | `. stdin is never read otherwise. Without a body only CLAIM,
RELEASE and ACCEPT are sent (empty body), other kinds are refused. A refusal (claim held,
rate limit, size) exits with code 1.
  --topic T           issue/186, run/42, a branch name (no spaces)
  --ref R             repeatable: branch, commit, file (no spaces)
  --re ID             repeatable: refers to message ID, stored as ref #ID
  --stdin             read the body from stdin (same as BODY `-`)
  --to NAME           repeatable or A,B: addressees (must have a token), shown as @NAME
  --data DIR          client data folder for the cached CLAIM policy
  --policy-hash HASH  supply a policy hash explicitly for CLAIM
                      in tail lines. Without --to the message is for everyone.
                      NAME/WORKER addresses one worker of that participant (WORKER has
                      letters, digits, dash, underscore, dot), for example
                      --to anna/d3d12-fixes. Always send to the one who needs it
KINDS: READY LANDED FIX STUCK FINDING CLAIM RELEASE TASK ACCEPT DECLINE
       RUN_REQUEST RUN_RESULT HELLO CHAT
",
            true,
        ),
        "tail" => (
            "sommelier tail [flags]",
            "Prints one line per message as it arrives, reconnects by itself and resumes after the
last id it printed, so nothing is lost or printed twice.
  --since ID          first replay everything from ID on (inclusive)
  --last N            first replay the last N matching messages
  --kind K,K          only these kinds
  --topic T           only this topic, or a prefix with a trailing * (issue/*)
  --from A,B          only these senders
  --to me             only messages for everyone or addressed to me (or to NAME)
  --worker W          only messages for everyone, for me as a whole and for my worker W,
                      not for another worker of mine (implies --to me)
  --json              print the raw JSON frame instead of the short line
  --max N             exit after N messages
  --all               include STATUS messages (left out unless --kind names them)
  --once              print the replay (--since or --last) and exit, no live stream
",
            true,
        ),
        "log" => (
            "sommelier log [flags]",
            "Queries stored history and exits. Without --since it shows the newest messages.
  --since ID          from ID on (inclusive), up to --limit
  --last N            the last N matching messages
  --limit N           at most N messages, default 100, max 1000
  --kind, --topic, --from, --to, --worker, --all, --json   as for tail
",
            true,
        ),
        "status" => (
            "sommelier status set AGENT [--state S] [--model M] [--branch B] TEXT... | clear AGENT | list | sync --file FILE",
            "Agent rows for the dashboard, sent as STATUS messages with topic agent/AGENT. The hub
keeps the latest per sender and agent, marks a row stale after 20 min without an update
and drops it after 24 h or when its state is done. `tail` and `log` leave STATUS out
unless --kind STATUS or --all is given.
  set AGENT TEXT      one row: what AGENT works on now
  --state S           running (default), idle, blocked or done
  --model M           model name, --branch B: the branch it works on
  clear AGENT         removes the row (sends state done)
  list                all rows, --owner NAME (or me) for one sender, --json for raw rows
  sync --file FILE    makes your rows equal to a TSV file (or --file - for stdin) with the
                      columns agent, state, model, branch, text: sends changed rows and rows
                      older than 10 min, clears rows missing from the file
",
            true,
        ),
        "policy" => (
            "sommelier policy show [--json] [--data DIR] | policy publish FILE | policy rollback VERSION",
            "Show the hub policy. Use --data to read the local archive. Publish and rollback require local hub data and an orchestrator token.\n",
            false,
        ),
        "sync" => (
            "sommelier sync [--data DIR]",
            "Fetch and verify the hub policy and store it in the client data folder.\n",
            true,
        ),
        "claims" => (
            "sommelier claims [--json] | claims --check TOPIC [--files GLOB,GLOB --board PATH]",
            "Lists open claims or checks topic and board overlap. Conflicts exit 3.\n",
            true,
        ),
        "check-brief" => (
            "sommelier check-brief FILE --branch BRANCH --task NAME",
            "Checks the task header or legacy branch note. Validation failures exit 3.\n",
            false,
        ),
        "check-result" => (
            "sommelier check-result FILE --branch BRANCH --task NAME [--remote-sha FILE]",
            "Checks a READY or STUCK result and optionally a saved git ls-remote listing. Validation failures exit 3.\n",
            false,
        ),
        "who" => ("sommelier who [--json]", "Lists open connections: name, role, listening or request.\n", true),
        "whoami" => ("sommelier whoami [--json]", "Shows your name, role and the newest message id.\n", true),
        "heartbeat" => (
            "sommelier heartbeat [--json]",
            "Keeps your claims alive without sending a message (any message does too).\n",
            true,
        ),
        "guide" => (
            "sommelier guide [--role worker|lead|coordinator|brain|guest-lead] [--vendor claude|codex|gemini|other] [--format md|json] | guide --loop [--role R] [--shell sh|ps] | guide --oneshot <situation> [--json]",
            "Prints the bounded role guide (at most 40 lines), the shell loop recipe for a role, or the one-shot command for a situation (start, progress, finish, stuck, quota, context, branch, run, ask, queue).\n",
            false,
        ),
        "doctor" => (
            "sommelier doctor [--data DIR] [--policy FILE] [--hooks-dir DIR] [--claude-settings PATH] [--json]",
            "Checks binary version, sha256, token, hub reachability, policy cache, launcher hooks, clock offset, disk space, vendor tools, quota sources, and Claude settings. Exit 0 when all pass, 3 otherwise.\n",
            true,
        ),
        "handover" => (
            "sommelier handover --role <role> [--data DIR] [--state-file FILE] [--json]",
            "Prints the bounded takeover page (at most 80 lines) built from /api/state.\n",
            true,
        ),
        "alerts" => (
            "sommelier alerts [--open] [--code C1,C2] [--wait SECS] [--limit N] [--data DIR] [--json]",
            "Lists alerts from the store or hub (oldest first, default limit 20). With --open, shows only open alerts. With --wait SECS, waits for an open alert and exits 6 if none opens before the timeout.\n",
            true,
        ),
        "alert" => (
            "sommelier alert ack <KEY> [--epoch N] [--who WHO] [--role R] [--data DIR] [--json]",
            "Acknowledges an open alert by key (`KEY`, `CODE/KEY`, or message id). When --epoch N is passed, fences against the current role epoch in ROLE (refuses stale epoch with exit code 3).\n",
            true,
        ),
        "task" => (
            "sommelier task add --kind K --title T --scope G --done-when D --brief FILE [--id ID] [--priority P] [--after ID] [--branch B] [--accepts C] [--prior-art] [--json] | list [--state S] [--mine] | show ID | cancel ID [--reason TEXT] | requeue ID [--reason TEXT] [--note TEXT] | import-board FILE [--dry-run]",
            "Manages the task backlog in state.db and uploads task briefs by SHA-256.\n",
            true,
        ),
        "next" | "work" | "checkpoint" | "done" | "stuck" | "decline" | "ask" | "offers" => (
            "sommelier next [--wait SECS] [--class C] [--peek] | work ID | checkpoint ID --file F | done ID --ref B --sha S --result-file F | stuck ID --why TEXT | decline ID --why TEXT | ask --task ID TEXT | offers [--top N]",
            "Worker lifecycle commands for pulling tasks, claiming leases, forwarding checkpoints, delivering results, reporting blockers, and inspecting offers.\n",
            true,
        ),
        "land" | "scan" => (
            "sommelier land enqueue REF SHA [--task ID] [--priority P] | next [--limit N] [--claim] | result BATCH --result R | approve REF SHA | list [--state S] | sweep [--repo DIR] | sommelier scan REF [--repo DIR] [--base SHA]",
            "Manages the landing queue, batch verification results, held-branch approvals, branch sweep, and licence marker scanning.\n",
            true,
        ),
        "run" => (
            "sommelier run request --sha S [--secs N] [--need K=V] | register | result RUN --outcome O | list [--sha S] [--unregistered] | sweep --dir DIR",
            "Requests, registers, records outcomes, and sweeps game run folders.\n",
            true,
        ),
        "metric" => (
            "sommelier metric add FILE [--data DIR] [--json] | metric compare [SHA] [--file FILE] [--data DIR] [--metric NAME] [--json]",
            "Ingests performance metric rows and compares against rolling baselines with MAD noise guards.\n",
            false,
        ),
        "quota" => (
            "sommelier quota show [--offline] | probe codex|claude|gemini | ingest claude-statusline | set POOL WINDOW PCT --resets TIME | hit --vendor V --text-file F [--task ID] | calibrate | wait-secs [--pool P]   [--data DIR] [--json]",
            "Reads, probes, ingests, calibrates, and forecasts vendor quota meters and pause windows.\n",
            false,
        ),
        "plan" | "sim" | "idle" | "pause" | "resume" => (
            "sommelier plan [--dry] [--explain ID] [--policy FILE] [--data DIR] [--now SEC] [--json] | sim <LOG> [--policy FILE] [--json] | idle [--json] | pause <all|class C|pool P|owner O> | resume <all|class C|pool P|owner O>",
            "Runs the deterministic scheduler in dry-run or explain mode, replays logs in the simulator, inspects idle capacity slots, or pauses/resumes scheduling scopes.\n",
            true,
        ),
        "agent" => (
            "sommelier agent run [--config FILE] [--class C[:N]] [--mode auto|own] [--once] | exec --name N --class C --branch B -- COMMAND... | status [--data DIR] | stop [NAME] [--data DIR]   [--json]",
            "Supervisor loop and single-worker wrapper: manages worktrees, status heartbeats, checkpoint forwarding, exit classification, disk guard, and outbox spooling.\n",
            true,
        ),
        "role" => (
            "sommelier role list | claim <ROLE> [--who WHO] [--scope full|limited] | release <ROLE> [--epoch N] | handover <ROLE> --note-file <FILE> [--epoch N] | check <ROLE> [--epoch N] [--cmd COMMAND] | veto <WHO>   [--data DIR] [--json]",
            "Manages leased singleton roles, epoch fencing, planned handovers, and owner vetoes.\n",
            false,
        ),
        "standby" => (
            "sommelier standby --of <URL> [--listen ADDR] [--data DIR] [--epoch N] [--bundle-dir DIR] [--quiet]",
            "Follows the primary hub log byte-for-byte, maintains a warm state.db replica, and answers HTTP 503 until promoted.\n",
            true,
        ),
        "promote" => (
            "sommelier promote [--confirm CODE | --handback] [--primary URL] [--witness URL_OR_FILE] [--witness-token TOKEN] [--holder NAME] [--data DIR] [--guest WHO]... [--json]",
            "Promotes a standby hub to primary after verifying stale witness, unreachable primary, and confirmation code (or hands back to primary).\n",
            false,
        ),
        "drill" => (
            "sommelier drill <coordinator-dies|hub-dies|quota-drain|flapping> [--data DIR] [--json]",
            "Runs an isolated high availability failure drill on temporary ports and folders without touching the live hub.\n",
            false,
        ),
        "snapshot" | "replay" => (
            "sommelier snapshot [--data DIR] [--out FILE] [--json] | sommelier replay [--verify] [--data DIR] [--log FILE] [--json]",
            "Writes a state.db projection snapshot or replays bus.jsonl to rebuild and verify table hashes.\n",
            false,
        ),
        _ => return None,
    };
    Some(format!(
        "usage: {usage}\n\n{body}{}",
        if conn { CONN_HELP } else { "" }
    ))
}

async fn run(args: Vec<String>) -> Result<ExitCode, String> {
    let (cmd, rest) = args
        .split_first()
        .map(|(c, r)| (c.as_str(), r))
        .unwrap_or(("help", &[]));
    let wants_help = rest.iter().any(|a| a == "--help" || a == "-h");
    let help_for = match cmd {
        "help" => rest.first().map(String::as_str),
        _ if wants_help => Some(cmd),
        _ => None,
    };
    if let Some(c) = help_for {
        print!(
            "{}",
            sub_help(c).ok_or_else(|| format!("no help for `{c}`, see `sommelier help`"))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    match cmd {
        "serve" => cmd_serve(rest).await,
        "policy" => cmd_policy(rest).await,
        "sync" => cmd_sync(rest).await,
        "token" => cmd_token(rest),
        "send" => cmd_send(rest).await,
        "tail" => cmd_tail(rest).await,
        "log" => cmd_log(rest).await,
        "claims" => return cmd_claims(rest).await,
        "check-brief" | "check-result" => return cmd_validate(cmd, rest),
        "who" | "whoami" | "heartbeat" => cmd_simple(cmd, rest).await,
        "status" => cmd_status(rest).await,
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("sommelier {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => match sommelier::cli::dispatch(other, rest).await {
            Some(res) => return res,
            None => Err(format!("unknown command `{other}`, see `sommelier help`")),
        },
    }
    .map(|()| ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("sommelier: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(args)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sommelier: error: {e}");
            ExitCode::FAILURE
        }
    }
}
