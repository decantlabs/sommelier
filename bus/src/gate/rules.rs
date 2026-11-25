//! The settings of the static gates. They arrive as a struct with a `Default`, never from the policy
//! file. The wiring that fills them from the policy comes in a later wave.

use serde::{Deserialize, Serialize};

/// Size limits for one tier of authors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierCaps {
    pub bundle_bytes: u64,
    pub changed_lines: u64,
    pub files: u64,
    pub largest_file: u64,
}

/// A named regular expression. The name is what a finding says, never the matched text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedPattern {
    pub name: String,
    pub pattern: String,
}

fn np(name: &str, pattern: &str) -> NamedPattern {
    NamedPattern {
        name: name.into(),
        pattern: pattern.into(),
    }
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// Everything the gates read besides the diff and the order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRules {
    /// Globs of paths that need a guarded order (CP 5.3 `trust.guest.hold_paths`).
    pub hold_paths: Vec<String>,
    /// Globs of manifests and lock files.
    pub manifests: Vec<String>,
    /// Caps by tier, index 0 to 3. A higher tier uses the last entry.
    pub caps: Vec<TierCaps>,
    /// Longest accepted author name and email, in characters.
    pub max_name_chars: usize,
    pub max_email_chars: usize,
    /// Capability patterns for added lines.
    pub capabilities: Vec<NamedPattern>,
    /// Patterns for removed lines that count as a test or an assertion.
    pub removed_test_patterns: Vec<NamedPattern>,
    /// Patterns for added lines that weaken tests.
    pub added_test_patterns: Vec<NamedPattern>,
    /// Globs of test files. Deleting one is listed.
    pub test_files: Vec<String>,
    /// Licence markers on added lines.
    pub licence_markers: Vec<NamedPattern>,
    /// SPDX identifiers the project accepts.
    pub allowed_spdx: Vec<String>,
    /// Secret and personal data patterns for added lines.
    pub secrets: Vec<NamedPattern>,
    /// Email addresses that are not personal. A match that ends with one of these is accepted.
    pub email_allow: Vec<String>,
    /// Most findings one gate may list. More are summed up in one note.
    pub max_findings_per_gate: usize,
}

impl Default for GateRules {
    fn default() -> Self {
        let kib = 1024u64;
        Self {
            hold_paths: strings(&[
                "**/CLAUDE.md",
                "**/AGENTS.md",
                ".claude/**",
                "swarm/**",
                "bus/**",
                "**/build.rs",
                "Cargo.lock",
                "**/*.sh",
                "**/*.ps1",
                ".github/**",
            ]),
            manifests: strings(&["**/Cargo.toml", "Cargo.lock"]),
            caps: vec![
                TierCaps {
                    bundle_bytes: 256 * kib,
                    changed_lines: 300,
                    files: 10,
                    largest_file: 64 * kib,
                },
                TierCaps {
                    bundle_bytes: 1024 * kib,
                    changed_lines: 1000,
                    files: 30,
                    largest_file: 128 * kib,
                },
                TierCaps {
                    bundle_bytes: 4096 * kib,
                    changed_lines: 3000,
                    files: 100,
                    largest_file: 256 * kib,
                },
                TierCaps {
                    bundle_bytes: 16384 * kib,
                    changed_lines: 10000,
                    files: 300,
                    largest_file: 1024 * kib,
                },
            ],
            max_name_chars: 100,
            max_email_chars: 254,
            capabilities: vec![
                np(
                    "process_start",
                    r"\b(std::process|tokio::process|Command::new)\b",
                ),
                np(
                    "network",
                    r"\b(TcpStream|TcpListener|UdpSocket|UnixStream|UnixListener|std::net|tokio::net|reqwest|hyper|ureq)\b",
                ),
                np(
                    "file_write",
                    r"\b(fs::write|fs::remove_file|fs::remove_dir_all|fs::remove_dir|fs::rename|fs::copy|fs::create_dir|fs::create_dir_all|File::create|File::create_new|OpenOptions)\b",
                ),
                np("unsafe", r"\bunsafe\b"),
                np("extern_block", r#"\bextern\s*("[^"]*"\s*)?\{"#),
                np("link_attribute", r"#!?\[\s*link\b"),
                np(
                    "proc_macro",
                    r"(#\[\s*proc_macro(_derive|_attribute)?\b|proc-macro\s*=\s*true)",
                ),
                np("embedded_file", r"\binclude_(bytes|str)!"),
                np(
                    "dynamic_library",
                    r"\b(libloading|dlopen|LoadLibrary[AW]?|Library::new)\b",
                ),
                np(
                    "environment",
                    r"\b(env::var|env::vars|env::var_os|env::vars_os|option_env!|env!)",
                ),
            ],
            removed_test_patterns: vec![
                np("test_attribute", r"#\[\s*(tokio::)?test\b"),
                np("assertion", r"\b(debug_)?assert(_eq|_ne|_matches)?!"),
            ],
            added_test_patterns: vec![np("ignore_attribute", r"#\[\s*ignore\b")],
            test_files: strings(&[
                "tests/**",
                "**/tests/**",
                "**/tests.rs",
                "**/*_test.rs",
                "**/*_tests.rs",
            ]),
            // Bracket expressions keep this file from matching its own patterns.
            licence_markers: vec![
                np(
                    "gpl_family_marker",
                    r"([G]PL-[23]\.0|[L]GPL-[23]\.[01]|[A]GPL-3\.0|GNU [G]eneral Public License|GNU [L]esser General Public License|GNU [A]ffero General Public License|SPDX-License-Identifier:\s*([G]PL|[L]GPL|[A]GPL))",
                ),
                np("ported_from", r"(?i)\bported\s+from\b"),
            ],
            allowed_spdx: strings(&["MIT"]),
            secrets: vec![
                np("private_key_header", r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY"),
                np(
                    "token_shape",
                    r"(\b(ghp|gho|ghs|ghu|ghr)_[A-Za-z0-9]{20,}|\bgithub_pat_[A-Za-z0-9_]{20,}|\bsk-[A-Za-z0-9_-]{20,}|\bAKIA[0-9A-Z]{16}\b|\bxox[abprs]-[A-Za-z0-9-]{10,}|\bAIza[0-9A-Za-z_-]{35}|\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})",
                ),
                np(
                    "assigned_secret",
                    r#"(?i)(token|secret|password|passwd|api_?key)\s*[:=]\s*["'][^"'\s]{16,}["']"#,
                ),
                np(
                    "user_folder_path",
                    r#"(?i)([a-z]:[\\/]+users[\\/]+[^\\/\s"']+|/home/[a-z0-9._-]+/|/users/[^/\s"']+/)"#,
                ),
                np(
                    "email_address",
                    r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}",
                ),
            ],
            email_allow: strings(&[
                "@example.com",
                "@example.org",
                "@example.net",
                "@users.noreply.github.com",
                "noreply@anthropic.com",
            ]),
            max_findings_per_gate: 100,
        }
    }
}
