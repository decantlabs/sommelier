//! Tests of the clean build module: requests, parsers, the state machine, the queue and the templates.

use std::collections::BTreeSet;
use std::path::PathBuf;

use sommelier::verify::{
    Action, ApiError, Conclusion, DispatchInputs, Entry, Event, FakeGitHub, GitHubApi, Phase,
    ReqwestGitHub, Timers, VerifyRules, VerifyState, conclusion, dispatch_request, fact_topic,
    find_run, job_minutes, jobs_request, list_runs_request, next_to_verify, parse_jobs, parse_run,
    parse_runs, run_name, run_request, step,
};

const FAKE_TOKEN: &str = "ghp_FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE0001";
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/verify")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn inputs() -> DispatchInputs {
    DispatchInputs {
        submission: A.into(),
        order: "t412".into(),
        base: "0123456789abcdef0123456789abcdef01234567".into(),
        hub: "https://hub.example.test".into(),
        attempt: 1,
        dispatch: 1,
        round: 1000,
    }
}

// ---- dispatch and requests ----

#[test]
fn dispatch_request_has_expected_url_and_body() {
    let r = dispatch_request(
        "example-org/project",
        "factory-verify.yml",
        "main",
        &inputs(),
    );
    assert_eq!(r.method, "POST");
    assert_eq!(
        r.url,
        "https://api.github.com/repos/example-org/project/actions/workflows/factory-verify.yml/dispatches"
    );
    let body = r.body.expect("body");
    assert_eq!(body["ref"], "main");
    let i = &body["inputs"];
    assert_eq!(i["submission"], A);
    assert_eq!(i["order"], "t412");
    assert_eq!(i["base"], "0123456789abcdef0123456789abcdef01234567");
    assert_eq!(i["hub"], "https://hub.example.test");
    assert_eq!(i["attempt"], "1");
    assert_eq!(i["dispatch"], "1");
    assert_eq!(i["round"], "1000");
    assert_eq!(i.as_object().unwrap().len(), 7);
}

#[test]
fn requests_carry_no_token_or_authorization_header() {
    let all = [
        dispatch_request("o/r", "factory-verify.yml", "main", &inputs()),
        list_runs_request("o/r", "factory-verify.yml"),
        run_request("o/r", 7),
        jobs_request("o/r", 7),
    ];
    for r in all {
        for (k, v) in &r.headers {
            assert!(!k.eq_ignore_ascii_case("authorization"), "header {k}");
            assert!(!v.contains(FAKE_TOKEN));
            assert!(!v.to_ascii_lowercase().contains("bearer"));
        }
        assert!(!format!("{r:?}").contains(FAKE_TOKEN));
    }
}

#[test]
fn urls_are_percent_encoded_per_segment() {
    let r = dispatch_request("o/r", "a b/../c.yml", "main", &inputs());
    assert!(
        r.url.contains("/workflows/a%20b%2F..%2Fc.yml/dispatches"),
        "{}",
        r.url
    );
    assert!(
        list_runs_request("o/r", "f.yml")
            .url
            .ends_with("event=workflow_dispatch&per_page=100")
    );
    assert!(
        jobs_request("o/r", 9)
            .url
            .ends_with("/runs/9/jobs?filter=latest&per_page=100")
    );
}

// ---- parsers ----

#[test]
fn runs_list_parses_and_find_run_matches_the_whole_run_name() {
    let runs = parse_runs(&fixture("runs_list.json")).unwrap();
    assert_eq!(runs.len(), 4);
    let name = |sha: &str, a: u32, d: u32, t: u64| run_name(sha, a, d, t);
    assert_eq!(
        find_run(&runs, &name(A, 1, 1, 1000)).map(|r| r.id),
        Some(1002)
    );
    assert_eq!(
        find_run(&runs, &name(B, 1, 1, 1000)).map(|r| r.id),
        Some(1003)
    );
    assert_eq!(
        find_run(&runs, &name(C, 1, 1, 1000)).map(|r| r.id),
        Some(1004)
    );
    assert!(find_run(&runs, &name(&"d".repeat(64), 1, 1, 1000)).is_none());
    assert!(
        find_run(&runs, &name(&A[..32], 1, 1, 1000)).is_none(),
        "a prefix must not match"
    );
    // The attempt, the dispatch count and the round are part of the name.
    assert!(find_run(&runs, &name(A, 2, 1, 1000)).is_none());
    assert!(find_run(&runs, &name(A, 1, 2, 1000)).is_none());
    assert!(find_run(&runs, &name(A, 1, 1, 2000)).is_none());
    assert!(find_run(&runs, "").is_none());
}

#[test]
fn find_run_reads_the_display_title_before_the_name() {
    // GitHub puts the workflow name into `name` and the `run-name` into `display_title`.
    let mut runs = parse_runs(&fixture("runs_list.json")).unwrap();
    assert_eq!(runs[2].name.as_deref(), Some("factory-verify"));
    assert_eq!(
        find_run(&runs, &inputs().run_name()).map(|r| r.id),
        Some(1002)
    );
    // A hostile name field does not make a run match: the title decides.
    runs[0].name = Some(inputs().run_name());
    assert_eq!(
        find_run(&runs, &inputs().run_name()).map(|r| r.id),
        Some(1002)
    );
}

#[test]
fn find_run_takes_the_newest_match_and_never_the_run_of_an_earlier_round() {
    let mut runs = parse_runs(&fixture("runs_list.json")).unwrap();
    // The failed run of the first round and the new run of the retry round.
    let mut retry = runs[2].clone();
    retry.id = 5000;
    retry.display_title = Some(run_name(A, 1, 1, 9000));
    runs.push(retry);
    assert_eq!(
        find_run(&runs, &run_name(A, 1, 1, 1000)).map(|r| r.id),
        Some(1002)
    );
    assert_eq!(
        find_run(&runs, &run_name(A, 1, 1, 9000)).map(|r| r.id),
        Some(5000)
    );
    // Before the retry run shows up, the old failed run is not taken for it.
    runs.pop();
    assert!(find_run(&runs, &run_name(A, 1, 1, 9000)).is_none());
    // Two runs with the same name: the newest wins.
    let mut dup = runs[2].clone();
    dup.id = 7000;
    runs.push(dup);
    assert_eq!(
        find_run(&runs, &run_name(A, 1, 1, 1000)).map(|r| r.id),
        Some(7000)
    );
}

#[test]
fn parse_errors_are_one_clear_line() {
    for e in [
        parse_runs("{}").unwrap_err(),
        parse_run("[]").unwrap_err(),
        parse_jobs("nope").unwrap_err(),
    ] {
        assert!(e.starts_with("invalid "), "{e}");
        assert!(!e.contains('\n'));
    }
}

fn concl(run: &str, jobs: &str) -> Conclusion {
    let run = parse_run(&fixture(run)).unwrap();
    let jobs = parse_jobs(&fixture(jobs)).unwrap();
    conclusion(&run, &jobs)
}

#[test]
fn conclusion_for_every_state() {
    assert_eq!(
        concl("run_queued.json", "jobs_running.json"),
        Conclusion::Queued
    );
    assert_eq!(
        concl("run_running.json", "jobs_running.json"),
        Conclusion::Running
    );
    assert_eq!(
        concl("run_success.json", "jobs_success.json"),
        Conclusion::Passed
    );
    assert_eq!(
        concl("run_cancelled.json", "jobs_success.json"),
        Conclusion::Cancelled
    );
    assert_eq!(
        concl("run_timed_out.json", "jobs_timed_out.json"),
        Conclusion::TimedOut
    );
}

#[test]
fn failed_step_in_the_middle_of_a_job_is_named() {
    assert_eq!(
        concl("run_failure.json", "jobs_failed_middle.json"),
        Conclusion::Failed {
            step: Some("Build and test touched crates".into())
        }
    );
}

#[test]
fn failure_without_jobs_and_job_timeout_are_read_closed() {
    assert_eq!(
        conclusion(&parse_run(&fixture("run_failure.json")).unwrap(), &[]),
        Conclusion::Failed { step: None }
    );
    // The run says failure, a step says timed out: the run timed out.
    assert_eq!(
        concl("run_failure.json", "jobs_timed_out.json"),
        Conclusion::TimedOut
    );
}

#[test]
fn unknown_completed_conclusion_counts_as_failed() {
    let mut run = parse_run(&fixture("run_success.json")).unwrap();
    run.conclusion = Some("startup_failure".into());
    assert!(matches!(conclusion(&run, &[]), Conclusion::Failed { .. }));
    run.conclusion = None;
    assert!(matches!(conclusion(&run, &[]), Conclusion::Failed { .. }));
}

#[test]
fn job_minutes_round_up_and_skip_open_jobs() {
    assert_eq!(
        job_minutes(&parse_jobs(&fixture("jobs_success.json")).unwrap()),
        3
    );
    assert_eq!(
        job_minutes(&parse_jobs(&fixture("jobs_running.json")).unwrap()),
        0
    );
}

// ---- the state machine ----

fn timers() -> Timers {
    Timers {
        first_poll_ms: 5_000,
        max_poll_ms: 60_000,
        run_wait_ms: 120_000,
        time_limit_ms: 600_000,
    }
}

fn fresh() -> VerifyState {
    VerifyState::new(A, "t412", 0, timers())
}

fn started(now: u64) -> VerifyState {
    step(&fresh(), now, Event::Start).0
}

fn running(now: u64, run_id: u64) -> VerifyState {
    step(
        &started(0),
        now,
        Event::RunSeen {
            run_id,
            conclusion: Conclusion::Running,
            minutes: 0,
        },
    )
    .0
}

fn kinds(a: &[Action]) -> Vec<&'static str> {
    a.iter()
        .map(|x| match x {
            Action::Dispatch { .. } => "dispatch",
            Action::Poll { .. } => "poll",
            Action::Emit { .. } => "emit",
        })
        .collect()
}

struct Case {
    name: &'static str,
    state: VerifyState,
    now: u64,
    event: Event,
    phase: Phase,
    actions: Vec<&'static str>,
}

fn seen(c: Conclusion, minutes: u32) -> Event {
    Event::RunSeen {
        run_id: 77,
        conclusion: c,
        minutes,
    }
}

#[test]
fn step_transition_table() {
    let polled = step(&started(0), 5_000, Event::Tick).0; // polls = 1, next poll at 10_000
    let cases = vec![
        Case {
            name: "start dispatches",
            state: fresh(),
            now: 1_000,
            event: Event::Start,
            phase: Phase::Dispatched,
            actions: vec!["dispatch", "emit"],
        },
        Case {
            name: "tick before start does nothing",
            state: fresh(),
            now: 1_000,
            event: Event::Tick,
            phase: Phase::Queued,
            actions: vec![],
        },
        Case {
            name: "run seen before start is ignored",
            state: fresh(),
            now: 1_000,
            event: seen(Conclusion::Passed, 1),
            phase: Phase::Queued,
            actions: vec![],
        },
        Case {
            name: "tick too early does not poll",
            state: started(0),
            now: 4_999,
            event: Event::Tick,
            phase: Phase::Dispatched,
            actions: vec![],
        },
        Case {
            name: "tick on time polls",
            state: started(0),
            now: 5_000,
            event: Event::Tick,
            phase: Phase::Dispatched,
            actions: vec!["poll"],
        },
        Case {
            name: "poll again only after back off",
            state: polled.clone(),
            now: 9_999,
            event: Event::Tick,
            phase: Phase::Dispatched,
            actions: vec![],
        },
        Case {
            name: "second poll after 5 s",
            state: polled.clone(),
            now: 10_000,
            event: Event::Tick,
            phase: Phase::Dispatched,
            actions: vec!["poll"],
        },
        Case {
            name: "poll failure backs off further",
            state: polled.clone(),
            now: 6_000,
            event: Event::PollFailed,
            phase: Phase::Dispatched,
            actions: vec![],
        },
        Case {
            name: "no run found keeps waiting",
            state: started(0),
            now: 6_000,
            event: Event::RunNotFound,
            phase: Phase::Dispatched,
            actions: vec![],
        },
        Case {
            name: "queued run moves to running and emits",
            state: started(0),
            now: 8_000,
            event: seen(Conclusion::Queued, 0),
            phase: Phase::Running,
            actions: vec!["emit"],
        },
        Case {
            name: "running run moves to running and emits",
            state: started(0),
            now: 8_000,
            event: seen(Conclusion::Running, 0),
            phase: Phase::Running,
            actions: vec!["emit"],
        },
        Case {
            name: "second running sighting is silent",
            state: running(8_000, 77),
            now: 20_000,
            event: seen(Conclusion::Running, 1),
            phase: Phase::Running,
            actions: vec![],
        },
        Case {
            name: "passed ends and emits",
            state: running(8_000, 77),
            now: 90_000,
            event: seen(Conclusion::Passed, 2),
            phase: Phase::Done,
            actions: vec!["emit"],
        },
        Case {
            name: "failed ends and emits",
            state: running(8_000, 77),
            now: 90_000,
            event: seen(
                Conclusion::Failed {
                    step: Some("Run done condition".into()),
                },
                2,
            ),
            phase: Phase::Done,
            actions: vec!["emit"],
        },
        Case {
            name: "cancelled ends",
            state: running(8_000, 77),
            now: 90_000,
            event: seen(Conclusion::Cancelled, 1),
            phase: Phase::Done,
            actions: vec!["emit"],
        },
        Case {
            name: "timed out run ends",
            state: running(8_000, 77),
            now: 90_000,
            event: seen(Conclusion::TimedOut, 20),
            phase: Phase::Done,
            actions: vec!["emit"],
        },
        Case {
            name: "final run seen straight after dispatch ends",
            state: started(0),
            now: 9_000,
            event: seen(Conclusion::Passed, 1),
            phase: Phase::Done,
            actions: vec!["emit"],
        },
        Case {
            name: "dispatch failure retries once at once",
            state: started(0),
            now: 2_000,
            event: Event::DispatchFailed,
            phase: Phase::Dispatched,
            actions: vec!["dispatch"],
        },
        Case {
            name: "dispatch without run is retried after the wait",
            state: started(0),
            now: 120_000,
            event: Event::Tick,
            phase: Phase::Dispatched,
            actions: vec!["dispatch"],
        },
        Case {
            name: "time limit ends a running submission",
            state: running(8_000, 77),
            now: 600_000,
            event: Event::Tick,
            phase: Phase::Done,
            actions: vec!["emit"],
        },
        Case {
            name: "time limit ends a dispatched submission",
            state: started(0),
            now: 600_000,
            event: Event::Tick,
            phase: Phase::Done,
            actions: vec!["emit"],
        },
        Case {
            name: "running polls with the run id",
            state: running(8_000, 77),
            now: 13_000,
            event: Event::Tick,
            phase: Phase::Running,
            actions: vec!["poll"],
        },
    ];
    assert!(cases.len() >= 15);
    for c in cases {
        let (s, a) = step(&c.state, c.now, c.event);
        assert_eq!(s.phase, c.phase, "{}", c.name);
        assert_eq!(kinds(&a), c.actions, "{}", c.name);
    }
}

#[test]
fn done_ignores_every_event() {
    let done = step(&running(8_000, 77), 90_000, seen(Conclusion::Passed, 2)).0;
    for ev in [
        Event::Start,
        Event::Tick,
        Event::DispatchFailed,
        Event::RunNotFound,
        Event::PollFailed,
        seen(Conclusion::Failed { step: None }, 9),
    ] {
        let (s, a) = step(&done, 999_999_999, ev);
        assert_eq!(s, done);
        assert!(a.is_empty());
    }
}

#[test]
fn backoff_doubles_and_is_capped() {
    let mut s = started(0);
    let mut now = 0;
    let mut waits = Vec::new();
    for _ in 0..7 {
        now = s.next_poll_at;
        let (n, a) = step(&s, now, Event::Tick);
        assert_eq!(kinds(&a), ["poll"]);
        waits.push(n.next_poll_at - now);
        s = n;
        // Keep the dispatch fresh so the run wait never fires in this test.
        s.dispatched_at = Some(now);
        s.started_at = Some(now);
    }
    assert_eq!(
        waits,
        [5_000, 10_000, 20_000, 40_000, 60_000, 60_000, 60_000]
    );
    let _ = now;
}

#[test]
fn poll_failure_pushes_the_next_poll_out() {
    let s = step(&started(0), 5_000, Event::Tick).0; // next at 10_000
    let f = step(&s, 6_000, Event::PollFailed).0; // polls 2 -> wait 10 s from now
    assert_eq!(f.next_poll_at, 6_000 + 10_000);
}

#[test]
fn a_lost_dispatch_is_retried_exactly_once_then_no_run() {
    let s1 = started(0);
    let (s2, a) = step(&s1, 120_000, Event::Tick);
    assert_eq!(kinds(&a), ["dispatch"]);
    assert_eq!(s2.dispatches, 2);
    assert!(matches!(&a[0], Action::Dispatch { attempt: 2, .. }));
    // The second dispatch is lost too.
    let (s3, a) = step(&s2, 240_000, Event::Tick);
    assert_eq!(s3.phase, Phase::Done);
    assert_eq!(s3.conclusion.as_deref(), Some("no_run"));
    assert_eq!(kinds(&a), ["emit"]);
    // A dispatch failure on the second attempt also ends it.
    let (s4, _) = step(&s2, 130_000, Event::DispatchFailed);
    assert_eq!(s4.conclusion.as_deref(), Some("no_run"));
}

#[test]
fn time_limit_counts_from_the_first_dispatch_and_minutes_round_up() {
    let (s, a) = step(&running(8_000, 77), 600_000, Event::Tick);
    assert_eq!(s.conclusion.as_deref(), Some("timed_out"));
    let Action::Emit { topic, body } = &a[0] else {
        panic!("emit")
    };
    assert_eq!(topic, &format!("verify/{A}"));
    assert_eq!(body.minutes, 10);
    assert_eq!(body.run_id, Some(77));
}

#[test]
fn fact_body_has_the_defined_fields() {
    let (s, a) = step(
        &running(8_000, 77),
        90_000,
        seen(
            Conclusion::Failed {
                step: Some("Build and test touched crates".into()),
            },
            4,
        ),
    );
    let Action::Emit { topic, body } = &a[0] else {
        panic!("emit")
    };
    assert_eq!(topic, &fact_topic(A));
    let v = serde_json::to_value(body).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "v": 1,
            "submission": A,
            "run_id": 77,
            "state": "done",
            "conclusion": "failed",
            "failing_step": "Build and test touched crates",
            "minutes": 4
        })
    );
    assert_eq!(s.fact(), *body);
    // The state survives a round trip through JSON, so a restart of the hub keeps it.
    let back: VerifyState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back, s);
}

#[test]
fn step_is_pure_and_default_timers_are_sane() {
    let s = started(0);
    assert_eq!(step(&s, 5_000, Event::Tick), step(&s, 5_000, Event::Tick));
    let t = Timers::default();
    assert!(t.first_poll_ms < t.max_poll_ms && t.run_wait_ms < t.time_limit_ms);
}

// ---- the queue ----

fn e(sub: &str, tier: u8, member: &str, block: &str, at: u64) -> Entry {
    Entry {
        submission: sub.into(),
        tier,
        member: member.into(),
        block: block.into(),
        queued_at: at,
    }
}

#[test]
fn default_rules_are_16_and_20_percent() {
    let r = VerifyRules::default();
    assert_eq!(
        (r.max_concurrent, r.tier0_share_percent, r.tier0_cap()),
        (16, 20, 3)
    );
}

#[test]
fn tier0_never_exceeds_its_share_with_a_thousand_pending() {
    let pending: Vec<Entry> = (0..1000)
        .map(|i| {
            e(
                &format!("n{i:04}"),
                0,
                &format!("m{}", i % 50),
                &format!("b{}", i % 7),
                i,
            )
        })
        .collect();
    let rules = VerifyRules::default();
    let picked = next_to_verify(&pending, &[], &rules);
    assert_eq!(picked.len(), 3);
    // With tier 0 already running, only the rest of the share is open.
    let running: Vec<Entry> = (0..2)
        .map(|i| e(&format!("r{i}"), 0, "mx", "bx", 0))
        .collect();
    assert_eq!(next_to_verify(&pending, &running, &rules).len(), 1);
    let running: Vec<Entry> = (0..3)
        .map(|i| e(&format!("r{i}"), 0, "mx", "bx", 0))
        .collect();
    assert!(next_to_verify(&pending, &running, &rules).is_empty());
}

#[test]
fn other_tiers_fill_the_rest_oldest_first() {
    let mut pending: Vec<Entry> = (0..40)
        .map(|i| e(&format!("t1_{i:02}"), 1, "m", "b", 100 + i))
        .collect();
    pending.push(e("t0_a", 0, "x", "bx", 1));
    let picked = next_to_verify(&pending, &[], &VerifyRules::default());
    assert_eq!(picked.len(), 16);
    assert!(picked.contains(&"t0_a".to_owned()));
    let tier1: Vec<_> = picked.iter().filter(|s| s.starts_with("t1_")).collect();
    assert_eq!(tier1.len(), 15);
    assert_eq!(tier1[0], "t1_00");
    assert_eq!(tier1[14], "t1_14");
}

#[test]
fn free_capacity_is_max_minus_running() {
    let pending: Vec<Entry> = (0..30)
        .map(|i| e(&format!("p{i:02}"), 2, "m", "b", i))
        .collect();
    let running: Vec<Entry> = (0..14)
        .map(|i| e(&format!("r{i:02}"), 2, "m", "b", 0))
        .collect();
    assert_eq!(
        next_to_verify(&pending, &running, &VerifyRules::default()).len(),
        2
    );
    let full: Vec<Entry> = (0..16)
        .map(|i| e(&format!("r{i:02}"), 2, "m", "b", 0))
        .collect();
    assert!(next_to_verify(&pending, &full, &VerifyRules::default()).is_empty());
}

#[test]
fn one_busy_member_does_not_starve_another() {
    let mut pending: Vec<Entry> = (0..50)
        .map(|i| e(&format!("big{i:02}"), 0, "busy", "b1", i))
        .collect();
    pending.push(e("small", 0, "quiet", "b2", 1000));
    let picked = next_to_verify(&pending, &[], &VerifyRules::default());
    assert_eq!(picked.len(), 3);
    assert!(picked.contains(&"small".to_owned()), "{picked:?}");
    // The same member in the same block, as a second key of one block, still gets a turn.
    let mut same_block: Vec<Entry> = (0..50)
        .map(|i| e(&format!("big{i:02}"), 0, "busy", "b1", i))
        .collect();
    same_block.push(e("small", 0, "quiet", "b1", 1000));
    let picked = next_to_verify(&same_block, &[], &VerifyRules::default());
    assert!(picked.contains(&"small".to_owned()), "{picked:?}");
}

#[test]
fn two_members_of_one_block_share_one_turn() {
    // Block b1 holds two members with many entries, block b2 holds one member. Three slots.
    let mut pending = Vec::new();
    for i in 0..10 {
        pending.push(e(&format!("a1_{i}"), 0, "ma", "b1", i));
        pending.push(e(&format!("a2_{i}"), 0, "mb", "b1", i));
        pending.push(e(&format!("c_{i}"), 0, "mc", "b2", 100 + i));
    }
    let picked = next_to_verify(&pending, &[], &VerifyRules::default());
    let in_b1 = picked.iter().filter(|s| s.starts_with('a')).count();
    let in_b2 = picked.iter().filter(|s| s.starts_with("c_")).count();
    assert_eq!((in_b1, in_b2), (2, 1), "{picked:?}");
    // Inside the block both members were served once each.
    assert!(picked.contains(&"a1_0".to_owned()) && picked.contains(&"a2_0".to_owned()));
}

#[test]
fn running_tier0_work_counts_against_the_fair_share() {
    let pending = vec![e("b1_new", 0, "ma", "b1", 1), e("b2_new", 0, "mc", "b2", 2)];
    let running = vec![e("r1", 0, "ma", "b1", 0), e("r2", 0, "ma", "b1", 0)];
    // One slot is left. Block b2 has no run, so it goes first although its entry is newer.
    let picked = next_to_verify(&pending, &running, &VerifyRules::default());
    assert_eq!(picked, ["b2_new"]);
}

#[test]
fn next_to_verify_is_deterministic_and_skips_running_and_duplicates() {
    let pending = vec![
        e("s2", 0, "m2", "b2", 5),
        e("s1", 0, "m1", "b1", 5),
        e("s1", 0, "m1", "b1", 5),
        e("s3", 1, "m3", "b3", 5),
        e("s4", 1, "m3", "b3", 5),
    ];
    let running = vec![e("s4", 1, "m3", "b3", 0)];
    let rules = VerifyRules::default();
    let first = next_to_verify(&pending, &running, &rules);
    let mut reversed = pending.clone();
    reversed.reverse();
    assert_eq!(first, next_to_verify(&reversed, &running, &rules));
    assert_eq!(first, ["s1", "s2", "s3"]);
    let unique: BTreeSet<_> = first.iter().collect();
    assert_eq!(unique.len(), first.len());
}

#[test]
fn zero_share_and_zero_capacity_start_nothing_of_tier0() {
    let pending = vec![e("s1", 0, "m", "b", 1)];
    let none = VerifyRules {
        max_concurrent: 16,
        tier0_share_percent: 0,
    };
    assert!(next_to_verify(&pending, &[], &none).is_empty());
    let zero = VerifyRules {
        max_concurrent: 0,
        tier0_share_percent: 20,
    };
    assert!(next_to_verify(&pending, &[], &zero).is_empty());
}

// ---- the real implementation against a local server, and the token ----

/// Serves one canned answer per connection on a loopback port and returns what it received.
async fn serve(answers: Vec<(u16, String)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let h = tokio::spawn(async move {
        let mut seen = Vec::new();
        for (code, body) in answers {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
            let reason = if code == 204 { "No Content" } else { "X" };
            let resp = format!(
                "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
        }
        seen
    });
    (root, h)
}

#[tokio::test]
async fn reqwest_impl_sends_the_token_only_as_a_header_and_never_echoes_it() {
    let echo = format!("{{\"message\":\"Bad credentials {FAKE_TOKEN}\"}}");
    let run = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/verify/run_success.json"),
    )
    .unwrap();
    let (root, h) = serve(vec![(204, String::new()), (401, echo), (200, run)]).await;
    let api = ReqwestGitHub::with_root(FAKE_TOKEN, &root).unwrap();
    assert!(!format!("{api:?}").contains(FAKE_TOKEN));

    api.dispatch("o/r", "factory-verify.yml", "main", &inputs())
        .await
        .unwrap();
    let err = api
        .list_runs("o/r", "factory-verify.yml")
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Status { code: 401, .. }));
    let text = err.to_string();
    assert!(!text.contains(FAKE_TOKEN), "{text}");
    assert!(!format!("{err:?}").contains(FAKE_TOKEN));
    let r = api.get_run("o/r", 2001).await.unwrap();
    assert_eq!(r.status, "completed");

    let seen = h.await.unwrap();
    assert!(
        seen[0].starts_with("POST /repos/o/r/actions/workflows/factory-verify.yml/dispatches"),
        "{}",
        seen[0]
    );
    assert!(
        seen[0].contains(&format!("bearer {FAKE_TOKEN}"))
            || seen[0].contains(&format!("Bearer {FAKE_TOKEN}"))
    );
    assert!(seen[0].contains(A), "body carries the submission");
    assert!(
        !seen[0].lines().next().unwrap().contains(FAKE_TOKEN),
        "token never in the URL"
    );
}

#[tokio::test]
async fn transport_error_text_has_no_token() {
    // Nothing listens on this port after the listener is dropped.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let root = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    let api = ReqwestGitHub::with_root(FAKE_TOKEN, &root).unwrap();
    let err = api.get_run("o/r", 1).await.unwrap_err();
    assert!(matches!(err, ApiError::Transport(_)));
    assert!(!err.to_string().contains(FAKE_TOKEN));
}

#[tokio::test]
async fn fake_github_drives_the_machine_end_to_end() {
    let gh = FakeGitHub::new();
    let mut state = VerifyState::new(A, "t412", 0, timers());
    let mut now = 0u64;
    let mut pending = vec![Event::Start];
    let mut log = Vec::new();
    for _ in 0..40 {
        let ev = if pending.is_empty() {
            Event::Tick
        } else {
            pending.remove(0)
        };
        let (next, actions) = step(&state, now, ev);
        state = next;
        for a in actions {
            match a {
                Action::Dispatch { .. } => {
                    gh.dispatch("o/r", "factory-verify.yml", "main", &inputs())
                        .await
                        .unwrap();
                    // The run shows up after the first dispatch, still in progress at first.
                    gh.set_runs(parse_runs(&fixture("runs_list.json")).unwrap());
                }
                Action::Poll { run_id, .. } => {
                    let runs = gh.list_runs("o/r", "factory-verify.yml").await.unwrap();
                    let found = match run_id {
                        Some(id) => gh.get_run("o/r", id).await.ok(),
                        None => find_run(&runs, &inputs().run_name()).cloned(),
                    };
                    match found {
                        Some(r) => {
                            let jobs = gh.jobs("o/r", r.id).await.unwrap();
                            pending.push(Event::RunSeen {
                                run_id: r.id,
                                conclusion: conclusion(&r, &jobs),
                                minutes: job_minutes(&jobs),
                            });
                        }
                        None => pending.push(Event::RunNotFound),
                    }
                }
                Action::Emit { body, .. } => log.push(body),
            }
        }
        now += 6_000;
        if state.phase == Phase::Done {
            break;
        }
    }
    assert_eq!(state.phase, Phase::Done);
    assert_eq!(state.conclusion.as_deref(), Some("failed"));
    assert_eq!(gh.dispatched().len(), 1);
    assert_eq!(log.first().map(|b| b.state.as_str()), Some("dispatched"));
    assert_eq!(log.last().map(|b| b.state.as_str()), Some("done"));
    for b in &log {
        assert!(!serde_json::to_string(b).unwrap().contains(FAKE_TOKEN));
    }
}

// ---- templates ----

fn template(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../templates/workflows")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("template {name}: {e}"))
}

const WORKFLOWS: [&str; 3] = ["factory-verify.yml", "factory-batch.yml", "release.yml"];

fn code_lines(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn every_template_has_a_top_level_permissions_block_and_no_forbidden_trigger_or_secret() {
    for f in WORKFLOWS {
        let t = template(f);
        assert!(
            t.lines().any(|l| l.starts_with("permissions:")),
            "{f}: no top level permissions"
        );
        assert!(!t.contains("secrets."), "{f}: secrets.");
        assert!(
            !t.contains("pull_request_target"),
            "{f}: pull_request_target"
        );
        let code = code_lines(&t);
        assert!(!code.contains("secrets"), "{f}: secrets in code");
        assert!(
            !code.contains("uses: ./"),
            "{f}: a local action comes from the patch"
        );
        assert!(!code.contains("--no-verify"), "{f}");
    }
}

#[test]
fn only_release_asks_for_write_permissions() {
    for f in ["factory-verify.yml", "factory-batch.yml"] {
        assert!(!code_lines(&template(f)).contains(": write"), "{f}");
    }
    let rel = template("release.yml");
    let top: Vec<&str> = rel
        .lines()
        .skip_while(|l| !l.starts_with("permissions:"))
        .skip(1)
        .take_while(|l| l.starts_with(' '))
        .collect();
    assert!(
        top.iter().all(|l| !l.contains("write")),
        "workflow level stays read only"
    );
}

#[test]
fn factory_verify_has_the_inputs_the_run_name_and_the_guards() {
    let t = template("factory-verify.yml");
    assert!(t.contains("permissions: {}"));
    assert!(t.contains(
        "run-name: factory-verify ${{ inputs.submission }} a${{ inputs.attempt }} d${{ inputs.dispatch }}"
    ));
    assert!(t.contains("workflow_dispatch:"));
    for input in [
        "submission:",
        "order:",
        "base:",
        "hub:",
        "attempt:",
        "dispatch:",
    ] {
        assert!(t.contains(&format!("      {input}\n")), "input {input}");
    }
    assert!(t.contains("group: factory-verify-${{ inputs.submission }}"));
    assert!(t.contains("timeout-minutes:"));
    assert!(t.contains("persist-credentials: false"));
    assert!(t.contains("sha256sum --check"));
    assert!(t.contains("core.hooksPath=/dev/null"));
    assert!(t.contains("--locked"));
    assert!(t.contains("--network none"), "the build has no network");
    assert!(
        t.contains("/api/order/$ORDER/done_when"),
        "done_when comes from the hub"
    );
    assert!(
        !t.contains("done_when.txt\" <"),
        "no redirect from the patch"
    );
    // Inputs reach scripts only through the environment.
    let code = code_lines(&t);
    let allowed = [
        "run-name:",
        "group:",
        "ref:",
        "SUBMISSION:",
        "ORDER:",
        "BASE:",
        "HUB:",
        "ATTEMPT:",
        "DISPATCH:",
    ];
    for l in code.lines().filter(|l| l.contains("${{ inputs.")) {
        assert!(
            allowed.iter().any(|p| l.trim_start().starts_with(p)),
            "input in a script: {l}"
        );
    }
    assert!(!code.contains("run: ${{ inputs"), "an input in a run line");
}

#[test]
fn factory_verify_runs_build_tests_and_done_condition_in_one_clean_container() {
    let t = template("factory-verify.yml");
    let code = code_lines(&t);
    // The ref that bundle::pack writes.
    assert!(code.contains("refs/sommelier/submit:"));
    assert!(!code.contains("refs/heads/*"), "the old refspec");
    // No sudo, no host network namespace trick, no shared environment file.
    for bad in ["sudo", "setpriv", "unshare", "GITHUB_ENV", "GITHUB_PATH"] {
        assert!(!code.contains(bad), "found {bad}");
    }
    // One docker run holds the build, the tests and the done condition.
    assert_eq!(code.matches("--network none").count(), 1);
    assert!(code.contains("env -i PATH="));
    assert!(code.contains("bash --noprofile --norc"));
    assert!(code.contains("--cap-drop ALL") && code.contains("--read-only"));
    assert!(code.contains("no-new-privileges"));
    assert!(!code.contains("--privileged") && !code.contains("docker.sock"));
    let run = code
        .find("cargo build --locked --offline")
        .expect("build line");
    assert!(code.find("cargo test --locked --offline").unwrap() > run);
    assert!(code.find("while IFS= read -r cmd").unwrap() > run);
    // The script of the container is written by the workflow, not taken from the checkout.
    assert!(code.contains("cat > \"$RUNNER_TEMP/ci/run.sh\""));
    // The done condition is JSON read with jq, fetched twice and compared.
    assert_eq!(code.matches("jq -er '.done_when").count(), 2);
    assert!(code.contains("the done condition changed during the run"));
    assert!(
        t.find("Read done condition").unwrap()
            < t.find("name: Build, test and run done condition").unwrap()
            && t.find("name: Build, test and run done condition").unwrap()
                < t.find("name: Check the done condition again").unwrap()
    );
    // The page says what a green run proves and what it does not.
    assert!(t.contains("NO proof against a hostile patch"));
    let readme = template("README.md");
    assert!(readme.contains("What a green run proves"));
    assert!(readme.contains("does NOT prove"));
}

#[test]
fn no_template_uses_a_cache_action_and_each_says_why() {
    for f in WORKFLOWS {
        let t = template(f);
        let code = code_lines(&t);
        assert!(!code.contains("actions/cache"), "{f}: a cache action");
        assert!(!code.contains("cache:"), "{f}: a cache setting");
        assert!(t.contains("cache action"), "{f}: no comment about caches");
    }
    assert!(template("README.md").contains("Never add a cache action"));
}

#[test]
fn factory_batch_is_no_lock_by_itself_and_guards_the_hold_list() {
    let t = template("factory-batch.yml");
    assert!(t.contains("THIS FILE ALONE IS NO LOCK"));
    assert!(t.contains("REQUIRED WORKFLOW"));
    assert!(t.contains("MUST STAY EQUAL"));
    // NUL separated paths with quoting off.
    assert!(t.contains("core.quotePath=false diff -z --name-only"));
    assert!(t.contains("grep -Ezq"));
    // Every glob of the hold list of the hub shows in the guarded paths.
    for needle in [
        r"AGENTS\.override\.md",
        r"CLAUDE\.local\.md",
        r"\.cursor",
        r"\.cursorrules",
        r"\.windsurfrules",
        r"\.clinerules",
        r"\.devcontainer",
        r"\.envrc",
        r"rustfmt\.toml",
        r"clippy\.toml",
        r"deny\.toml",
        r"\.claude",
        r"build\.rs",
        r"Cargo\.lock",
        r"(swarm|bus|\.github)",
    ] {
        assert!(t.contains(needle), "GUARDED_PATHS lacks {needle}");
    }
    let readme = template("README.md");
    assert!(readme.contains("Setting up the second lock"));
    assert!(readme.contains("Require workflows to pass before merging"));
    assert!(
        readme.contains("the owner of the repository sets this up")
            || readme.contains("The owner of the repository sets this up")
    );
}

#[test]
fn factory_batch_fails_closed_on_the_guarded_marker() {
    let t = template("factory-batch.yml");
    assert!(t.contains("- 'landing/**'"));
    assert!(t.contains("Factory-Guarded:"));
    assert!(t.contains("exit 1"));
    assert!(t.contains("FAIL: the maintainer signature check is not built yet"));
    assert!(t.contains("FAIL CLOSED"));
    assert!(t.contains("name: factory-batch"));
    assert!(t.contains("contents: read"));
    // The failing step comes before the project checks.
    assert!(t.find("exit 1").unwrap() < t.find("name: Format").unwrap());
}

#[test]
fn release_builds_attests_and_attaches() {
    let t = template("release.yml");
    assert!(t.contains("branches:\n      - main"));
    assert!(t.contains("attest-build-provenance"));
    assert!(t.contains("gh release create"));
    assert!(t.contains("attestations: write"));
}

#[test]
fn templates_hold_no_personal_data() {
    for f in WORKFLOWS.iter().chain(["README.md"].iter()) {
        let t = template(f);
        assert!(
            !t.contains("C:\\") && !t.contains("/Users/") && !t.contains("zijad"),
            "{f}"
        );
        assert!(!t.contains(FAKE_TOKEN));
    }
}
