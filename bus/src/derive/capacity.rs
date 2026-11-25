//! Capacity parsing from HELLO messages and `sommelier hello` flags (section 5.1).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Capacity delivery mode of a participant or machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapacityMode {
    #[default]
    Auto,
    Own,
}

impl CapacityMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CapacityMode::Auto => "auto",
            CapacityMode::Own => "own",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(CapacityMode::Auto),
            "own" => Some(CapacityMode::Own),
            _ => None,
        }
    }
}

/// Capacity declared by a member in a HELLO body or refs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloCapacity {
    pub machine: String,
    pub os: String,
    pub gpu: String,
    pub mode: CapacityMode,
    pub gpu_slots: u32,
    /// `class -> useful_parallel` slots.
    pub classes: BTreeMap<String, u32>,
    /// Accepted task kinds (`fix`, `review`, `run`, `scout`, ...).
    pub accepts: Vec<String>,
    pub declared_at_ms: u64,
}

impl HelloCapacity {
    pub fn total_slots(&self) -> u32 {
        self.classes.values().sum()
    }
}

/// Parses capacity fields from a HELLO body and refs.
///
/// Accepts `class=codex:4,gemini:2` or `classes=codex:4` or `capacity=codex:4` or
/// `slots=4`, `mode=auto|own`, `gpu_slots=1` (or `gpu-slots=1`), `accepts=fix,review`,
/// `machine=box1`, `os=windows`, `gpu=rtx4090`, as well as refs like `class:codex:4`,
/// `mode:own`, `gpu_slots:1`, `accepts:fix,review`.
pub fn parse_hello_capacity(body: &str, refs: &[String], ts_ms: u64) -> HelloCapacity {
    let mut cap = HelloCapacity {
        declared_at_ms: ts_ms,
        ..Default::default()
    };
    for token in split_tokens(body) {
        if let Some((k, v)) = split_kv(&token) {
            apply_kv(&mut cap, &k, &v);
        }
    }
    for r in refs {
        if let Some((k, v)) = r.split_once(':').or_else(|| r.split_once('=')) {
            apply_kv(&mut cap, &k.trim().to_ascii_lowercase(), v.trim());
        }
    }
    cap
}

fn split_tokens(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in body.split(['|', ';']) {
        for part in seg.split_whitespace() {
            let trimmed = part.trim_matches(',');
            if !trimmed.is_empty() {
                out.push(trimmed.to_owned());
            }
        }
    }
    out
}

fn split_kv(token: &str) -> Option<(String, String)> {
    let (k, v) = token.split_once('=').or_else(|| token.split_once(':'))?;
    let key = k.trim().to_ascii_lowercase().replace('-', "_");
    let val = v.trim().trim_matches(',').to_owned();
    if key.is_empty() || val.is_empty() {
        return None;
    }
    Some((key, val))
}

fn apply_kv(cap: &mut HelloCapacity, key: &str, val: &str) {
    match key {
        "machine" | "host" => cap.machine = val.to_owned(),
        "os" => cap.os = val.to_owned(),
        "gpu" => cap.gpu = val.to_owned(),
        "mode" => {
            if let Some(m) = CapacityMode::parse(val) {
                cap.mode = m;
            }
        }
        "gpu_slots" | "gpuslots" => {
            if let Ok(n) = val.parse::<u32>() {
                cap.gpu_slots = n;
            }
        }
        "accepts" | "accept" => {
            for item in val.split([',', '+']) {
                let item = item.trim().to_ascii_lowercase();
                if !item.is_empty() && !cap.accepts.contains(&item) {
                    cap.accepts.push(item);
                }
            }
        }
        "class" | "classes" | "capacity" => {
            for item in val.split([',', '+']) {
                let item = item.trim();
                if item.is_empty() {
                    continue;
                }
                if let Some((cls, count_s)) = item.split_once(':').or_else(|| item.split_once('='))
                {
                    let cls = cls.trim().to_ascii_lowercase();
                    if !cls.is_empty()
                        && let Ok(n) = count_s.trim().parse::<u32>()
                    {
                        cap.classes.insert(cls, n);
                    }
                } else if let Ok(n) = item.parse::<u32>() {
                    cap.classes.insert("default".into(), n);
                }
            }
        }
        "slots" | "useful_parallel" | "parallel" => {
            if let Ok(n) = val.parse::<u32>() {
                cap.classes.entry("default".into()).or_insert(n);
            }
        }
        _ => {}
    }
}
