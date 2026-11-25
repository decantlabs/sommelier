//! Tests for the `stamp` module: validation, the projection, `standing` and the pure `tier` function.

use serde_json::json;
use tempfile::TempDir;

use sommelier::msg::{Kind, Message, Role};
use sommelier::stamp::{
    KeyState, MemberFacts, Stamp, StampKind, TierRules, query_stamps, standing, tier,
    validate_stamp,
};
use sommelier::store::{Store, StoreModule};

const DAY: u64 = 86_400;
const NOW: u64 = 100 * DAY;

fn facts() -> MemberFacts {
    MemberFacts {
        member: "m1".into(),
        joined_at: NOW - 10 * DAY,
        key_state: KeyState::Active,
        linked_account: None,
        is_maintainer: false,
    }
}

/// A stamp about `m1`. `n` is the message id and keeps ids distinct.
fn st(kind: StampKind, n: u64, at: u64) -> Stamp {
    Stamp {
        id: n,
        subject: "m1".into(),
        kind,
        evidence_msg: 1000 + n,
        evidence_commit: String::new(),
        evidence_ref: String::new(),
        issuer: "boss".into(),
        at,
        msg_id: n,
    }
}

fn many(kind: StampKind, count: u64, first: u64, at: u64) -> Vec<Stamp> {
    (0..count).map(|i| st(kind, first + i, at)).collect()
}

fn cat(parts: Vec<Vec<Stamp>>) -> Vec<Stamp> {
    parts.into_iter().flatten().collect()
}

fn tier_of(f: &MemberFacts, s: &[Stamp]) -> Option<u8> {
    tier(f, s, &TierRules::default(), NOW).tier
}

fn tier2_stamps() -> Vec<Stamp> {
    cat(vec![
        many(StampKind::Landed, 25, 1, NOW - 5 * DAY),
        many(StampKind::ReviewConfirmed, 20, 100, NOW - 5 * DAY),
        vec![st(StampKind::GrantTier2, 200, NOW - DAY)],
    ])
}

fn linked() -> MemberFacts {
    MemberFacts {
        linked_account: Some("acct".into()),
        ..facts()
    }
}

#[test]
fn tier_table() {
    struct Case {
        name: &'static str,
        facts: MemberFacts,
        stamps: Vec<Stamp>,
        want: Option<u8>,
    }
    let old_penalty = |kind| vec![st(kind, 900, NOW - 40 * DAY)];
    let cases = vec![
        Case {
            name: "fresh member",
            facts: facts(),
            stamps: vec![],
            want: Some(0),
        },
        Case {
            name: "4 landed is just below",
            facts: facts(),
            stamps: many(StampKind::Landed, 4, 1, NOW - DAY),
            want: Some(0),
        },
        Case {
            name: "5 landed is just reached",
            facts: facts(),
            stamps: many(StampKind::Landed, 5, 1, NOW - DAY),
            want: Some(1),
        },
        Case {
            name: "19 run_agreed is just below",
            facts: facts(),
            stamps: many(StampKind::RunAgreed, 19, 1, NOW - DAY),
            want: Some(0),
        },
        Case {
            name: "20 run_agreed is just reached",
            facts: facts(),
            stamps: many(StampKind::RunAgreed, 20, 1, NOW - DAY),
            want: Some(1),
        },
        Case {
            name: "key one second short of 7 days",
            facts: MemberFacts {
                joined_at: NOW - 7 * DAY + 1,
                ..facts()
            },
            stamps: many(StampKind::Landed, 5, 1, NOW - DAY),
            want: Some(0),
        },
        Case {
            name: "key exactly 7 days",
            facts: MemberFacts {
                joined_at: NOW - 7 * DAY,
                ..facts()
            },
            stamps: many(StampKind::Landed, 5, 1, NOW - DAY),
            want: Some(1),
        },
        Case {
            name: "recent open reverted blocks tier 1",
            facts: facts(),
            stamps: cat(vec![
                many(StampKind::Landed, 5, 1, NOW - 50 * DAY),
                vec![st(StampKind::Reverted, 900, NOW - 29 * DAY)],
            ]),
            want: Some(0),
        },
        Case {
            name: "recent open review_missed blocks tier 1",
            facts: facts(),
            stamps: cat(vec![
                many(StampKind::Landed, 5, 1, NOW - 50 * DAY),
                vec![st(StampKind::ReviewMissed, 900, NOW - DAY)],
            ]),
            want: Some(0),
        },
        Case {
            name: "old open reverted still drops tier 1 to 0",
            facts: facts(),
            stamps: cat(vec![
                many(StampKind::Landed, 5, 1, NOW - 60 * DAY),
                old_penalty(StampKind::Reverted),
            ]),
            want: Some(0),
        },
        Case {
            name: "cleared penalty restores tier 1",
            facts: facts(),
            stamps: cat(vec![
                many(StampKind::Landed, 5, 1, NOW - 50 * DAY),
                vec![st(StampKind::Reverted, 900, NOW - 3 * DAY), {
                    let mut c = st(StampKind::Cleared, 901, NOW - DAY);
                    c.evidence_msg = 900;
                    c
                }],
            ]),
            want: Some(1),
        },
        Case {
            name: "cleared that names another message does not clear",
            facts: facts(),
            stamps: cat(vec![
                many(StampKind::Landed, 5, 1, NOW - 50 * DAY),
                vec![st(StampKind::Reverted, 900, NOW - 3 * DAY), {
                    let mut c = st(StampKind::Cleared, 901, NOW - DAY);
                    c.evidence_msg = 899;
                    c
                }],
            ]),
            want: Some(0),
        },
        Case {
            name: "cleared older than the penalty does not clear",
            facts: facts(),
            stamps: cat(vec![
                many(StampKind::Landed, 5, 1, NOW - 50 * DAY),
                vec![st(StampKind::Reverted, 900, NOW - 3 * DAY), {
                    let mut c = st(StampKind::Cleared, 901, NOW - 4 * DAY);
                    c.evidence_msg = 900;
                    c
                }],
            ]),
            want: Some(0),
        },
        Case {
            name: "tier 2 with every fact and the grant",
            facts: linked(),
            stamps: tier2_stamps(),
            want: Some(2),
        },
        Case {
            name: "tier 2 facts without the grant",
            facts: linked(),
            stamps: tier2_stamps()
                .into_iter()
                .filter(|s| s.kind != StampKind::GrantTier2)
                .collect(),
            want: Some(1),
        },
        Case {
            name: "grant without the facts",
            facts: linked(),
            stamps: cat(vec![
                many(StampKind::Landed, 24, 1, NOW - 5 * DAY),
                many(StampKind::ReviewConfirmed, 20, 100, NOW - 5 * DAY),
                vec![st(StampKind::GrantTier2, 200, NOW - DAY)],
            ]),
            want: Some(1),
        },
        Case {
            name: "19 review_confirmed is just below tier 2",
            facts: linked(),
            stamps: cat(vec![
                many(StampKind::Landed, 25, 1, NOW - 5 * DAY),
                many(StampKind::ReviewConfirmed, 19, 100, NOW - 5 * DAY),
                vec![st(StampKind::GrantTier2, 200, NOW - DAY)],
            ]),
            want: Some(1),
        },
        Case {
            name: "tier 2 without a linked account",
            facts: facts(),
            stamps: tier2_stamps(),
            want: Some(1),
        },
        Case {
            name: "open penalty drops tier 2 to 1",
            facts: linked(),
            stamps: cat(vec![
                tier2_stamps(),
                vec![st(StampKind::ReviewMissed, 900, NOW - 2 * DAY)],
            ]),
            want: Some(1),
        },
        Case {
            name: "revoked key gives no tier",
            facts: MemberFacts {
                key_state: KeyState::Revoked,
                ..linked()
            },
            stamps: tier2_stamps(),
            want: None,
        },
        Case {
            name: "intent stamp gives no tier",
            facts: linked(),
            stamps: cat(vec![tier2_stamps(), vec![st(StampKind::Intent, 901, NOW)]]),
            want: None,
        },
        Case {
            name: "maintainer is tier 3",
            facts: MemberFacts {
                is_maintainer: true,
                ..facts()
            },
            stamps: vec![],
            want: Some(3),
        },
        Case {
            name: "maintainer with a revoked key has no tier",
            facts: MemberFacts {
                is_maintainer: true,
                key_state: KeyState::Revoked,
                ..facts()
            },
            stamps: vec![],
            want: None,
        },
        Case {
            name: "rotated key counts like an active key",
            facts: MemberFacts {
                key_state: KeyState::Rotated,
                ..facts()
            },
            stamps: many(StampKind::Landed, 5, 1, NOW - DAY),
            want: Some(1),
        },
        Case {
            name: "stamps about other members are ignored",
            facts: facts(),
            stamps: many(StampKind::Landed, 5, 1, NOW - DAY)
                .into_iter()
                .map(|mut s| {
                    s.subject = "other".into();
                    s
                })
                .collect(),
            want: Some(0),
        },
        Case {
            name: "run_outlier and abandoned do not count for tier 1",
            facts: facts(),
            stamps: cat(vec![
                many(StampKind::RunOutlier, 30, 1, NOW - DAY),
                many(StampKind::Abandoned, 30, 100, NOW - DAY),
            ]),
            want: Some(0),
        },
    ];
    assert!(cases.len() >= 20);
    for c in cases {
        assert_eq!(tier_of(&c.facts, &c.stamps), c.want, "case: {}", c.name);
    }
}

#[test]
fn decision_explains_and_names_what_is_missing() {
    let d = tier(
        &facts(),
        &many(StampKind::Landed, 3, 1, NOW),
        &TierRules::default(),
        NOW,
    );
    assert_eq!(d.tier, Some(0));
    assert!(!d.reasons.is_empty());
    assert!(
        d.missing.iter().any(|m| m.contains("2 more landed")),
        "{:?}",
        d.missing
    );

    let d = tier(
        &linked(),
        &many(StampKind::Landed, 5, 1, NOW - DAY),
        &TierRules::default(),
        NOW,
    );
    assert_eq!(d.tier, Some(1));
    assert!(d.missing.iter().any(|m| m.contains("grant_tier2")));
    assert!(
        d.missing
            .iter()
            .any(|m| m.contains("20 more review_confirmed"))
    );

    let pen = vec![st(StampKind::Reverted, 900, NOW - DAY)];
    let d = tier(&facts(), &pen, &TierRules::default(), NOW);
    assert!(d.missing.iter().any(|m| m.contains("cleared")));
    assert!(d.reasons.iter().any(|r| r.contains("dropped one tier")));
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn order_of_stamps_never_changes_the_decision() {
    let sets = vec![
        cat(vec![
            tier2_stamps(),
            vec![st(StampKind::Reverted, 900, NOW - 2 * DAY), {
                let mut c = st(StampKind::Cleared, 901, NOW - DAY);
                c.evidence_msg = 900;
                c
            }],
            vec![st(StampKind::ReviewMissed, 902, NOW - DAY)],
        ]),
        cat(vec![
            many(StampKind::Landed, 5, 1, NOW - 40 * DAY),
            vec![st(StampKind::Reverted, 900, NOW - 20 * DAY)],
        ]),
    ];
    let rules = TierRules::default();
    for set in sets {
        let want = tier(&linked(), &set, &rules, NOW);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..100 {
            let mut s = set.clone();
            for i in (1..s.len()).rev() {
                let j = (rng.next() % (i as u64 + 1)) as usize;
                s.swap(i, j);
            }
            assert_eq!(tier(&linked(), &s, &rules, NOW), want);
        }
    }
}

fn body(kind: &str, subject: &str, issuer: &str, evidence: serde_json::Value) -> String {
    json!({"v": 1, "subject": subject, "kind": kind, "evidence": evidence, "issuer": issuer, "at": 1000})
        .to_string()
}

#[test]
fn validate_refuses_each_bad_case_with_a_reason() {
    let ev = json!({"msg": 7});
    let ok = validate_stamp(
        &body(
            "landed",
            "a",
            "b",
            json!({"msg": 7, "commit": "abc", "ref": "r"}),
        ),
        false,
    )
    .unwrap();
    assert_eq!(
        (
            ok.evidence_msg,
            ok.evidence_commit.as_str(),
            ok.evidence_ref.as_str()
        ),
        (7, "abc", "r")
    );
    assert_eq!(ok.kind, StampKind::Landed);

    let bad = |b: String, m: bool, needle: &str| {
        let e = validate_stamp(&b, m).unwrap_err();
        assert!(e.contains(needle), "{e} should contain {needle}");
        assert!(!e.contains('\n'));
    };
    bad(
        body("landed", "a", "b", json!({})),
        false,
        "evidence message id",
    );
    bad(
        body("landed", "a", "b", json!({"msg": 0})),
        false,
        "evidence message id",
    );
    bad(
        body("landed", "a", "b", json!({"commit": "x"})),
        false,
        "evidence message id",
    );
    bad(body("landed", "a", "b", json!("nope")), false, "evidence");
    bad(
        body("landed", "a", "a", ev.clone()),
        true,
        "nobody stamps their own work",
    );
    bad(
        body("hero", "a", "b", ev.clone()),
        true,
        "unknown stamp kind",
    );
    for k in ["grant_tier2", "cleared", "intent"] {
        bad(body(k, "a", "b", ev.clone()), false, "needs a maintainer");
        assert!(validate_stamp(&body(k, "a", "b", ev.clone()), true).is_ok());
    }
    bad("not json".into(), true, "not JSON");
    bad("[1]".into(), true, "JSON object");
    bad(
        json!({"subject":"a","kind":"landed","evidence":ev,"issuer":"b","at":1}).to_string(),
        true,
        "`v`",
    );
}

fn fact(id: u64, from: &str, role: Role, topic: &str, body: String) -> Message {
    Message {
        id,
        ts: "2026-11-25T12:00:00Z".into(),
        from: from.into(),
        role,
        kind: Kind::Fact,
        topic: topic.into(),
        body,
        refs: Vec::new(),
        to: Vec::new(),
    }
}

fn log() -> Vec<Message> {
    let ev = json!({"msg": 5, "commit": "abc123"});
    vec![
        fact(
            1,
            "hub",
            Role::Hub,
            "stamp/m1",
            body("landed", "m1", "hub", ev.clone()),
        ),
        fact(
            2,
            "hub",
            Role::Hub,
            "stamp/m1",
            body("run_agreed", "m1", "hub", ev.clone()),
        ),
        fact(
            3,
            "hub",
            Role::Hub,
            "stamp/m2",
            body("landed", "m2", "hub", ev.clone()),
        ),
        // Bad lines: no evidence, self issued, unknown kind, grant from a guest, wrong topic,
        // issuer that is not the sender, not JSON.
        fact(
            4,
            "hub",
            Role::Hub,
            "stamp/m1",
            body("landed", "m1", "hub", json!({})),
        ),
        fact(
            5,
            "m1",
            Role::Member,
            "stamp/m1",
            body("landed", "m1", "m1", ev.clone()),
        ),
        fact(
            6,
            "hub",
            Role::Hub,
            "stamp/m1",
            body("hero", "m1", "hub", ev.clone()),
        ),
        fact(
            7,
            "guest",
            Role::Member,
            "stamp/m1",
            body("grant_tier2", "m1", "guest", ev.clone()),
        ),
        fact(
            8,
            "hub",
            Role::Hub,
            "stamp/m2",
            body("landed", "m1", "hub", ev.clone()),
        ),
        fact(
            9,
            "guest",
            Role::Member,
            "stamp/m1",
            body("landed", "m1", "someone", ev.clone()),
        ),
        fact(10, "hub", Role::Hub, "stamp/m1", "garbage".into()),
        fact(
            11,
            "hub",
            Role::Hub,
            "other/m1",
            body("landed", "m1", "hub", ev.clone()),
        ),
        // A guest may issue an ordinary stamp in its own name.
        fact(
            12,
            "guest",
            Role::Member,
            "stamp/m1",
            body("review_confirmed", "m1", "guest", ev),
        ),
    ]
}

fn replay(msgs: &[Message]) -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    for m in msgs {
        store.apply(m).unwrap();
    }
    (dir, store)
}

#[test]
fn replay_twice_gives_equal_tables_and_bad_lines_leave_no_row() {
    let msgs = log();
    let (_d1, a) = replay(&msgs);
    let (_d2, b) = replay(&msgs);
    let ra = query_stamps(a.connection()).unwrap();
    let rb = query_stamps(b.connection()).unwrap();
    assert_eq!(ra, rb);
    let ids: Vec<u64> = ra.iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![1, 2, 3, 12]);
    assert_eq!(ra[0].evidence_commit, "abc123");
    assert!(ra.iter().all(|s| s.msg_id == s.id && s.evidence_msg == 5));

    // Applying the same log again on the same store changes nothing.
    let (_d3, mut c) = replay(&msgs);
    for m in &msgs {
        let _ = c.apply(m);
    }
    assert_eq!(query_stamps(c.connection()).unwrap(), ra);
}

#[test]
fn standing_counts_by_kind_and_api_has_no_tier() {
    let (_d, store) = replay(&log());
    let rows = standing(store.connection()).unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r.member.as_str()).collect();
    assert_eq!(names, vec!["m1", "m2"]);
    assert_eq!(rows[0].counts["landed"], 1);
    assert_eq!(rows[0].counts["run_agreed"], 1);
    assert_eq!(rows[0].counts["review_confirmed"], 1);
    assert_eq!(rows[0].counts["intent"], 0);
    assert_eq!(rows[1].counts["landed"], 1);

    let mut state = serde_json::Map::new();
    sommelier::stamp::StampModule
        .api(store.connection(), 0, &mut state)
        .unwrap();
    let arr = state["standing"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr[0].get("tier").is_none());
    assert_eq!(arr[0]["counts"]["landed"], 1);
}
