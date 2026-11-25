//! `sommelier hello` capacity declaration command (section 5.1).

use std::process::ExitCode;

pub async fn run(_args: &[String]) -> Result<ExitCode, String> {
    super::unimplemented("hello")
}
