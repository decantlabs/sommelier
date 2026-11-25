//! Command line modules for all sommelier command groups (section 5.1).

use std::process::ExitCode;

pub mod agent;
pub mod alerts;
pub mod doctor;
pub mod drill;
pub mod guide;
pub mod handover;
pub mod hello;
pub mod key;
pub mod land;
pub mod metric;
pub mod plan;
pub mod promote;
pub mod quota;
pub mod role;
pub mod run;
pub mod standby;
pub mod state;
pub mod task;
pub mod work;

fn unimplemented(cmd: &str) -> Result<ExitCode, String> {
    Err(format!("command `{cmd}` is not implemented yet"))
}

pub async fn dispatch(cmd: &str, args: &[String]) -> Option<Result<ExitCode, String>> {
    match cmd {
        "guide" => Some(guide::run(args).await),
        "doctor" => Some(doctor::run(args).await),
        "hello" => Some(hello::run(args).await),
        "task" => Some(task::run(args).await),
        "next" | "work" | "checkpoint" | "done" | "stuck" | "decline" | "ask" | "offers" => {
            Some(work::run(cmd, args).await)
        }
        "handover" => Some(handover::run(args).await),
        "agent" | "watch" => Some(agent::run(cmd, args).await),
        "quota" => Some(quota::run(args).await),
        "plan" | "sim" | "idle" | "pause" | "resume" => Some(plan::run(cmd, args).await),
        "land" | "scan" => Some(land::run(cmd, args).await),
        "run" => Some(run::run(args).await),
        "metric" => Some(metric::run(args).await),
        "alerts" | "alert" | "digest" => Some(alerts::run(cmd, args).await),
        "snapshot" | "replay" => Some(state::run(cmd, args).await),
        "role" => Some(role::run(args).await),
        "standby" => Some(standby::run(args).await),
        "promote" => Some(promote::run(args).await),
        "drill" => Some(drill::run(args).await),
        "join" | "key" => Some(key::run(cmd, args).await),
        _ => None,
    }
}
