//! The feature map a hub holds. It is loaded when `--factory` names a feature file, checked again
//! whenever the file changes (time stamp or size) and replaced only by a map that passes every check.
//! A broken new file keeps the old map and raises one alert, `FEATURES_INVALID`, with the first
//! refusal line. A file that is good again closes the alert.
//!
//! The maps live in a process wide table keyed by the data folder of the hub, so a store module and an
//! op can find the map of their hub without a field in the hub settings. Without a configured file the
//! part is off: [`current`] answers `None` and nothing else changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde_json::json;

use super::FeatureMap;
use crate::msg::{Kind, Message, Role};
use crate::time::rfc3339;

/// Name of the alert that a broken feature file raises.
pub const FEATURES_INVALID: &str = "FEATURES_INVALID";

/// Key of the alert. There is one feature file per hub.
const ALERT_KEY: &str = "map";

/// Longest refusal line in the alert text, in characters.
const MAX_ALERT_TEXT: usize = 300;

type Stamp = Option<(SystemTime, u64)>;

struct Slot {
    file: PathBuf,
    map: Option<Arc<FeatureMap>>,
    /// The time stamp of the file at the last attempt to read it.
    seen: Option<Stamp>,
    /// The first refusal line of the file as it is now, `None` when the file is good.
    error: Option<String>,
    /// The refusal line the hub already raised an alert for.
    alerted: Option<String>,
}

fn table() -> &'static Mutex<HashMap<PathBuf, Slot>> {
    static T: OnceLock<Mutex<HashMap<PathBuf, Slot>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stamp_of(file: &Path) -> Stamp {
    let m = std::fs::metadata(file).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// Reads the file when its time stamp changed since the last attempt. A refusal keeps the old map.
fn refresh(slot: &mut Slot) {
    let now = stamp_of(&slot.file);
    if slot.seen == Some(now) {
        return;
    }
    slot.seen = Some(now);
    match FeatureMap::load(&slot.file) {
        Ok(map) => {
            slot.map = Some(Arc::new(map));
            slot.error = None;
        }
        Err(e) => slot.error = Some(e),
    }
}

/// Names the feature file of the hub whose data folder is `data_dir` and reads it. A refusal is not an
/// error here: the hub starts without a map and raises the alert on its first tick.
pub fn configure(data_dir: &Path, file: &Path) {
    let mut slot = Slot {
        file: file.to_path_buf(),
        map: None,
        seen: None,
        error: None,
        alerted: None,
    };
    refresh(&mut slot);
    crate::lock(table()).insert(data_dir.to_path_buf(), slot);
}

/// Forgets the feature file of this hub: the part is off again.
pub fn clear(data_dir: &Path) {
    crate::lock(table()).remove(data_dir);
}

/// True when a feature file is configured for this hub, whether it is good or not.
pub fn is_on(data_dir: &Path) -> bool {
    crate::lock(table()).contains_key(data_dir)
}

/// The map of this hub, after a look at the time stamp of the file. `None` when no file is configured
/// or no good version of it was ever read.
pub fn current(data_dir: &Path) -> Option<Arc<FeatureMap>> {
    let mut t = crate::lock(table());
    let slot = t.get_mut(data_dir)?;
    refresh(slot);
    slot.map.clone()
}

/// What an op says when it needs a feature: `Ok(map)` or one line that tells why there is none.
pub fn need(data_dir: &Path) -> Result<Arc<FeatureMap>, String> {
    if !is_on(data_dir) {
        return Err(
            "this hub has no feature map: name a file in [features] of the factory file".into(),
        );
    }
    current(data_dir).ok_or_else(|| {
        "the feature file of this hub has no good version yet, see the alert FEATURES_INVALID"
            .into()
    })
}

/// The tick of the hub: looks at the file and returns the alert to raise or to close, at most one.
pub fn on_tick(data_dir: &Path, now_ms: u64) -> Vec<Message> {
    let mut t = crate::lock(table());
    let Some(slot) = t.get_mut(data_dir) else {
        return Vec::new();
    };
    refresh(slot);
    let text = match (&slot.error, &slot.alerted) {
        (Some(e), alerted) if alerted.as_ref() != Some(e) => {
            let line: String = e.chars().take(MAX_ALERT_TEXT).collect();
            let kept = if slot.map.is_some() {
                "the hub keeps the old map"
            } else {
                "the hub has no map yet"
            };
            Some((
                "open",
                format!("the feature file is broken, {kept}: {line}"),
            ))
        }
        (None, Some(_)) => Some(("closed", "the feature file is good again".to_owned())),
        _ => None,
    };
    let Some((state, text)) = text else {
        return Vec::new();
    };
    slot.alerted = slot.error.clone();
    let now_s = now_ms / 1000;
    vec![Message {
        id: 0,
        ts: rfc3339(now_ms),
        from: "hub".into(),
        role: Role::Hub,
        kind: Kind::Alert,
        topic: format!("alert/{FEATURES_INVALID}/{ALERT_KEY}"),
        body: json!({
            "v": 1,
            "code": FEATURES_INVALID,
            "key": ALERT_KEY,
            "severity": "warn",
            "state": state,
            "opened_at": now_s,
            "last_at": now_s,
            "count": 1,
            "text": text,
        })
        .to_string(),
        refs: vec![format!("state:{state}")],
        to: vec![],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "version = 1\n[[area]]\nid = \"a\"\ntitle = \"A\"\n[[feature]]\nid = \"a.one\"\ntitle = \"One\"\narea = \"a\"\npaths = [\"one/**\"]\n";

    #[test]
    fn a_broken_file_keeps_the_old_map_and_raises_one_alert() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let file = dir.path().join("features.toml");
        std::fs::write(&file, GOOD).unwrap();
        assert!(!is_on(&data));
        assert!(current(&data).is_none());
        configure(&data, &file);
        assert!(is_on(&data));
        assert_eq!(current(&data).unwrap().len(), 1);
        assert!(on_tick(&data, 1000).is_empty());

        std::fs::write(&file, format!("{GOOD}[[feature]]\nid = \"a.two\"\n")).unwrap();
        let msgs = on_tick(&data, 2000);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].topic, "alert/FEATURES_INVALID/map");
        assert!(msgs[0].body.contains("\"state\":\"open\""));
        assert_eq!(current(&data).unwrap().len(), 1, "the old map stays");
        assert!(on_tick(&data, 3000).is_empty(), "one alert for one refusal");

        // Another change that is still broken, with the same first refusal, raises nothing more.
        std::fs::write(&file, format!("{GOOD}[[feature]]\nid = \"a.two\"\n\n")).unwrap();
        assert!(on_tick(&data, 4000).is_empty());

        std::fs::write(
            &file,
            format!("{GOOD}[[feature]]\nid = \"a.two\"\ntitle = \"Two\"\narea = \"a\"\npaths = [\"two/**\"]\n"),
        )
        .unwrap();
        let msgs = on_tick(&data, 5000);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].body.contains("\"state\":\"closed\""));
        assert_eq!(current(&data).unwrap().len(), 2);
        clear(&data);
        assert!(!is_on(&data));
    }

    #[test]
    fn a_file_that_is_broken_at_start_alerts_on_the_first_tick_and_loads_when_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data2");
        let file = dir.path().join("f.toml");
        std::fs::write(&file, "version = 9\n").unwrap();
        configure(&data, &file);
        assert!(current(&data).is_none());
        assert!(need(&data).unwrap_err().contains("no good version"));
        let msgs = on_tick(&data, 1000);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].body.contains("no map yet"));
        std::fs::write(&file, GOOD).unwrap();
        assert_eq!(need(&data).unwrap().len(), 1);
        clear(&data);
        assert!(need(&data).unwrap_err().contains("no feature map"));
    }
}
