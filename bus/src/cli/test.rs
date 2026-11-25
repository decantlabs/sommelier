//! `sommelier test`: runs a test order on this machine and sends the signed report.
//!
//! Stub of the wave two spine. `cli/mod.rs` and the help of `main.rs` already route here.

use std::process::ExitCode;

const HELP: &str = "\
usage: sommelier test
Not built yet.
";

/// The help text of this command.
pub fn help() -> &'static str {
    HELP
}

/// Entry point of `sommelier test`.
pub async fn run(_args: &[String]) -> Result<ExitCode, String> {
    Err("command `test` is not built yet".into())
}
