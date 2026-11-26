//! The feature map: one file per project that names what the product can do, which code carries it
//! and which run proves it. See `docs/factory.md`, section "Feature map".
//!
//! The file is `features.toml` in the project repository. The hub reads it from the path in the
//! `[features]` table of the `--factory` file. Orders take their scope and their proof from it, the
//! gates compare changed paths with it, the matrix shows a state per feature and the reviewer sees
//! which feature an order is about. The status of a feature is never in the file, it is derived from
//! the cells of its proofs ([`state`]).
//!
//! This file holds the types, the loader and its checks. The rest:
//!
//! * [`registry`]: the map the hub holds, reloaded when the file changes, with the alert.
//! * [`state`]: the pure derivation of the state of a feature from matrix cells.
//! * [`store`]: the projection module (feature of a run order) and the `features` part of `/api/state`.
//! * [`profiles`]: the offline check of proofs against a folder of title profiles.

pub mod profiles;
pub mod registry;
pub mod state;
pub mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

use crate::gate::gates::is_invisible;
use crate::gate::glob::any_match;

/// The only format version.
pub const VERSION: u32 = 1;
/// Most features in one file.
pub const MAX_FEATURES: usize = 2000;
/// Most areas in one file.
pub const MAX_AREAS: usize = 200;
/// Longest `title` or `how`, in characters.
pub const MAX_TEXT: usize = 200;
/// Longest id of a feature, in characters.
pub const MAX_ID: usize = 64;
/// Most paths one feature may name.
pub const MAX_PATHS: usize = 100;
/// Most proofs one feature may name.
pub const MAX_PROOFS: usize = 20;
/// Longest glob, in characters.
pub const MAX_GLOB: usize = 256;
/// Largest file the loader reads, in bytes.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// A group of features: an epic for planning and the dashboard.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Area {
    pub id: String,
    pub title: String,
}

/// The run that proves a feature: a milestone of a scenario of a title profile.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    pub profile: String,
    pub scenario: String,
    pub milestone: String,
}

impl Proof {
    /// `title-a boot first_present`, the words an order puts into its done condition.
    pub fn line(&self) -> String {
        format!("{} {} {}", self.profile, self.scenario, self.milestone)
    }
}

/// One thing the product can do, as a user or a title sees it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    pub id: String,
    pub title: String,
    pub area: String,
    /// Gate globs of the code that carries the feature. At least one.
    pub paths: Vec<String>,
    /// Features that must work first.
    #[serde(default)]
    pub depends: Vec<String>,
    /// How to reach it, one line.
    #[serde(default)]
    pub how: Option<String>,
    #[serde(default)]
    pub proof: Vec<Proof>,
}

/// The file as it is written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    version: u32,
    #[serde(default)]
    area: Vec<Area>,
    #[serde(default)]
    feature: Vec<Feature>,
}

/// A checked feature map. Features keep the order of the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeatureMap {
    areas: Vec<Area>,
    features: Vec<Feature>,
    index: BTreeMap<String, usize>,
}

fn plain_text(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() || is_invisible(c))
}

/// A lower case id of letters, digits and `_`. With `dots` the id may hold `.` between non empty parts.
fn well_formed_id(s: &str, dots: bool) -> bool {
    let ok_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_';
    if dots {
        s.split('.')
            .all(|p| !p.is_empty() && p.chars().all(ok_char))
    } else {
        !s.is_empty() && s.chars().all(ok_char)
    }
}

/// A profile, scenario or milestone id as `tester/profile.rs` writes them.
fn plain_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Why a glob cannot be a feature path, or `None` when it is fine. The gate matcher accepts any text,
/// so this lists what would never match a changed path or would read as something else.
fn bad_glob(g: &str) -> Option<&'static str> {
    if g.is_empty() || g.chars().count() > MAX_GLOB {
        return Some("is empty or longer than 256 characters");
    }
    if !plain_text(g) || g.contains('\\') {
        return Some("holds a control character or a backslash");
    }
    if g.starts_with('/') || g.ends_with('/') || g.contains("//") {
        return Some("must be a relative path without empty parts");
    }
    for seg in g.split('/') {
        if seg == ".." || seg == "." {
            return Some("must not hold a `.` or `..` part");
        }
        if seg.contains("**") && seg != "**" {
            return Some("uses `**` inside a part: it only works as a whole part");
        }
    }
    None
}

fn check_text(what: &str, field: &str, s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        return Err(format!("{what}: `{field}` is empty"));
    }
    if s.chars().count() > MAX_TEXT {
        return Err(format!(
            "{what}: `{field}` is longer than {MAX_TEXT} characters"
        ));
    }
    if !plain_text(s) {
        return Err(format!(
            "{what}: `{field}` must be plain text on one line, without control or invisible characters"
        ));
    }
    Ok(())
}

/// Cuts hostile text for an error line.
pub(crate) fn clip(s: &str) -> String {
    s.chars()
        .take(60)
        .map(|c| {
            if c.is_control() || is_invisible(c) {
                '?'
            } else {
                c
            }
        })
        .collect()
}

impl FeatureMap {
    /// Parses and checks the text of a feature file. The error is one line that names the entry.
    pub fn parse(text: &str) -> Result<FeatureMap, String> {
        let raw: RawFile = toml::from_str(text).map_err(|e| match e.span() {
            Some(span) => {
                let line = text[..span.start.min(text.len())].matches('\n').count() + 1;
                format!("line {line}: {}", e.message())
            }
            None => e.message().to_owned(),
        })?;
        Self::from_raw(raw)
    }

    /// Reads, parses and checks a feature file. The error is one line.
    pub fn load(path: &Path) -> Result<FeatureMap, String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("{}: cannot read the file: {}", path.display(), e.kind()))?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(format!(
                "{}: the file is larger than {MAX_FILE_BYTES} bytes",
                path.display()
            ));
        }
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "{}: cannot read the file as text: {}",
                path.display(),
                e.kind()
            )
        })?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn from_raw(raw: RawFile) -> Result<FeatureMap, String> {
        if raw.version != VERSION {
            return Err(format!(
                "version: `{}` is not supported, this hub reads version {VERSION}",
                raw.version
            ));
        }
        if raw.area.len() > MAX_AREAS {
            return Err(format!("area: more than {MAX_AREAS} areas"));
        }
        if raw.feature.len() > MAX_FEATURES {
            return Err(format!("feature: more than {MAX_FEATURES} features"));
        }
        let mut areas = BTreeSet::new();
        for a in &raw.area {
            let what = format!("area `{}`", clip(&a.id));
            if !well_formed_id(&a.id, false) || a.id.len() > 32 {
                return Err(format!(
                    "{what}: the id is 1 to 32 lower case letters, digits or `_`"
                ));
            }
            if !areas.insert(a.id.as_str()) {
                return Err(format!("{what}: the id is used twice"));
            }
            check_text(&what, "title", &a.title)?;
        }
        let mut index = BTreeMap::new();
        for (i, f) in raw.feature.iter().enumerate() {
            let what = format!("feature `{}`", clip(&f.id));
            if !(3..=MAX_ID).contains(&f.id.len()) || !well_formed_id(&f.id, true) {
                return Err(format!(
                    "{what}: the id is 3 to {MAX_ID} characters of lower case letters, digits, `_` and `.`, without empty parts"
                ));
            }
            if index.insert(f.id.clone(), i).is_some() {
                return Err(format!("{what}: the id is used twice"));
            }
            if !areas.contains(f.area.as_str()) {
                return Err(format!(
                    "{what}: the area `{}` does not exist",
                    clip(&f.area)
                ));
            }
            if !f.id.starts_with(&format!("{}.", f.area)) {
                return Err(format!(
                    "{what}: the id must start with the id of its area and a dot (`{}.`)",
                    f.area
                ));
            }
            check_text(&what, "title", &f.title)?;
            if let Some(h) = &f.how {
                check_text(&what, "how", h)?;
            }
            if f.paths.is_empty() {
                return Err(format!("{what}: `paths` needs at least one glob"));
            }
            if f.paths.len() > MAX_PATHS {
                return Err(format!("{what}: more than {MAX_PATHS} paths"));
            }
            for (k, g) in f.paths.iter().enumerate() {
                if let Some(why) = bad_glob(g) {
                    return Err(format!("{what}: paths[{k}] `{}` {why}", clip(g)));
                }
            }
            if f.proof.len() > MAX_PROOFS {
                return Err(format!("{what}: more than {MAX_PROOFS} proofs"));
            }
            for (k, p) in f.proof.iter().enumerate() {
                for (field, v) in [
                    ("profile", &p.profile),
                    ("scenario", &p.scenario),
                    ("milestone", &p.milestone),
                ] {
                    if !plain_ref(v) {
                        return Err(format!(
                            "{what}: proof[{k}].{field} `{}` is not a plain id: lower case letters, digits, `_` and `-`, 64 characters at most",
                            clip(v)
                        ));
                    }
                }
                if f.proof[..k].contains(p) {
                    return Err(format!("{what}: proof[{k}] is listed twice"));
                }
            }
        }
        for f in &raw.feature {
            let what = format!("feature `{}`", f.id);
            for (k, d) in f.depends.iter().enumerate() {
                if !index.contains_key(d) {
                    return Err(format!(
                        "{what}: depends[{k}] `{}` is not a feature of this file",
                        clip(d)
                    ));
                }
                if f.depends[..k].contains(d) {
                    return Err(format!("{what}: depends[{k}] `{d}` is listed twice"));
                }
            }
        }
        let map = FeatureMap {
            areas: raw.area,
            features: raw.feature,
            index,
        };
        if let Some(cycle) = map.find_cycle() {
            return Err(format!(
                "feature `{}`: `depends` has a cycle: {}",
                cycle[0],
                cycle.join(" -> ")
            ));
        }
        Ok(map)
    }

    /// One dependency cycle as a list of ids that starts and ends with the same id, or `None`.
    fn find_cycle(&self) -> Option<Vec<String>> {
        // Iterative depth first search with colours, so a hostile file cannot overflow the stack.
        let n = self.features.len();
        let mut colour = vec![0u8; n];
        for start in 0..n {
            if colour[start] != 0 {
                continue;
            }
            let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
            colour[start] = 1;
            while let Some(&(node, next)) = stack.last() {
                let deps = &self.features[node].depends;
                if next < deps.len() {
                    if let Some(top) = stack.last_mut() {
                        top.1 += 1;
                    }
                    let d = self.index[&deps[next]];
                    match colour[d] {
                        0 => {
                            colour[d] = 1;
                            stack.push((d, 0));
                        }
                        1 => {
                            let from = stack.iter().position(|(x, _)| *x == d).unwrap_or(0);
                            let mut ids: Vec<String> = stack[from..]
                                .iter()
                                .map(|(x, _)| self.features[*x].id.clone())
                                .collect();
                            ids.push(self.features[d].id.clone());
                            return Some(ids);
                        }
                        _ => {}
                    }
                } else {
                    colour[node] = 2;
                    stack.pop();
                }
            }
        }
        None
    }

    /// The areas in file order.
    pub fn areas(&self) -> &[Area] {
        &self.areas
    }

    /// The features in file order.
    pub fn features(&self) -> &[Feature] {
        &self.features
    }

    /// Number of features.
    pub fn len(&self) -> usize {
        self.features.len()
    }

    /// True when the map holds no feature.
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    /// The feature with this id.
    pub fn get(&self, id: &str) -> Option<&Feature> {
        self.index.get(id).map(|i| &self.features[*i])
    }

    /// The area with this id.
    pub fn area(&self, id: &str) -> Option<&Area> {
        self.areas.iter().find(|a| a.id == id)
    }

    /// The paths of a feature, empty for an unknown id.
    pub fn paths_of(&self, id: &str) -> &[String] {
        self.get(id).map(|f| f.paths.as_slice()).unwrap_or(&[])
    }

    /// The proofs of a feature, empty for an unknown id.
    pub fn proofs_of(&self, id: &str) -> &[Proof] {
        self.get(id).map(|f| f.proof.as_slice()).unwrap_or(&[])
    }

    /// The ids of the features whose paths match `path`, in file order. The matcher is the one of the
    /// scope gate, case sensitive.
    pub fn features_touching(&self, path: &str) -> Vec<String> {
        self.features
            .iter()
            .filter(|f| any_match(&f.paths, path, false))
            .map(|f| f.id.clone())
            .collect()
    }

    /// The features in an order where every feature comes after the features it depends on. Features
    /// that are free to go keep the order of the file.
    pub fn ordered(&self) -> Vec<&Feature> {
        let mut done: BTreeSet<&str> = BTreeSet::new();
        let mut out: Vec<&Feature> = Vec::with_capacity(self.features.len());
        // Take the first feature in file order whose dependencies are all taken, again and again.
        while out.len() < self.features.len() {
            let next = self.features.iter().find(|f| {
                !done.contains(f.id.as_str()) && f.depends.iter().all(|d| done.contains(d.as_str()))
            });
            let Some(f) = next else {
                break; // not reachable for a checked map, a cycle is refused at load
            };
            done.insert(f.id.as_str());
            out.push(f);
        }
        out
    }

    /// What the feature gate needs for an order that names `id`.
    pub fn gate_scope(&self, id: &str) -> Option<crate::gate::FeatureScope> {
        let f = self.get(id)?;
        Some(crate::gate::FeatureScope {
            feature: f.id.clone(),
            paths: f.paths.clone(),
            others: self
                .features
                .iter()
                .filter(|o| o.id != f.id)
                .map(|o| (o.id.clone(), o.paths.clone()))
                .collect(),
        })
    }

    /// The scope text of an order for the feature: its paths as a comma list, as `task add` writes it.
    pub fn scope_text(&self, id: &str) -> String {
        self.paths_of(id).join(",")
    }

    /// The done condition of an order for the feature when the caller gave none: which run proves it.
    /// `None` when the feature has no proof.
    pub fn done_text(&self, id: &str) -> Option<String> {
        let p = self.proofs_of(id);
        if p.is_empty() {
            return None;
        }
        Some(
            p.iter()
                .map(|p| format!("proof: {}", p.line()))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
version = 1
[[area]]
id = "graphics"
title = "Graphics"
[[feature]]
id = "graphics.device"
title = "A device starts"
area = "graphics"
paths = ["crates/gpu/src/device/**"]
[[feature]]
id = "graphics.present"
title = "Frames reach the window"
area = "graphics"
paths = ["crates/gpu/src/present/**", "crates/gpu/src/swap.rs"]
depends = ["graphics.device"]
how = "Start the title and wait for the first frame."
[[feature.proof]]
profile = "title-a"
scenario = "boot"
milestone = "first_present"
"#;

    fn refused(text: &str) -> String {
        FeatureMap::parse(text).expect_err("the file must be refused")
    }

    #[test]
    fn a_good_file_loads() {
        let m = FeatureMap::parse(GOOD).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m.paths_of("graphics.present").len(), 2);
        assert_eq!(
            m.proofs_of("graphics.present")[0].line(),
            "title-a boot first_present"
        );
        assert!(m.proofs_of("graphics.device").is_empty());
        assert!(m.paths_of("nope").is_empty());
    }

    #[test]
    fn every_rule_names_the_entry() {
        let swap = |from: &str, to: &str| GOOD.replace(from, to);
        let cases = [
            (swap("version = 1", "version = 2"), "version"),
            (
                swap("id = \"graphics.device\"", "id = \"graphics.present\""),
                "feature `graphics.present`: the id is used twice",
            ),
            (
                swap("id = \"graphics.device\"", "id = \"Graphics.Device\""),
                "feature `Graphics.Device`",
            ),
            (
                swap("id = \"graphics.device\"", "id = \"gr\""),
                "feature `gr`",
            ),
            (
                swap("id = \"graphics.device\"", "id = \"graphics..device\""),
                "graphics..device",
            ),
            (
                swap("id = \"graphics.device\"", "id = \"other.device\""),
                "feature `other.device`",
            ),
            (
                swap(
                    "area = \"graphics\"\npaths = [\"crates/gpu/src/device/**\"]",
                    "area = \"nothing\"\npaths = [\"x\"]",
                ),
                "the area `nothing` does not exist",
            ),
            (
                swap("paths = [\"crates/gpu/src/device/**\"]", "paths = []"),
                "feature `graphics.device`: `paths` needs at least one glob",
            ),
            (
                swap("crates/gpu/src/device/**", "/abs/**"),
                "feature `graphics.device`: paths[0]",
            ),
            (
                swap("crates/gpu/src/device/**", "a/../b"),
                "feature `graphics.device`: paths[0]",
            ),
            (
                swap("crates/gpu/src/device/**", "a/b**"),
                "feature `graphics.device`: paths[0]",
            ),
            (
                swap(
                    "depends = [\"graphics.device\"]",
                    "depends = [\"graphics.nope\"]",
                ),
                "feature `graphics.present`: depends[0] `graphics.nope`",
            ),
            (
                swap("title = \"A device starts\"", "title = \"\""),
                "feature `graphics.device`: `title` is empty",
            ),
            (
                swap(
                    "how = \"Start the title and wait for the first frame.\"",
                    &format!("how = \"{}\"", "x".repeat(201)),
                ),
                "feature `graphics.present`: `how` is longer than 200",
            ),
            (
                swap("how = \"Start", "how = \"Start\\u0007"),
                "`how` must be plain text",
            ),
            (
                swap("scenario = \"boot\"", "scenario = \"Boot Up\""),
                "feature `graphics.present`: proof[0].scenario",
            ),
            (
                swap(
                    "[[area]]\nid = \"graphics\"\ntitle = \"Graphics\"",
                    "[[area]]\nid = \"graphics\"\ntitle = \"Graphics\"\n[[area]]\nid = \"graphics\"\ntitle = \"Again\"",
                ),
                "area `graphics`: the id is used twice",
            ),
            (
                swap(
                    "[[area]]\nid = \"graphics\"",
                    "[[area]]\nid = \"Gr.aphics\"",
                ),
                "area `Gr.aphics`",
            ),
            (
                swap("title = \"A device starts\"", "title = \"A\"\nsurprise = 1"),
                "unknown field",
            ),
        ];
        for (text, want) in cases {
            let e = refused(&text);
            assert!(e.contains(want), "wanted `{want}` in `{e}`");
            assert!(!e.contains('\n'), "one line: {e}");
        }
    }

    #[test]
    fn a_cycle_is_found_and_named() {
        let text = GOOD.replace(
            "paths = [\"crates/gpu/src/device/**\"]",
            "paths = [\"crates/gpu/src/device/**\"]\ndepends = [\"graphics.present\"]",
        );
        let e = refused(&text);
        assert!(e.contains("cycle"), "{e}");
        assert!(
            e.contains("graphics.device -> graphics.present -> graphics.device"),
            "{e}"
        );
        let own = GOOD.replace(
            "paths = [\"crates/gpu/src/device/**\"]",
            "paths = [\"x/**\"]\ndepends = [\"graphics.device\"]",
        );
        assert!(refused(&own).contains("graphics.device -> graphics.device"));
    }

    #[test]
    fn too_many_features_are_refused() {
        let mut t = String::from("version = 1\n[[area]]\nid = \"a\"\ntitle = \"A\"\n");
        for i in 0..=MAX_FEATURES {
            t.push_str(&format!(
                "[[feature]]\nid = \"a.f{i}\"\ntitle = \"T\"\narea = \"a\"\npaths = [\"p{i}/**\"]\n"
            ));
        }
        assert!(refused(&t).contains("more than 2000 features"));
    }

    #[test]
    fn the_order_puts_dependencies_first_and_keeps_the_file_order_otherwise() {
        let text = r#"
version = 1
[[area]]
id = "a"
title = "A"
[[feature]]
id = "a.c"
title = "C"
area = "a"
paths = ["c/**"]
depends = ["a.b"]
[[feature]]
id = "a.b"
title = "B"
area = "a"
paths = ["b/**"]
depends = ["a.d"]
[[feature]]
id = "a.d"
title = "D"
area = "a"
paths = ["d/**"]
[[feature]]
id = "a.e"
title = "E"
area = "a"
paths = ["e/**"]
"#;
        let m = FeatureMap::parse(text).unwrap();
        let ids: Vec<&str> = m.ordered().iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["a.d", "a.b", "a.c", "a.e"]);
    }

    #[test]
    fn touching_uses_the_gate_matcher() {
        let m = FeatureMap::parse(GOOD).unwrap();
        assert_eq!(
            m.features_touching("crates/gpu/src/present/a/b.rs"),
            ["graphics.present"]
        );
        assert_eq!(
            m.features_touching("crates/gpu/src/swap.rs"),
            ["graphics.present"]
        );
        assert!(m.features_touching("crates/gpu/src/swap.rs.bak").is_empty());
        assert!(m.features_touching("README.md").is_empty());
        // Case sensitive, as the scope gate is.
        assert!(m.features_touching("Crates/gpu/src/swap.rs").is_empty());
    }

    #[test]
    fn scope_and_done_text_come_from_the_map() {
        let m = FeatureMap::parse(GOOD).unwrap();
        assert_eq!(
            m.scope_text("graphics.present"),
            "crates/gpu/src/present/**,crates/gpu/src/swap.rs"
        );
        assert_eq!(
            m.done_text("graphics.present").as_deref(),
            Some("proof: title-a boot first_present")
        );
        assert_eq!(m.done_text("graphics.device"), None);
    }
}
