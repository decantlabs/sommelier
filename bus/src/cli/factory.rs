//! `sommelier factory status` and `sommelier op`: what an operator of the factory needs besides the
//! typed commands. `status` reads the tables of a hub data folder, `op` sends one module request.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::Value;

use super::review::{call, parse};
use crate::exit;
use crate::store::Store;

const HELP: &str = "\
usage:
  sommelier factory status [--data DIR]
  sommelier op OP [JSON] [--json] [--url URL] [--token T | --token-file FILE | --data DIR]
status  prints what the hub data folder DIR holds of the factory: the submissions and their state, the landing
        queue, the tiers and stamps of key members, the run orders. It reads the tables, the hub may run.
op      sends one module request, for example `sommelier op report.send '{\"report\":{...}}'` or
        `sommelier op test.order '{\"build\":\"...\",\"channel\":\"main\",\"profile\":\"p\",\"scenario\":\"s\"}'`.
        JSON is the body of the request (default `{}`), `-` reads it from stdin. The reply is printed as JSON.
";

/// The help text of these commands.
pub fn help() -> &'static str {
    HELP
}

fn short(s: &str) -> &str {
    s.get(..12).unwrap_or(s)
}

/// Entry point of `sommelier factory` and `sommelier op`.
pub async fn run(cmd: &str, args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return Ok(exit::code(exit::OK));
    }
    let f = parse(args)?;
    match (cmd, f.rest.first().map(String::as_str)) {
        ("factory", Some("status")) => status(&f.data),
        ("op", Some(op)) => {
            let text = match f.rest.get(1).map(String::as_str) {
                None => "{}".to_owned(),
                Some("-") => {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                        .map_err(|e| format!("cannot read stdin: {e}"))?;
                    s
                }
                Some(t) => t.to_owned(),
            };
            let body: Value =
                serde_json::from_str(&text).map_err(|e| format!("the body is no JSON: {e}"))?;
            if !body.is_object() {
                return Err("the body must be one JSON object".into());
            }
            let v = call(&f, op, body).await?;
            println!("{v}");
            Ok(exit::code(exit::OK))
        }
        _ => Err("usage: sommelier factory status [--data DIR] | op OP [JSON]".into()),
    }
}

fn status(data: &PathBuf) -> Result<ExitCode, String> {
    let mut store = Store::open(data)?;
    let log = data.join("bus.jsonl");
    if log.exists() {
        store.catch_up_from_log(&log)?;
    }
    let conn = store.connection();
    println!("submissions:");
    let subs = crate::intake::facts::query_newest(conn, None, 50);
    for s in subs.iter().rev() {
        println!(
            "  {} order {} by {} state {}",
            short(&s.sha256),
            s.task,
            short(&s.member),
            s.state
        );
        if s.state == "gate_failed"
            && let Ok(Some((_, text))) = crate::review::data::gate_of(conn, &s.sha256)
            && let Ok(report) = serde_json::from_str::<Value>(&text)
        {
            for f in report["findings"].as_array().into_iter().flatten().take(5) {
                println!(
                    "    gate {}: {}",
                    f["gate"].as_str().unwrap_or("?"),
                    f["text"].as_str().unwrap_or("?")
                );
            }
        }
    }
    if subs.is_empty() {
        println!("  none");
    }
    println!("reviews:");
    let reviews = crate::review::query_reviews(conn)?;
    for r in &reviews {
        println!(
            "  {}/{} for {} reviewer {} {} {}",
            r.task,
            r.n,
            short(&r.submission),
            short(&r.reviewer),
            r.state,
            r.verdict
        );
    }
    if reviews.is_empty() {
        println!("  none");
    }
    println!("landing queue:");
    let landing = crate::land::query_landings(conn)?;
    for l in &landing {
        println!(
            "  {} {}",
            l.ref_name.get(..19).unwrap_or(&l.ref_name),
            l.state
        );
    }
    if landing.is_empty() {
        println!("  empty");
    }
    println!("tiers and stamps:");
    let stamps = crate::stamp::query_stamps(conn)?;
    let mut by_member: BTreeMap<&str, BTreeMap<&str, u32>> = BTreeMap::new();
    for s in &stamps {
        *by_member
            .entry(&s.subject)
            .or_default()
            .entry(s.kind.as_str())
            .or_insert(0) += 1;
    }
    for (member, kinds) in &by_member {
        let tier = crate::stamp::tier_table::tier_from_table(conn, member, None);
        let list: Vec<String> = kinds.iter().map(|(k, n)| format!("{k}={n}")).collect();
        println!("  {} tier {tier} {}", short(member), list.join(" "));
    }
    if by_member.is_empty() {
        println!("  none");
    }
    println!("run orders:");
    let orders = crate::report::orders::query_orders(conn)?;
    for o in &orders {
        let n = crate::report::query_reports(conn)
            .map(|r| r.iter().filter(|r| r.run == o.id).count())
            .unwrap_or(0);
        println!("  {} {} reports {n} of {}", o.id, o.state, o.wanted);
    }
    if orders.is_empty() {
        println!("  none");
    }
    Ok(exit::code(exit::OK))
}
