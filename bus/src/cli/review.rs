//! `sommelier review`: takes an assigned review and sends the verdict.
//!
//! Stub of the wave two spine. `cli/mod.rs` and the help of `main.rs` already route here.

use std::process::ExitCode;

const HELP: &str = "\
usage: sommelier review
Not built yet.
";

/// The help text of this command.
pub fn help() -> &'static str {
    HELP
}

/// Entry point of `sommelier review`.
pub async fn run(_args: &[String]) -> Result<ExitCode, String> {
    Err("command `review` is not built yet".into())
}
