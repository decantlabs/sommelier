//! `sommelier feature`: reads the feature map of a project.
//!
//! `list`, `show` and `touching` read the structure from the feature file (`--file`, else the
//! environment variable SOMMELIER_FEATURES, else `features.toml` in the current folder) and add the live
//! part (open orders, state per machine key) from the hub unless `--offline` is given. `check` runs the
//! loader offline and exits 3 on a refusal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Value, json};

use crate::client::{self, Opts};
use crate::exit;
use crate::feature::{FeatureMap, profiles};
use crate::key::client as kc;
use crate::{errln, outln};

const HELP: &str = "\
usage: sommelier feature list [--area A] [--file F] [--offline] [--json]
       sommelier feature show ID [--file F] [--offline] [--json]
       sommelier feature check [--file F] [--profiles DIR] [--json]
       sommelier feature touching PATH... [--file F] [--json]
The feature map names what the product can do, which code carries it and which run proves it. One
file per project, `features.toml`, which the hub reads from the path in the [features] table of its
factory file. An order that names a feature (`sommelier task add --feature ID`) takes its scope and
its proof from the map.
  list      the features, one line each, with their area. With the hub: open orders and the state
  show      one feature: title, how to reach it, paths, depends, proofs. With the hub: open orders and
            the state per machine key
  check     runs the loader of the hub offline and says the first refusal. Exit 3 on a refusal. With
            --profiles DIR it also lists the proofs whose profile, scenario or milestone is not in the
            title profiles of DIR
  touching  the features whose paths match each given path, by the matcher of the scope gate
  --file F        the feature file, default: env SOMMELIER_FEATURES, else ./features.toml
  --offline       do not ask the hub (list and show)
  --area A        only the features of this area (list)
  --profiles DIR  a folder of title profiles (*.toml) to check the proofs against (check)
  --url URL --token TOKEN --token-file FILE  how to reach the hub, as for the other commands
  --json          one JSON object instead of text
";

/// The help text of this command.
pub fn help() -> &'static str {
    HELP
}

#[derive(Default)]
struct Flags {
    pos: Vec<String>,
    vals: BTreeMap<String, String>,
    bools: Vec<String>,
}

impl Flags {
    fn parse(raw: &[String]) -> Result<Flags, String> {
        const VALUES: [&str; 6] = ["file", "area", "profiles", "url", "token", "token-file"];
        const BOOLS: [&str; 2] = ["offline", "json"];
        let mut f = Flags::default();
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            let Some(flag) = a.strip_prefix("--") else {
                f.pos.push(a.clone());
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (flag, None),
            };
            if BOOLS.contains(&name) {
                f.bools.push(name.into());
            } else if VALUES.contains(&name) {
                let v = inline
                    .or_else(|| it.next().cloned())
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                f.vals.insert(name.into(), v);
            } else {
                return Err(format!(
                    "unknown flag --{name}, see `sommelier feature --help`"
                ));
            }
        }
        Ok(f)
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.vals.get(k).map(String::as_str)
    }

    fn flag(&self, k: &str) -> bool {
        self.bools.iter().any(|b| b == k)
    }

    fn file(&self) -> PathBuf {
        self.get("file")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("SOMMELIER_FEATURES")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from("features.toml"))
    }
}

fn refused(json_out: bool, reason: &str) -> ExitCode {
    if json_out {
        outln!(
            "{}",
            json!({ "ok": false, "code": exit::REFUSED, "reason": reason })
        );
    } else {
        errln!("refused: {reason}");
    }
    exit::code(exit::REFUSED)
}

/// Runs `sommelier feature ...`.
pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    let Some((sub, rest)) = args.split_first() else {
        errln!("{HELP}");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    if matches!(sub.as_str(), "--help" | "-h" | "help") {
        outln!("{HELP}");
        return Ok(exit::code(exit::OK));
    }
    let flags = match Flags::parse(rest) {
        Ok(f) => f,
        Err(e) => {
            errln!("sommelier: {e}");
            return Ok(exit::code(exit::WRONG_USAGE));
        }
    };
    match sub.as_str() {
        "list" => list(&flags).await,
        "show" => show(&flags).await,
        "check" => check(&flags),
        "touching" => touching(&flags),
        other => {
            errln!("sommelier: unknown subcommand `feature {other}`\n{HELP}");
            Ok(exit::code(exit::WRONG_USAGE))
        }
    }
}

fn load(flags: &Flags) -> Result<FeatureMap, String> {
    FeatureMap::load(&flags.file())
}

/// The `features` rows of the hub by id, or the reason there are none.
async fn hub_rows(flags: &Flags) -> Result<BTreeMap<String, Value>, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let url = flags
        .get("url")
        .map(str::to_owned)
        .or_else(|| env("SOMMELIER_URL"))
        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", crate::DEFAULT_PORT));
    let token = match flags
        .get("token")
        .map(str::to_owned)
        .or_else(|| env("SOMMELIER_TOKEN"))
    {
        Some(t) => t,
        None => {
            let path = flags
                .get("token-file")
                .map(str::to_owned)
                .or_else(|| env("SOMMELIER_TOKEN_FILE"))
                .or_else(kc::default_marker_file)
                .ok_or("no token")?;
            std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read the token file: {}", e.kind()))?
                .trim()
                .to_owned()
        }
    };
    let opts = Opts { url, token };
    let http = kc::http_url(
        opts.urls().first().ok_or("no hub URL configured")?,
        "/api/state",
    )?;
    let header = if kc::marker_path(&opts.token).is_some() {
        kc::auth_header(&opts.url, &opts.token)
            .await
            .map_err(|e| e.msg)?
    } else {
        format!("Bearer {}", opts.token.trim())
    };
    client::init();
    let resp = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(http)
        .header("authorization", header)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("the hub answered HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let state: Value = serde_json::from_slice(&bytes).map_err(|e| format!("bad state: {e}"))?;
    let rows = state["features"]
        .as_array()
        .ok_or("the hub has no feature map")?;
    Ok(rows
        .iter()
        .filter_map(|r| Some((r["id"].as_str()?.to_owned(), r.clone())))
        .collect())
}

fn states_text(row: &Value) -> String {
    let parts: Vec<String> = row["state"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| format!("{k} {}", v.as_str().unwrap_or("unknown")))
                .collect()
        })
        .unwrap_or_default();
    if parts.is_empty() {
        "no run yet".into()
    } else {
        parts.join(", ")
    }
}

async fn list(flags: &Flags) -> Result<ExitCode, String> {
    let json_out = flags.flag("json");
    let map = match load(flags) {
        Ok(m) => m,
        Err(e) => return Ok(refused(json_out, &e)),
    };
    let area = flags.get("area");
    if let Some(a) = area
        && map.area(a).is_none()
    {
        return Ok(refused(json_out, &format!("no area `{a}` in the file")));
    }
    let live = if flags.flag("offline") {
        Err("offline".to_owned())
    } else {
        hub_rows(flags).await
    };
    let rows: Vec<_> = map
        .ordered()
        .into_iter()
        .filter(|f| area.is_none_or(|a| f.area == a))
        .collect();
    if json_out {
        let list: Vec<Value> = rows
            .iter()
            .map(|f| {
                let mut v = json!({"id": f.id, "area": f.area, "title": f.title});
                if let Ok(l) = &live
                    && let Some(r) = l.get(&f.id)
                {
                    v["open_orders"] = r["open_orders"].clone();
                    v["state"] = r["state"].clone();
                }
                v
            })
            .collect();
        outln!(
            "{}",
            json!({"ok": true, "code": exit::OK, "features": list})
        );
        return Ok(exit::code(exit::OK));
    }
    if rows.is_empty() {
        outln!("no features");
    }
    for f in rows {
        let extra = match live.as_ref().ok().and_then(|l| l.get(&f.id)) {
            Some(r) => format!(
                "  [{} open, {}]",
                r["open_orders"].as_u64().unwrap_or(0),
                states_text(r)
            ),
            None => String::new(),
        };
        outln!("{}  {}  {}{extra}", f.id, f.area, f.title);
    }
    if let Err(e) = &live
        && e != "offline"
    {
        outln!("(no live data: {e})");
    }
    Ok(exit::code(exit::OK))
}

async fn show(flags: &Flags) -> Result<ExitCode, String> {
    let json_out = flags.flag("json");
    let Some(id) = flags.pos.first() else {
        errln!("sommelier: usage: sommelier feature show ID [--file F] [--offline] [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    };
    let map = match load(flags) {
        Ok(m) => m,
        Err(e) => return Ok(refused(json_out, &e)),
    };
    let Some(f) = map.get(id) else {
        return Ok(refused(
            json_out,
            &format!("no feature `{}` in the file", crate::feature::clip(id)),
        ));
    };
    let live = if flags.flag("offline") {
        Err("offline".to_owned())
    } else {
        hub_rows(flags).await
    };
    let row = live.as_ref().ok().and_then(|l| l.get(&f.id));
    let area_title = map.area(&f.area).map(|a| a.title.as_str()).unwrap_or("");
    if json_out {
        let proofs: Vec<Value> = f
            .proof
            .iter()
            .map(
                |p| json!({"profile": p.profile, "scenario": p.scenario, "milestone": p.milestone}),
            )
            .collect();
        let mut v = json!({
            "ok": true, "code": exit::OK, "id": f.id, "title": f.title, "area": f.area,
            "how": f.how, "paths": f.paths, "depends": f.depends, "proof": proofs,
        });
        if let Some(r) = row {
            v["open_orders"] = r["open_orders"].clone();
            v["open_tasks"] = r["open_tasks"].clone();
            v["open_runs"] = r["open_runs"].clone();
            v["state"] = r["state"].clone();
        }
        outln!("{v}");
        return Ok(exit::code(exit::OK));
    }
    outln!("{}  {}", f.id, f.title);
    outln!("area: {} ({area_title})", f.area);
    if let Some(h) = &f.how {
        outln!("how: {h}");
    }
    outln!("paths: {}", f.paths.join(", "));
    if !f.depends.is_empty() {
        outln!("depends: {}", f.depends.join(", "));
    }
    for p in &f.proof {
        outln!("proof: {}", p.line());
    }
    match row {
        Some(r) => {
            outln!(
                "open orders: {} (tasks {}, runs {})",
                r["open_orders"].as_u64().unwrap_or(0),
                r["open_tasks"].as_u64().unwrap_or(0),
                r["open_runs"].as_u64().unwrap_or(0)
            );
            outln!("state: {}", states_text(r));
        }
        None => {
            if let Err(e) = &live
                && e != "offline"
            {
                outln!("(no live data: {e})");
            }
        }
    }
    Ok(exit::code(exit::OK))
}

fn check(flags: &Flags) -> Result<ExitCode, String> {
    let json_out = flags.flag("json");
    let file = flags.file();
    let map = match FeatureMap::load(&file) {
        Ok(m) => m,
        Err(e) => return Ok(refused(json_out, &e)),
    };
    let mut problems = Vec::new();
    let mut skipped = Vec::new();
    if let Some(dir) = flags.get("profiles") {
        match profiles::read_dir(Path::new(dir)) {
            Ok((list, bad)) => {
                skipped = bad;
                problems = profiles::missing(&map, &list);
            }
            Err(e) => return Ok(refused(json_out, &e)),
        }
    }
    let ok = problems.is_empty();
    if json_out {
        outln!(
            "{}",
            json!({
                "ok": ok,
                "code": if ok { exit::OK } else { exit::REFUSED },
                "areas": map.areas().len(),
                "features": map.len(),
                "problems": problems,
                "skipped": skipped,
            })
        );
    } else {
        outln!(
            "{}: {} areas, {} features, no refusal",
            file.display(),
            map.areas().len(),
            map.len()
        );
        for s in &skipped {
            outln!("note: {s}");
        }
        for p in &problems {
            errln!("refused: {p}");
        }
    }
    Ok(exit::code(if ok { exit::OK } else { exit::REFUSED }))
}

fn touching(flags: &Flags) -> Result<ExitCode, String> {
    let json_out = flags.flag("json");
    if flags.pos.is_empty() {
        errln!("sommelier: usage: sommelier feature touching PATH... [--file F] [--json]");
        return Ok(exit::code(exit::WRONG_USAGE));
    }
    let map = match load(flags) {
        Ok(m) => m,
        Err(e) => return Ok(refused(json_out, &e)),
    };
    let rows: Vec<(String, Vec<String>)> = flags
        .pos
        .iter()
        .map(|p| (p.clone(), map.features_touching(p)))
        .collect();
    if json_out {
        let m: serde_json::Map<String, Value> =
            rows.iter().map(|(p, f)| (p.clone(), json!(f))).collect();
        outln!("{}", json!({"ok": true, "code": exit::OK, "touching": m}));
    } else {
        for (p, f) in &rows {
            if f.is_empty() {
                outln!("{p}: no feature");
            } else {
                outln!("{p}: {}", f.join(", "));
            }
        }
    }
    Ok(exit::code(exit::OK))
}
