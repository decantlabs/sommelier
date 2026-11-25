//! `sommelier submit`: sends the work of a lease to the hub as a signed bundle.
//!
//! Stub of the wave two spine. `cli/mod.rs` and the help of `main.rs` already route here.

use std::process::ExitCode;

const HELP: &str = "\
usage: sommelier submit
Not built yet.
";

/// The help text of this command.
pub fn help() -> &'static str {
    HELP
}

/// Entry point of `sommelier submit`.
pub async fn run(_args: &[String]) -> Result<ExitCode, String> {
    Err("command `submit` is not built yet".into())
}
