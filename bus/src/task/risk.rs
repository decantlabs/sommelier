//! Risk of an order, computed from its scope (docs/factory.md section 6).
//!
//! An order is `guarded` when its scope can meet a hold path: some path exists that the scope allows
//! a patch to touch and that is also a hold path. Whether two globs can meet is the question whether
//! one path matches both, so [`globs_can_meet`] decides the intersection of two path globs. It uses the
//! rules of `gate::glob`: `/` separates segments, a whole segment `**` matches zero or more segments,
//! `*` matches any run of characters inside one segment and `?` one character.
//!
//! The rule is exact for those globs. The inputs around it lean to the safe side: matching ignores
//! case, a scope that names a directory (it ends with `/`, or its last segment has no dot and no
//! wildcard) also stands for everything below it, and a glob that cannot be read counts as meeting.

use std::collections::BTreeSet;

/// The three risks of an order, in rising order.
pub const RISKS: [&str; 3] = ["low", "normal", "guarded"];

/// Size words and numbers that count as small, matching the sizes of the planner.
const SMALL_SIZE: f64 = 0.5;

/// True when `risk` is one of the three known risks.
pub fn is_risk(risk: &str) -> bool {
    RISKS.contains(&risk)
}

/// The size of an order as a number, with the same words as the planner uses. An empty or unknown
/// size is 1.0, a normal order.
pub fn size_value(raw: &str) -> f64 {
    let t = raw.trim().to_ascii_lowercase();
    if t.is_empty() {
        return 1.0;
    }
    if let Ok(n) = t.parse::<f64>()
        && n.is_finite()
        && n > 0.0
    {
        return n;
    }
    match t.as_str() {
        "xs" | "tiny" => 0.25,
        "s" | "small" => 0.5,
        "m" | "medium" | "normal" => 1.0,
        "l" | "large" => 2.0,
        "xl" | "huge" => 4.0,
        _ => 1.0,
    }
}

/// True when a size value is small (half a normal order or less).
pub fn is_small(size: f64) -> bool {
    size <= SMALL_SIZE
}

/// True when the size text of an order is small.
pub fn size_is_small(raw: &str) -> bool {
    is_small(size_value(raw))
}

/// True when some path matches both `a` and `b`.
pub fn globs_can_meet(a: &str, b: &str) -> bool {
    let (a, b) = (normalize(a), normalize(b));
    let sa: Vec<&str> = a.split('/').collect();
    let sb: Vec<&str> = b.split('/').collect();
    let (na, nb) = (sa.len(), sb.len());
    // f[i][j]: the segment lists sa[i..] and sb[j..] have a common path.
    let mut f = vec![vec![false; nb + 1]; na + 1];
    f[na][nb] = true;
    for i in (0..=na).rev() {
        for j in (0..=nb).rev() {
            if i == na && j == nb {
                continue;
            }
            let a_star = i < na && sa[i] == "**";
            let b_star = j < nb && sb[j] == "**";
            let mut v = false;
            if a_star {
                // `**` of a matches nothing, or takes one segment of b.
                v |= f[i + 1][j] || (j < nb && f[i][j + 1]);
            }
            if b_star {
                v |= f[i][j + 1] || (i < na && f[i + 1][j]);
            }
            if !a_star && !b_star && i < na && j < nb {
                v = segments_can_meet(sa[i], sb[j]) && f[i + 1][j + 1];
            }
            f[i][j] = v;
        }
    }
    f[0][0]
}

/// True when two segment patterns (`*` and `?` only) match a common text.
fn segments_can_meet(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (la, lb) = (a.len(), b.len());
    let mut f = vec![vec![false; lb + 1]; la + 1];
    f[la][lb] = true;
    for i in (0..=la).rev() {
        for j in (0..=lb).rev() {
            if i == la && j == lb {
                continue;
            }
            let a_star = i < la && a[i] == '*';
            let b_star = j < lb && b[j] == '*';
            let mut v = false;
            if a_star {
                // The star of a matches nothing, or one char that b spends.
                v |= f[i + 1][j] || (j < lb && f[i][j + 1]);
            }
            if b_star {
                v |= f[i][j + 1] || (i < la && f[i + 1][j]);
            }
            if !a_star && !b_star && i < la && j < lb {
                v = (a[i] == '?' || b[j] == '?' || a[i] == b[j]) && f[i + 1][j + 1];
            }
            f[i][j] = v;
        }
    }
    f[0][0]
}

fn normalize(glob: &str) -> String {
    let g = glob.trim().replace('\\', "/").to_lowercase();
    let g = g.trim_start_matches("./").trim_start_matches('/');
    let mut out = String::new();
    let mut last_slash = false;
    for c in g.chars() {
        if c == '/' {
            if last_slash {
                continue;
            }
            last_slash = true;
        } else {
            last_slash = false;
        }
        out.push(c);
    }
    out
}

/// The globs a scope entry stands for: itself, and the whole directory below it when it names one.
fn scope_forms(entry: &str) -> Vec<String> {
    let e = normalize(entry);
    let mut forms = Vec::new();
    if e.is_empty() {
        return forms;
    }
    if let Some(dir) = e.strip_suffix('/') {
        forms.push(format!("{dir}/**"));
        return forms;
    }
    forms.push(e.clone());
    let last = e.rsplit('/').next().unwrap_or("");
    let plain = !last.contains(['*', '?', '.']) && last != "**";
    if plain {
        forms.push(format!("{e}/**"));
    }
    forms
}

/// The entries of a scope text: a comma separated list of globs.
pub fn scope_entries(scope: &str) -> Vec<&str> {
    scope
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// The hold paths a scope can meet, in the order of `hold_paths`, without repeats.
pub fn met_hold_paths<'a>(scope: &str, hold_paths: &'a [String]) -> Vec<&'a str> {
    let forms: Vec<String> = scope_entries(scope)
        .into_iter()
        .flat_map(scope_forms)
        .collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for h in hold_paths {
        if seen.insert(h.as_str()) && forms.iter().any(|f| globs_can_meet(f, h)) {
            out.push(h.as_str());
        }
    }
    out
}

/// True when `scope` can meet any of the hold paths.
pub fn scope_is_guarded(scope: &str, hold_paths: &[String]) -> bool {
    !met_hold_paths(scope, hold_paths).is_empty()
}

/// The hold paths a new order is checked against: the default list of the gates.
pub fn default_hold_paths() -> Vec<String> {
    crate::gate::GateRules::default().hold_paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::glob::glob_match;

    fn holds() -> Vec<String> {
        default_hold_paths()
    }

    #[test]
    fn scope_against_default_hold_paths() {
        let cases: [(&str, bool); 32] = [
            // plain files and narrow globs away from every hold path
            ("src/a.rs", false),
            ("src/shaders/x.glsl", false),
            ("docs/guide.md", false),
            ("docs/*.md", true),
            ("src/*/lib.rs", false),
            ("README.md", false),
            ("crates/x/src/lib.rs", false),
            // direct hits
            ("bus/src/main.rs", true),
            ("bus/**", true),
            ("swarm/run.sh", true),
            (".claude/settings.json", true),
            (".github/workflows/ci.yml", true),
            ("Cargo.lock", true),
            ("CLAUDE.md", true),
            ("docs/CLAUDE.md", true),
            ("tools/run.ps1", true),
            // wide scopes can reach a hold path
            ("src/**", true),
            ("**", true),
            ("docs/**", true),
            ("*", true),
            ("**/*.rs", true),
            // near misses
            ("bus2/src/a.rs", false),
            ("busy/a.rs", false),
            ("src/build.rs.txt", false),
            ("src/claude.mdx", false),
            ("src/*.shx", false),
            ("src/shaders/*", true),
            ("src/**/*.rs", true),
            ("Cargo.lockfile", false),
            ("github/x.yml", false),
            ("src/*.s?", true),
            ("src/x/build.r?", true),
        ];
        for (scope, want) in cases {
            assert_eq!(scope_is_guarded(scope, &holds()), want, "scope {scope}");
        }
    }

    #[test]
    fn a_directory_scope_stands_for_everything_below() {
        assert!(scope_is_guarded("bus", &holds()));
        assert!(scope_is_guarded("bus/", &holds()));
        assert!(scope_is_guarded("swarm", &holds()));
        assert!(scope_is_guarded(".github", &holds()));
        assert!(
            scope_is_guarded("src/ui/", &holds()),
            "a new script below would be a hold path"
        );
        assert!(!scope_is_guarded("src/ui/button.rs", &holds()));
    }

    #[test]
    fn a_list_is_guarded_when_one_entry_is() {
        assert!(!scope_is_guarded("src/a.rs, src/b.rs", &holds()));
        assert!(scope_is_guarded("src/a.rs, bus/src/x.rs", &holds()));
        assert!(!scope_is_guarded("", &holds()));
    }

    #[test]
    fn meet_is_symmetric_and_handles_double_star() {
        let pairs = [
            ("a/**/b", "a/b"),
            ("a/**/b", "a/x/y/b"),
            ("**/c", "a/**"),
            ("a/*", "a/**/z"),
            ("a/*/z", "a/b"),
            ("*.rs", "**/x.rs"),
            ("a?c", "ab*"),
            ("a*c", "b*c"),
        ];
        for (x, y) in pairs {
            assert_eq!(globs_can_meet(x, y), globs_can_meet(y, x), "{x} {y}");
        }
        assert!(globs_can_meet("a/**/b", "a/b"));
        assert!(globs_can_meet("a/**/b", "a/x/y/b"));
        assert!(globs_can_meet("**/c", "a/**"));
        assert!(globs_can_meet("a/*", "a/**/z"));
        assert!(!globs_can_meet("a/*/z", "a/b"));
        assert!(globs_can_meet("*.rs", "**/x.rs"));
        assert!(globs_can_meet("a?c", "ab*"));
        assert!(!globs_can_meet("a*c", "b*c"));
        assert!(!globs_can_meet("a/b", "a/b/c"));
        assert!(globs_can_meet("**", "x/y/z"));
        assert!(globs_can_meet("a/**", "a"));
    }

    /// Soundness against the matcher of the gates: when some sample path matches both globs, the rule
    /// must say they can meet. And when the rule says they can meet, some path must exist, found here
    /// among all sample paths built from a small alphabet.
    #[test]
    fn agrees_with_the_gate_matcher_on_sample_paths() {
        let parts = ["a", "b", "ab", "a.rs"];
        let mut paths: Vec<String> = Vec::new();
        let mut level: Vec<String> = parts.iter().map(|p| p.to_string()).collect();
        for _ in 0..3 {
            paths.extend(level.iter().cloned());
            level = level
                .iter()
                .flat_map(|p| parts.iter().map(move |q| format!("{p}/{q}")))
                .collect();
        }
        paths.sort();
        paths.dedup();
        let globs = [
            "a",
            "a/**",
            "**",
            "*",
            "a/*",
            "**/b",
            "*/b",
            "a?",
            "*.rs",
            "**/*.rs",
            "a/**/b",
            "ab/*",
            "?",
            "a/b/**",
            "**/a/**",
            "*b",
            "a*",
            "b/**/a.rs",
        ];
        for x in globs {
            for y in globs {
                let found = paths.iter().any(|p| glob_match(x, p) && glob_match(y, p));
                let said = globs_can_meet(x, y);
                if found {
                    assert!(said, "{x} and {y} share a path but the rule says no");
                }
                // The sample is finite, so a `true` without a witness only needs a word in the
                // glob that the alphabet lacks. Every glob here is built from the alphabet.
                assert_eq!(found, said, "{x} and {y}");
            }
        }
    }

    #[test]
    fn hostile_globs_end() {
        let long = "a/".repeat(500);
        let _ = globs_can_meet(&format!("{long}**"), &format!("**/{long}x"));
        let stars = "*a".repeat(300);
        let _ = globs_can_meet(&stars, &stars);
    }

    #[test]
    fn sizes() {
        assert!(size_is_small("small"));
        assert!(size_is_small("xs"));
        assert!(size_is_small("0.5"));
        assert!(!size_is_small(""));
        assert!(!size_is_small("m"));
        assert!(!size_is_small("junk"));
    }
}
