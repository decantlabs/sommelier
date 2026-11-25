//! Path glob matching for scope, hold path and manifest patterns.
//!
//! The repository has no path matcher yet (`task::state` only compares glob prefixes), so the gates
//! carry this small one. Rules: `/` separates segments, `**` as a whole segment matches zero or more
//! segments, `*` matches any run of characters inside one segment, `?` matches one character inside
//! one segment. Matching is polynomial in the segment counts and never recurses.

/// True when `path` matches `pattern`. Case sensitive.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let ps: Vec<&str> = pattern.split('/').collect();
    let xs: Vec<&str> = path.split('/').collect();
    let (np, nx) = (ps.len(), xs.len());
    // dp[i][j]: pattern segments i.. match path segments j..
    let mut dp = vec![vec![false; nx + 1]; np + 1];
    dp[np][nx] = true;
    for i in (0..np).rev() {
        for j in (0..=nx).rev() {
            dp[i][j] = if ps[i] == "**" {
                dp[i + 1][j] || (j < nx && dp[i][j + 1])
            } else {
                j < nx && segment_match(ps[i], xs[j]) && dp[i + 1][j + 1]
            };
        }
    }
    dp[0][0]
}

/// Like [`glob_match`] but ignores case. Used where the file system may fold case.
pub fn glob_match_ci(pattern: &str, path: &str) -> bool {
    glob_match(&pattern.to_lowercase(), &path.to_lowercase())
}

/// True when any pattern of the list matches.
pub fn any_match<S: AsRef<str>>(patterns: &[S], path: &str, ci: bool) -> bool {
    patterns.iter().any(|p| {
        if ci {
            glob_match_ci(p.as_ref(), path)
        } else {
            glob_match(p.as_ref(), path)
        }
    })
}

fn segment_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_usual_shapes() {
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("**/Cargo.toml", "Cargo.toml"));
        assert!(glob_match("**/Cargo.toml", "a/b/Cargo.toml"));
        assert!(glob_match("src/*.rs", "src/a.rs"));
        assert!(!glob_match("src/*.rs", "src/a/b.rs"));
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("src/**", "src"));
        assert!(!glob_match("src/**", "srcx/a"));
        assert!(glob_match_ci("Swarm/**", "swarm/x.sh"));
    }

    #[test]
    fn hostile_input_ends() {
        let long = "a/".repeat(2000);
        assert!(!glob_match("**/**/**/x", &long));
        assert!(!glob_match("*a*a*a*a*b", &"a".repeat(2000)));
    }
}
