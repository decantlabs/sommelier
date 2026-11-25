//! Task priority, aging, resume boost, and track share scoring (section 4.1).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::PlanTask;

/// Priority, aging, resume boost, and track share weights from `[priority]` (section 5.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PriorityConfig {
    #[serde(default = "default_levels")]
    pub levels: BTreeMap<String, i64>,
    #[serde(default = "default_aging_per_10min")]
    pub aging_per_10min: i64,
    #[serde(default = "default_aging_cap")]
    pub aging_cap: i64,
    #[serde(default = "default_resume_boost")]
    pub resume_boost: i64,
    #[serde(default = "default_track_weight")]
    pub track_weight: f64,
    #[serde(default = "default_tracks")]
    pub tracks: BTreeMap<String, f64>,
}

fn default_levels() -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    m.insert("blocker".to_owned(), 100);
    m.insert("high".to_owned(), 60);
    m.insert("normal".to_owned(), 30);
    m.insert("filler".to_owned(), 5);
    m
}

fn default_aging_per_10min() -> i64 {
    2
}

fn default_aging_cap() -> i64 {
    40
}

fn default_resume_boost() -> i64 {
    25
}

fn default_track_weight() -> f64 {
    20.0
}

fn default_tracks() -> BTreeMap<String, f64> {
    let mut m = BTreeMap::new();
    m.insert("game".to_owned(), 0.6);
    m.insert("shaders".to_owned(), 0.3);
    m.insert("control".to_owned(), 0.1);
    m
}

impl Default for PriorityConfig {
    fn default() -> Self {
        Self {
            levels: default_levels(),
            aging_per_10min: default_aging_per_10min(),
            aging_cap: default_aging_cap(),
            resume_boost: default_resume_boost(),
            track_weight: default_track_weight(),
            tracks: default_tracks(),
        }
    }
}

impl PriorityConfig {
    pub fn level(&self, priority: &str) -> i64 {
        let key = priority.trim().to_ascii_lowercase();
        if let Some(&v) = self.levels.get(&key) {
            return v;
        }
        match key.as_str() {
            "blocker" => 100,
            "high" => 60,
            "normal" => 30,
            "filler" => 5,
            _ => 30,
        }
    }
}

/// Deterministic breakdown of `score(t, now)` for scheduling and `plan --explain`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub base: i64,
    pub minutes_ready: u64,
    pub aging: i64,
    pub resume_boost: i64,
    pub target_share: f64,
    pub running_share: f64,
    pub track_bonus: f64,
    pub total: f64,
    /// Fixed point millipoints used for deterministic ordering without float drift.
    pub total_milli: i64,
}

/// Computes the current running share per track across all active tasks.
/// Uses `BTreeMap` so iteration order is strictly deterministic.
pub fn compute_running_shares(active_tracks: &[&str]) -> BTreeMap<String, f64> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0usize;
    for track in active_tracks {
        let t = track.trim();
        if !t.is_empty() {
            *counts.entry(t.to_owned()).or_insert(0) += 1;
            total += 1;
        }
    }
    let mut shares = BTreeMap::new();
    if total == 0 {
        return shares;
    }
    for (track, count) in counts {
        shares.insert(track, count as f64 / total as f64);
    }
    shares
}

/// Computes `score(t, now)` per section 4.1:
///
/// `score(t, now) = level(t.priority)`
/// `              + min(aging_cap, aging_per_10min * floor(minutes_ready / 10))`
/// `              + (resume_boost if t.attempt > 1 else 0)`
/// `              + track_weight * (target_share(t.track) - running_share(t.track))`
pub fn score_task(
    task: &PlanTask,
    cfg: &PriorityConfig,
    running_shares: &BTreeMap<String, f64>,
    now: u64,
) -> ScoreBreakdown {
    let base = cfg.level(&task.priority);
    let ready_at = if task.ready_at > 0 {
        task.ready_at
    } else {
        task.created_at
    };
    let minutes_ready = if ready_at > 0 {
        now.saturating_sub(ready_at) / 60
    } else {
        0
    };
    let buckets = (minutes_ready / 10) as i64;
    let aging = buckets
        .saturating_mul(cfg.aging_per_10min)
        .min(cfg.aging_cap)
        .max(0);
    let resume_boost = if task.attempt > 1 {
        cfg.resume_boost
    } else {
        0
    };

    let track_key = task.track.trim();
    let (target_share, running_share, track_bonus) = if track_key.is_empty() {
        (0.0, 0.0, 0.0)
    } else {
        let target = cfg.tracks.get(track_key).copied().unwrap_or(0.0);
        let running = running_shares.get(track_key).copied().unwrap_or(0.0);
        let bonus = if cfg.tracks.contains_key(track_key) || running > 0.0 {
            cfg.track_weight * (target - running)
        } else {
            0.0
        };
        (target, running, bonus)
    };

    let total = (base + aging + resume_boost) as f64 + track_bonus;
    let total_milli = (total * 1000.0).round() as i64;

    ScoreBreakdown {
        base,
        minutes_ready,
        aging,
        resume_boost,
        target_share,
        running_share,
        track_bonus: (track_bonus * 1000.0).round() / 1000.0,
        total: (total * 1000.0).round() / 1000.0,
        total_milli,
    }
}

/// Sorts ready tasks by `(score desc, id asc)` deterministically (section 4.1).
pub fn sort_ready_tasks(tasks: Vec<(PlanTask, ScoreBreakdown)>) -> Vec<(PlanTask, ScoreBreakdown)> {
    let mut out = tasks;
    out.sort_by(|(ta, sa), (tb, sb)| {
        sb.total_milli
            .cmp(&sa.total_milli)
            .then_with(|| ta.id.cmp(&tb.id))
    });
    out
}
