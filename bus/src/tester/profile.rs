//! Title profiles and the local title check.
//!
//! A title profile is a public TOML file: a neutral name, the fingerprint of the executable and the
//! scenarios to run. The factory never distributes title files. [`check_title`] compares the tester's own
//! copy with the fingerprint and only says yes or no.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Largest compiled size of a milestone pattern. It keeps a hostile profile from costing memory.
const PATTERN_SIZE_LIMIT: usize = 1 << 20;

/// The fingerprint of the executable of the title.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Executable {
    /// File name only, no folder.
    pub file: String,
    /// SHA 256 of the file, 64 lower case hex digits.
    pub sha256: String,
    /// Size of the file in bytes.
    pub size: u64,
}

/// A key press at a second of the scenario.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Second after the start of the run.
    pub at: u32,
    /// Key name, for example `enter`.
    pub key: String,
}

/// Something the log must show, and the second by which it must show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Milestone {
    pub id: String,
    /// Regular expression searched in every log line.
    pub pattern: String,
    /// The line must appear at this second of the run or earlier.
    pub by: u32,
}

/// One scenario of a title: what to do and what to look for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    /// How long the title runs.
    pub seconds: u32,
    #[serde(default)]
    pub inputs: Vec<Input>,
    pub milestones: Vec<Milestone>,
    /// Names of the values to read from the summary of the run.
    #[serde(default)]
    pub metrics: Vec<String>,
}

/// A title profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    /// A neutral name of the title, not a brand claim.
    pub name: String,
    pub executable: Executable,
    pub scenarios: Vec<Scenario>,
}

/// A plain id: lower case letters, digits, `_` and `-`, starting with a letter or digit, 64 characters at most.
fn plain_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn need_id(field: &str, v: &str) -> Result<(), String> {
    if plain_id(v) {
        Ok(())
    } else {
        Err(format!(
            "{field}: `{v}` is not a plain id: use lower case letters, digits, `_` and `-`, 64 characters at most"
        ))
    }
}

impl Profile {
    /// Reads and validates a profile file.
    pub fn load(path: &Path) -> Result<Profile, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the profile file: {}", e.kind()))?;
        Profile::parse(&text)
    }

    /// Parses and validates profile text.
    pub fn parse(text: &str) -> Result<Profile, String> {
        let p: Profile = toml::from_str(text).map_err(|e| format!("profile: {}", e.message()))?;
        p.validate()?;
        Ok(p)
    }

    /// The scenario with this id.
    pub fn scenario(&self, id: &str) -> Option<&Scenario> {
        self.scenarios.iter().find(|s| s.id == id)
    }

    /// Checks every rule. The error names the field that breaks it.
    pub fn validate(&self) -> Result<(), String> {
        need_id("id", &self.id)?;
        if self.name.trim().is_empty() || self.name.len() > 100 {
            return Err("name: write a short neutral name of the title".into());
        }
        let e = &self.executable;
        if e.file.is_empty()
            || e.file.len() > 255
            || e.file.contains(['/', '\\', ':'])
            || e.file == "."
            || e.file == ".."
        {
            return Err("executable.file: give the file name only, without a folder".into());
        }
        if e.sha256.len() != 64
            || !e
                .sha256
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err("executable.sha256: give 64 lower case hex digits".into());
        }
        if e.size == 0 {
            return Err("executable.size: give the size in bytes, more than 0".into());
        }
        if self.scenarios.is_empty() {
            return Err("scenarios: a profile needs at least one scenario".into());
        }
        for (i, s) in self.scenarios.iter().enumerate() {
            s.validate(i)?;
            if self.scenarios[..i].iter().any(|o| o.id == s.id) {
                return Err(format!("scenarios[{i}].id: `{}` is used twice", s.id));
            }
        }
        Ok(())
    }
}

impl Scenario {
    fn validate(&self, i: usize) -> Result<(), String> {
        let at = format!("scenarios[{i}]");
        need_id(&format!("{at}.id"), &self.id)?;
        if self.seconds == 0 || self.seconds > 3600 {
            return Err(format!("{at}.seconds: give 1 to 3600 seconds"));
        }
        for (k, inp) in self.inputs.iter().enumerate() {
            if inp.at > self.seconds {
                return Err(format!(
                    "{at}.inputs[{k}].at: {} is after the end of the run ({} s)",
                    inp.at, self.seconds
                ));
            }
            need_id(&format!("{at}.inputs[{k}].key"), &inp.key)?;
        }
        if self.milestones.is_empty() {
            return Err(format!(
                "{at}.milestones: a scenario needs at least one milestone"
            ));
        }
        for (k, m) in self.milestones.iter().enumerate() {
            let mf = format!("{at}.milestones[{k}]");
            need_id(&format!("{mf}.id"), &m.id)?;
            if self.milestones[..k].iter().any(|o| o.id == m.id) {
                return Err(format!("{mf}.id: `{}` is used twice", m.id));
            }
            if m.pattern.is_empty() {
                return Err(format!("{mf}.pattern: the pattern is empty"));
            }
            if let Err(e) = RegexBuilder::new(&m.pattern)
                .size_limit(PATTERN_SIZE_LIMIT)
                .build()
            {
                let first = e
                    .to_string()
                    .lines()
                    .last()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                return Err(format!("{mf}.pattern: does not compile: {first}"));
            }
            if m.by == 0 || m.by > self.seconds {
                return Err(format!(
                    "{mf}.by: give a second from 1 to the length of the run ({} s)",
                    self.seconds
                ));
            }
        }
        for (k, name) in self.metrics.iter().enumerate() {
            need_id(&format!("{at}.metrics[{k}]"), name)?;
        }
        Ok(())
    }
}

/// Checks the local copy of the title against the profile. Yes or no, nothing else leaves this function.
///
/// The size is compared first, then the SHA 256 of the file, which is read as a stream. An error means
/// the file could not be read. It names no path and no hash.
pub fn check_title(profile: &Profile, exe: &Path) -> Result<bool, String> {
    let unreadable = |e: std::io::Error| {
        format!(
            "cannot read the title executable ({}): check the path of your copy",
            e.kind()
        )
    };
    let mut f = File::open(exe).map_err(unreadable)?;
    let meta = f.metadata().map_err(unreadable)?;
    if !meta.is_file() || meta.len() != profile.executable.size {
        return Ok(false);
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = f.read(&mut buf).map_err(unreadable)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buf[..n]);
    }
    if total != profile.executable.size {
        return Ok(false);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex == profile.executable.sha256)
}
