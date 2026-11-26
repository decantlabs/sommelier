//! Offline check of the proofs of a feature map against a folder of title profiles. Profiles live on
//! tester machines, so the hub never needs them. `sommelier feature check --profiles DIR` reads every
//! `*.toml` file of DIR that parses as a profile and lists the proofs that name a profile, a scenario
//! or a milestone that does not exist there.

use std::path::Path;

use super::FeatureMap;
use crate::tester::profile::Profile;

/// Reads the profiles of a folder. A file that is no profile is named in the second list.
pub fn read_dir(dir: &Path) -> Result<(Vec<Profile>, Vec<String>), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: cannot read the folder: {}", dir.display(), e.kind()))?;
    let mut names: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml") && p.is_file())
        .collect();
    names.sort();
    let (mut ok, mut skipped) = (Vec::new(), Vec::new());
    for p in names {
        match Profile::load(&p) {
            Ok(profile) => ok.push(profile),
            Err(e) => skipped.push(format!(
                "{}: not read as a profile: {e}",
                p.file_name().and_then(|n| n.to_str()).unwrap_or("?")
            )),
        }
    }
    Ok((ok, skipped))
}

/// One line per proof of the map that has no match in `profiles`. Empty when every proof is found.
pub fn missing(map: &FeatureMap, profiles: &[Profile]) -> Vec<String> {
    let mut out = Vec::new();
    for f in map.features() {
        for p in &f.proof {
            let what = format!("feature `{}`: proof {}", f.id, p.line());
            let Some(profile) = profiles.iter().find(|x| x.id == p.profile) else {
                out.push(format!("{what}: no profile `{}` in the folder", p.profile));
                continue;
            };
            let Some(sc) = profile.scenario(&p.scenario) else {
                out.push(format!(
                    "{what}: profile `{}` has no scenario `{}`",
                    p.profile, p.scenario
                ));
                continue;
            };
            if !sc.milestones.iter().any(|m| m.id == p.milestone) {
                out.push(format!(
                    "{what}: scenario `{}` has no milestone `{}`",
                    p.scenario, p.milestone
                ));
            }
        }
    }
    out
}
