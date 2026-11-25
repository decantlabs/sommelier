//! Exit codes of the sommelier CLI (section 8.4).

use std::process::ExitCode;

pub const OK: u8 = 0;
pub const INTERNAL_ERROR: u8 = 1;
pub const WRONG_USAGE: u8 = 2;
pub const REFUSED: u8 = 3;
pub const STALE_POLICY: u8 = 4;
pub const QUOTA_PAUSE: u8 = 5;
pub const NO_WORK: u8 = 6;
pub const HUB_UNREACHABLE: u8 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitSpec {
    pub code: u8,
    pub meaning: &'static str,
    pub caller_action: &'static str,
}

pub const ALL: &[ExitSpec] = &[
    ExitSpec {
        code: OK,
        meaning: "done",
        caller_action: "go on",
    },
    ExitSpec {
        code: INTERNAL_ERROR,
        meaning: "internal error",
        caller_action: "report it, do not retry blindly",
    },
    ExitSpec {
        code: WRONG_USAGE,
        meaning: "wrong usage",
        caller_action: "read sommelier help <command>",
    },
    ExitSpec {
        code: REFUSED,
        meaning: "refused, with the reason and the fix in one line",
        caller_action: "fix what it says, do not work around it",
    },
    ExitSpec {
        code: STALE_POLICY,
        meaning: "stale policy or old binary",
        caller_action: "sommelier sync, or update",
    },
    ExitSpec {
        code: QUOTA_PAUSE,
        meaning: "quota pause until the time printed",
        caller_action: "sleep until then",
    },
    ExitSpec {
        code: NO_WORK,
        meaning: "no work now",
        caller_action: "sleep, then ask again",
    },
    ExitSpec {
        code: HUB_UNREACHABLE,
        meaning: "hub not reachable, the request is in the outbox",
        caller_action: "carry on, it is sent later",
    },
];

pub fn spec(code: u8) -> Option<&'static ExitSpec> {
    ALL.iter().find(|s| s.code == code)
}

pub fn code(c: u8) -> ExitCode {
    ExitCode::from(c)
}
