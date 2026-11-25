//! `sommelier quota` commands (section 5.1, step 5).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::exit;
use crate::quota::calibrate::{
    calibrate_codex_from_rollout, parse_launcher_log_limits, parse_limited_tsv, parse_quotas_tsv,
};
use crate::quota::claude::{self, ClaudeDropState};
use crate::quota::codex;
use crate::quota::forecast::{forecast, on_sample, pool_state};
use crate::quota::gemini::{self, SEED_CAPACITY_UNITS};
use crate::quota::meter::{
    Meter, MeterRow, PoolState, QuotaConfig, QuotaSample, Source, TokenWeights,
    contains_sensitive_text,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct QuotaStore {
    meters: BTreeMap<String, Meter>,
    #[serde(default)]
    last_gemini_hit_at: Option<u64>,
}

fn now_secs() -> u64 {
    crate::time::now_ms() / 1000
}

fn default_owner() -> String {
    std::env::var("SOMMELIER_OWNER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "local".to_owned())
}

fn quota_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("quota")
}

fn meters_path(data_dir: &Path) -> PathBuf {
    quota_dir(data_dir).join("meters.json")
}

fn claude_path(data_dir: &Path) -> PathBuf {
    quota_dir(data_dir).join("claude.json")
}

fn load_store(data_dir: &Path) -> Result<QuotaStore, String> {
    let path = meters_path(data_dir);
    if !path.exists() {
        return Ok(QuotaStore::default());
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn save_store(data_dir: &Path, store: &QuotaStore) -> Result<(), String> {
    let dir = quota_dir(data_dir);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let json = serde_json::to_string_pretty(store).map_err(|e| format!("serialize: {e}"))?;
    if contains_sensitive_text(&json) {
        return Err("refused to write sensitive text to meters.json".to_owned());
    }
    let mut tmp = tempfile::NamedTempFile::new_in(&dir).map_err(|e| format!("temp file: {e}"))?;
    tmp.write_all(json.as_bytes())
        .map_err(|e| format!("write temp: {e}"))?;
    tmp.persist(meters_path(data_dir))
        .map_err(|e| format!("persist meters.json: {e}"))?;
    Ok(())
}

fn load_claude_drop(data_dir: &Path) -> Option<ClaudeDropState> {
    let raw = fs::read_to_string(claude_path(data_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_claude_drop(data_dir: &Path, state: &ClaudeDropState) -> Result<(), String> {
    let dir = quota_dir(data_dir);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let json = serde_json::to_string_pretty(state).map_err(|e| format!("serialize: {e}"))?;
    if contains_sensitive_text(&json) {
        return Err("refused to write sensitive text to claude.json".to_owned());
    }
    let mut tmp = tempfile::NamedTempFile::new_in(&dir).map_err(|e| format!("temp file: {e}"))?;
    tmp.write_all(json.as_bytes())
        .map_err(|e| format!("write temp: {e}"))?;
    tmp.persist(claude_path(data_dir))
        .map_err(|e| format!("persist claude.json: {e}"))?;
    Ok(())
}

fn upsert_sample(store: &mut QuotaStore, sample: QuotaSample) {
    let entry = store
        .meters
        .entry(sample.pool.clone())
        .or_insert_with(|| Meter::new(&sample.owner, &sample.vendor, &sample.pool, &sample.window));
    on_sample(entry, sample);
}

fn ensure_inferred_gemini(store: &mut QuotaStore, owner: &str, now: u64) {
    let m = store
        .meters
        .entry("agy.window".to_owned())
        .or_insert_with(|| Meter::new(owner, "gemini", "agy.window", "5h"));
    if m.latest.is_none() {
        let resets_at = now.saturating_add(5 * 3600);
        let sample = gemini::inferred_sample(
            owner,
            "agy.window",
            "5h",
            0.0,
            if m.capacity_units > 0.0 {
                m.capacity_units
            } else {
                SEED_CAPACITY_UNITS
            },
            resets_at,
            now,
            m.n_calibration,
        );
        on_sample(m, sample);
    }
}

fn to_rows(store: &QuotaStore, now: u64, cfg: &QuotaConfig) -> Vec<MeterRow> {
    store
        .meters
        .values()
        .map(|m| {
            let f = forecast(m, now, &[]);
            let st = pool_state(m, &f, now, cfg);
            let latest = m.latest.as_ref();
            MeterRow {
                owner: m.owner.clone(),
                vendor: m.vendor.clone(),
                pool: m.pool.clone(),
                window: m.window.clone(),
                used_pct: f.used,
                resets_at: m
                    .state_event
                    .as_ref()
                    .filter(|ev| now < ev.resets_at)
                    .map_or(m.resets_at, |ev| ev.resets_at),
                source: latest.map_or(Source::Inferred, |s| s.source),
                confidence: f.conf,
                observed_at: latest.map_or(now, |s| s.observed_at),
                state: Some(st),
                band: Some(f.band),
                line: Some(f.line),
                burn_pct_per_min: Some(f.burn),
                t_line: f.t_line,
            }
        })
        .collect()
}

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(usage());
    };
    match sub.as_str() {
        "show" => cmd_show(rest),
        "probe" => cmd_probe(rest),
        "ingest" => cmd_ingest(rest),
        "set" => cmd_set(rest),
        "hit" => cmd_hit(rest),
        "calibrate" => cmd_calibrate(rest),
        "wait-secs" => cmd_wait_secs(rest),
        "--help" | "-h" | "help" => {
            println!("{}", usage());
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown quota subcommand `{other}`\n{}", usage())),
    }
}

fn usage() -> String {
    "\
usage:
  sommelier quota show [--json] [--offline] [--data DIR]
  sommelier quota probe codex|claude|gemini [--json] [--data DIR]
  sommelier quota ingest claude-statusline [--json] [--data DIR]
  sommelier quota set POOL WINDOW PCT --resets TIME [--vendor V] [--json] [--data DIR]
  sommelier quota hit --vendor V --text-file F [--task ID] [--family F] [--now SECS] [--json] [--data DIR]
  sommelier quota calibrate [--vendor codex|gemini|all] [--codex-dir DIR] [--gemini-dir DIR] [--json] [--data DIR]
  sommelier quota wait-secs [--pool P] [--json] [--data DIR]"
        .to_owned()
}

fn take_val(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn resolve_data_dir(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(crate::default_data_dir)
}

fn cmd_show(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut offline = std::env::var("SOMMELIER_QUOTA_NO_PROBE").is_ok_and(|v| v == "1");
    let mut data_dir = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--offline" => offline = true,
            "--data" => data_dir = Some(PathBuf::from(take_val(&mut it, "--data")?)),
            other => return Err(format!("unknown flag `{other}` for `quota show`")),
        }
    }

    let dir = resolve_data_dir(data_dir);
    let mut store = load_store(&dir)?;
    let now = now_secs();
    let owner = default_owner();

    if !offline {
        if let Ok((samples, _)) = codex::probe_codex(&owner, now, 20, None) {
            for s in samples {
                upsert_sample(&mut store, s);
            }
        }
        if let Some(drop_state) = load_claude_drop(&dir) {
            for s in drop_state.samples {
                upsert_sample(&mut store, s);
            }
        }
    }
    ensure_inferred_gemini(&mut store, &owner, now);
    save_store(&dir, &store)?;

    let cfg = QuotaConfig::default();
    let rows = to_rows(&store, now, &cfg);
    if json_out {
        let out = json!({ "ok": true, "code": 0, "as_of": now, "meters": rows });
        let text = out.to_string();
        if contains_sensitive_text(&text) {
            return Err("internal error: sensitive field in output".to_owned());
        }
        println!("{text}");
    } else if rows.is_empty() {
        println!("no quota meters recorded");
    } else {
        for r in &rows {
            let st = r.state.map_or("unknown", PoolState::as_str);
            let reset_str = if r.resets_at > 0 {
                let clock =
                    crate::time::clock(&crate::time::rfc3339(r.resets_at * 1000)).to_owned();
                format!("resets {clock}Z")
            } else {
                "no reset".to_owned()
            };
            println!(
                "{:<14} {:<7} {:>5.1}% / {:>4.0}%  {:<9} ({:?}, conf {:.2})  {}",
                r.pool,
                r.vendor,
                r.used_pct,
                r.line.unwrap_or(100.0),
                st,
                r.source,
                r.confidence,
                reset_str
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_probe(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut data_dir = None;
    let mut vendor: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => data_dir = Some(PathBuf::from(take_val(&mut it, "--data")?)),
            other if !other.starts_with('-') && vendor.is_none() => vendor = Some(other.to_owned()),
            other => return Err(format!("unknown argument `{other}` for `quota probe`")),
        }
    }
    let vendor =
        vendor.ok_or_else(|| "usage: sommelier quota probe codex|claude|gemini".to_owned())?;
    let dir = resolve_data_dir(data_dir);
    let mut store = load_store(&dir)?;
    let now = now_secs();
    let owner = default_owner();

    let (samples, via) = match vendor.as_str() {
        "codex" => {
            let (s, src) = codex::probe_codex(&owner, now, 20, None)?;
            (s, src.to_owned())
        }
        "claude" => {
            let drop = load_claude_drop(&dir).ok_or_else(|| {
                "no claude statusline data yet; configure `sommelier quota ingest claude-statusline`"
                    .to_owned()
            })?;
            (drop.samples, "statusline".to_owned())
        }
        "gemini" => {
            ensure_inferred_gemini(&mut store, &owner, now);
            let s = store
                .meters
                .get("agy.window")
                .and_then(|m| m.latest.clone())
                .into_iter()
                .collect();
            (s, "inferred".to_owned())
        }
        other => return Err(format!("unknown vendor `{other}`")),
    };

    for s in &samples {
        upsert_sample(&mut store, s.clone());
    }
    save_store(&dir, &store)?;

    if json_out {
        let out = json!({
            "ok": true,
            "code": 0,
            "vendor": vendor,
            "source_via": via,
            "samples": samples,
        });
        let text = out.to_string();
        if contains_sensitive_text(&text) {
            return Err("internal error: sensitive field in output".to_owned());
        }
        println!("{text}");
    } else {
        println!("probed {vendor} via {via}: {} sample(s)", samples.len());
        for s in &samples {
            println!(
                "  {} ({}) {:.1}% resets_at {}",
                s.pool, s.window, s.used_pct, s.resets_at
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_ingest(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut data_dir = None;
    let mut target: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => data_dir = Some(PathBuf::from(take_val(&mut it, "--data")?)),
            other if !other.starts_with('-') && target.is_none() => target = Some(other.to_owned()),
            other => return Err(format!("unknown argument `{other}` for `quota ingest`")),
        }
    }
    if target.as_deref() != Some("claude-statusline") {
        return Err("usage: sommelier quota ingest claude-statusline".to_owned());
    }

    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("read stdin: {e}"))?;

    let dir = resolve_data_dir(data_dir);
    let prev = load_claude_drop(&dir);
    let now = now_secs();
    let owner = default_owner();
    let res = claude::parse_statusline(&input, &owner, now, prev.as_ref())?;
    save_claude_drop(&dir, &res.state)?;

    let mut store = load_store(&dir)?;
    for s in &res.state.samples {
        upsert_sample(&mut store, s.clone());
    }
    save_store(&dir, &store)?;

    if json_out {
        let out = json!({
            "ok": true,
            "code": 0,
            "fresh": res.fresh,
            "bar_line": res.bar_line,
            "samples": res.state.samples,
        });
        println!("{out}");
    } else {
        println!("{}", res.bar_line);
    }
    Ok(ExitCode::SUCCESS)
}

fn parse_resets_arg(raw: &str, now: u64) -> Result<u64, String> {
    if let Ok(secs) = raw.parse::<u64>() {
        return Ok(secs);
    }
    if let Some(secs) = codex::parse_iso_epoch_secs(raw) {
        return Ok(secs);
    }
    if let Some(dur) = gemini::parse_reset_duration(&format!("Resets in {raw}")) {
        return Ok(now.saturating_add(dur));
    }
    Err(format!(
        "cannot parse --resets `{raw}` as epoch seconds, ISO UTC, or duration"
    ))
}

fn cmd_set(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut data_dir = None;
    let mut resets_raw: Option<String> = None;
    let mut vendor_override: Option<String> = None;
    let mut pos = Vec::new();

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => data_dir = Some(PathBuf::from(take_val(&mut it, "--data")?)),
            "--resets" => resets_raw = Some(take_val(&mut it, "--resets")?),
            "--vendor" => vendor_override = Some(take_val(&mut it, "--vendor")?),
            other if !other.starts_with('-') => pos.push(other.to_owned()),
            other => return Err(format!("unknown flag `{other}` for `quota set`")),
        }
    }
    if pos.len() != 3 {
        return Err(
            "usage: sommelier quota set POOL WINDOW PCT --resets TIME [--vendor V]".to_owned(),
        );
    }
    let pool = pos[0].clone();
    let window = pos[1].clone();
    let used_pct: f64 = pos[2]
        .parse()
        .map_err(|_| format!("invalid percentage `{}`", pos[2]))?;
    if !used_pct.is_finite() || !(0.0..=150.0).contains(&used_pct) {
        return Err("percentage must be between 0 and 150".to_owned());
    }

    let now = now_secs();
    let resets_at = match resets_raw {
        Some(r) => parse_resets_arg(&r, now)?,
        None => return Err("--resets TIME is required".to_owned()),
    };
    let vendor = vendor_override.unwrap_or_else(|| {
        if pool.starts_with("claude") {
            "claude".to_owned()
        } else if pool.starts_with("codex") {
            "codex".to_owned()
        } else {
            "gemini".to_owned()
        }
    });

    let sample = QuotaSample {
        owner: default_owner(),
        vendor,
        pool: pool.clone(),
        window: window.clone(),
        observed_at: now,
        used_pct,
        resets_at,
        source: Source::Manual,
        confidence: Source::Manual.base_confidence(),
    };

    let dir = resolve_data_dir(data_dir);
    let mut store = load_store(&dir)?;
    upsert_sample(&mut store, sample.clone());
    save_store(&dir, &store)?;

    if json_out {
        println!("{}", json!({"ok": true, "code": 0, "sample": sample}));
    } else {
        println!("set {pool} ({window}) to {used_pct:.1}% resets_at {resets_at}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_hit(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut data_dir = None;
    let mut vendor: Option<String> = None;
    let mut text_file: Option<PathBuf> = None;
    let mut task: Option<String> = None;
    let mut family: Option<String> = None;
    let mut now_override: Option<u64> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => data_dir = Some(PathBuf::from(take_val(&mut it, "--data")?)),
            "--vendor" => vendor = Some(take_val(&mut it, "--vendor")?),
            "--text-file" => text_file = Some(PathBuf::from(take_val(&mut it, "--text-file")?)),
            "--task" => task = Some(take_val(&mut it, "--task")?),
            "--family" => family = Some(take_val(&mut it, "--family")?),
            "--now" => {
                now_override = Some(
                    take_val(&mut it, "--now")?
                        .parse()
                        .map_err(|_| "--now must be epoch seconds".to_owned())?,
                )
            }
            other => return Err(format!("unknown flag `{other}` for `quota hit`")),
        }
    }
    let vendor = vendor.ok_or_else(|| "--vendor is required".to_owned())?;
    let path = text_file.ok_or_else(|| "--text-file is required".to_owned())?;
    let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;

    let dir = resolve_data_dir(data_dir);
    let mut store = load_store(&dir)?;
    let now = now_override.unwrap_or_else(now_secs);
    let owner = default_owner();

    if vendor == "gemini" {
        if let Some(hit) = gemini::parse_limit_message(&text, now, family.as_deref()) {
            let prev = store.last_gemini_hit_at;
            let mut week_meter = store
                .meters
                .remove("agy.week")
                .unwrap_or_else(|| Meter::new(&owner, "gemini", "agy.week", "week"));
            let mut win_meter = store
                .meters
                .remove("agy.window")
                .unwrap_or_else(|| Meter::new(&owner, "gemini", "agy.window", "5h"));
            let fam_key = hit.family.as_ref().map(|f| format!("agy.family.{f}"));
            let mut fam_meter = fam_key.as_ref().map(|k| {
                store
                    .meters
                    .remove(k)
                    .unwrap_or_else(|| Meter::new(&owner, "gemini", k, "5h"))
            });

            gemini::apply_limit_hit(
                &mut win_meter,
                &mut week_meter,
                fam_meter.as_mut(),
                &hit,
                prev,
            );
            store.meters.insert("agy.window".to_owned(), win_meter);
            store.meters.insert("agy.week".to_owned(), week_meter);
            if let (Some(k), Some(fm)) = (fam_key, fam_meter) {
                store.meters.insert(k, fm);
            }
            store.last_gemini_hit_at = Some(now);
            save_store(&dir, &store)?;

            let clock = crate::time::clock(&crate::time::rfc3339(hit.resets_at * 1000)).to_owned();
            let target = hit.family.as_deref().unwrap_or(&hit.target_pool);
            let task_note = task.map_or_else(String::new, |t| format!(" task {t}"));
            if json_out {
                println!(
                    "{}",
                    json!({
                        "ok": true,
                        "code": exit::QUOTA_PAUSE,
                        "paused": target,
                        "resets_at": hit.resets_at,
                        "pool": hit.target_pool,
                    })
                );
            } else {
                println!("pause {target} until {clock}Z{task_note}");
            }
            return Ok(exit::code(exit::QUOTA_PAUSE));
        }
    } else if vendor == "claude" {
        for line in text.lines() {
            if let Some(ev) = claude::parse_stream_rate_limit_event(line, &owner, now) {
                if let Some(s) = ev.sample {
                    upsert_sample(&mut store, s);
                }
                if let Some(se) = ev.state_event {
                    let pool = if se.message.contains("seven_day") {
                        "claude.week"
                    } else {
                        "claude.5h"
                    };
                    let win = if pool == "claude.week" { "week" } else { "5h" };
                    let m = store
                        .meters
                        .entry(pool.to_owned())
                        .or_insert_with(|| Meter::new(&owner, "claude", pool, win));
                    let resets_at = se.resets_at;
                    m.state_event = Some(se);
                    save_store(&dir, &store)?;
                    if json_out {
                        println!(
                            "{}",
                            json!({"ok": true, "code": exit::QUOTA_PAUSE, "paused": pool, "resets_at": resets_at})
                        );
                    } else {
                        println!("pause {pool} until {resets_at}");
                    }
                    return Ok(exit::code(exit::QUOTA_PAUSE));
                }
            }
        }
    }

    if json_out {
        println!("{}", json!({"ok": true, "code": 0, "matched": false}));
    } else {
        println!("no quota limit pattern matched");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_calibrate(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut data_dir = None;
    let mut vendor = "all".to_owned();
    let mut codex_dir: Option<PathBuf> = None;
    let mut gemini_dir: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => data_dir = Some(PathBuf::from(take_val(&mut it, "--data")?)),
            "--vendor" => vendor = take_val(&mut it, "--vendor")?,
            "--codex-dir" => codex_dir = Some(PathBuf::from(take_val(&mut it, "--codex-dir")?)),
            "--gemini-dir" => gemini_dir = Some(PathBuf::from(take_val(&mut it, "--gemini-dir")?)),
            other => return Err(format!("unknown flag `{other}` for `quota calibrate`")),
        }
    }

    let dir = resolve_data_dir(data_dir);
    let mut store = load_store(&dir)?;
    let owner = default_owner();
    let mut codex_slopes: BTreeMap<String, f64> = BTreeMap::new();
    let mut gemini_events: usize = 0;

    if vendor == "all" || vendor == "codex" {
        let sdir = codex_dir.or_else(codex::default_sessions_dir);
        if let Some(sdir) = sdir
            && sdir.exists()
        {
            let mut combined = String::new();
            let mut stack = vec![sdir];
            while let Some(d) = stack.pop() {
                if let Ok(entries) = fs::read_dir(d) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if p.is_dir() {
                            stack.push(p);
                        } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                            && let Ok(txt) = fs::read_to_string(&p)
                        {
                            combined.push_str(&txt);
                            combined.push('\n');
                        }
                    }
                }
            }
            for pool in ["codex.5h", "codex.week"] {
                if let Some(slope) =
                    calibrate_codex_from_rollout(&combined, &owner, pool, &TokenWeights::default())
                {
                    codex_slopes.insert(pool.to_owned(), slope);
                    if let Some(m) = store.meters.get_mut(pool) {
                        m.pct_per_unit = slope;
                        m.n_calibration += 1;
                    }
                }
            }
        }
    }

    if (vendor == "all" || vendor == "gemini")
        && let Some(gdir) = gemini_dir
    {
        for (file, parser) in [
            ("quotas.tsv", parse_quotas_tsv as fn(&str) -> Vec<_>),
            ("limited.tsv", parse_limited_tsv as fn(&str) -> Vec<_>),
            (
                "launcher.log",
                parse_launcher_log_limits as fn(&str) -> Vec<_>,
            ),
        ] {
            let p = gdir.join(file);
            if let Ok(txt) = fs::read_to_string(&p) {
                gemini_events += parser(&txt).len();
            }
        }
    }

    save_store(&dir, &store)?;
    if json_out {
        println!(
            "{}",
            json!({
                "ok": true,
                "code": 0,
                "codex_slopes": codex_slopes,
                "gemini_history_events": gemini_events,
            })
        );
    } else {
        println!(
            "calibrated codex pools: {}, gemini history events: {}",
            codex_slopes.len(),
            gemini_events
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_wait_secs(args: &[String]) -> Result<ExitCode, String> {
    let mut json_out = false;
    let mut data_dir = None;
    let mut pool_filter: Option<String> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => json_out = true,
            "--data" => data_dir = Some(PathBuf::from(take_val(&mut it, "--data")?)),
            "--pool" => pool_filter = Some(take_val(&mut it, "--pool")?),
            other => return Err(format!("unknown flag `{other}` for `quota wait-secs`")),
        }
    }

    let dir = resolve_data_dir(data_dir);
    let store = load_store(&dir)?;
    let now = now_secs();
    let cfg = QuotaConfig::default();

    let mut wait: Option<u64> = None;
    for (name, m) in &store.meters {
        if let Some(pf) = &pool_filter
            && pf != name
        {
            continue;
        }
        let f = forecast(m, now, &[]);
        let st = pool_state(m, &f, now, &cfg);
        if matches!(st, PoolState::Paused | PoolState::Closed) {
            let reset = m
                .state_event
                .as_ref()
                .filter(|ev| now < ev.resets_at)
                .map_or(m.resets_at, |ev| ev.resets_at);
            let secs = if reset > now { reset - now } else { 60 };
            wait = Some(wait.map_or(secs, |cur| cur.min(secs)));
        }
    }

    let secs = wait.unwrap_or(0);
    if json_out {
        println!("{}", json!({"ok": true, "code": 0, "wait_secs": secs}));
    } else {
        println!("{secs}");
    }
    Ok(ExitCode::SUCCESS)
}
