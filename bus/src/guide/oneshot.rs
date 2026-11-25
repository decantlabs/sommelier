//! One shot command table and checkpoint form (sections 7.3 and 8.4).

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct OneShot {
    pub situation: &'static str,
    pub command: &'static str,
    pub output: &'static str,
    pub note: Option<&'static str>,
}

pub const CHECKPOINT_FORM: &str = "\
CHECKPOINT <task> attempt <n> <UTC time>
STATE: one line
DONE: what is finished
NEXT: the very next step
BRANCH: <branch> COMMIT: <sha> PUSHED: yes|no
OPEN: files with uncommitted work, commands still running";

pub const TABLE: &[OneShot] = &[
    OneShot {
        situation: "start",
        command: "sommelier work <task>",
        output: "ok task t42 branch codex/t42 lease until 12:40Z brief .sommelier/brief.md",
        note: None,
    },
    OneShot {
        situation: "progress",
        command: "sommelier checkpoint <task> --file .sommelier/checkpoint.md",
        output: "ok checkpoint 3 stored, lease until 13:10Z",
        note: Some(CHECKPOINT_FORM),
    },
    OneShot {
        situation: "finish",
        command: "sommelier done <task> --ref <branch> --sha <sha> --result-file <last message>",
        output: "ok delivered, queued for landing at position 2",
        note: None,
    },
    OneShot {
        situation: "stuck",
        command: "sommelier stuck <task> --why \"<facts>\"",
        output: "ok stuck recorded, alert a17 to the coordinator",
        note: None,
    },
    OneShot {
        situation: "quota",
        command: "sommelier quota hit --vendor <v> --text-file <error file> --task <task>",
        output: "pause gemini-4-carbon until 14:02Z, task requeued (code 5)",
        note: Some(
            "Before stopping on quota/rate limit: commit WIP, push wip/<task> to origin, and post STUCK with quota % + branch/SHA.",
        ),
    },
    OneShot {
        situation: "context",
        command: "sommelier handover <task> --file .sommelier/checkpoint.md",
        output: "ok handed over, attempt 2 is ready with your note",
        note: None,
    },
    OneShot {
        situation: "branch",
        command: "sommelier land sweep --report <ref> --note \"<what it is>\"",
        output: "ok branch recorded as orphan, triage task t77",
        note: None,
    },
    OneShot {
        situation: "run",
        command: "sommelier run request --sha <sha> --secs 150 --need os=windows",
        output: "ok run r311 queued behind 1, slot on owner",
        note: None,
    },
    OneShot {
        situation: "ask",
        command: "sommelier ask --task <task> \"<question>\"",
        output: "ok consult c9 sent to the brain",
        note: None,
    },
    OneShot {
        situation: "queue",
        command: "sommelier next --peek",
        output: "3 ready for you, top: t51 fix decant-d3d12x views (blocker), or code 6",
        note: None,
    },
];

pub fn lookup(situation: &str) -> Option<&'static OneShot> {
    let key = situation.trim().to_ascii_lowercase();
    TABLE.iter().find(|item| item.situation == key)
}

pub fn format_text(item: &OneShot) -> String {
    let mut out = format!("{}\n-> {}", item.command, item.output);
    if let Some(note) = item.note {
        out.push_str("\n\n");
        out.push_str(note);
    }
    out.push('\n');
    out
}
