//! The factory sections of the policy file: defaults, the example file, validation and the hash rule.

use sommelier::policy::{DEFAULT_POLICY, FactoryRules, Mode, OtherFamily, Snapshot, TierRules};

const EXAMPLE: &str = include_str!("../policy/factory.example.toml");

/// Hash of the shipped policy computed before the factory sections existed.
const SHIPPED_HASH: &str = "527cdca11d51c07f4d6168f3c79db68ccc1b7e2c587d0e6fd942b56160440c5a";

fn validator() -> jsonschema::Validator {
    let schema = serde_json::from_str(include_str!("../policy/schema.json")).unwrap();
    jsonschema::validator_for(&schema).unwrap()
}

fn error_of(source: &str) -> String {
    Snapshot::parse(source).unwrap_err().to_string()
}

#[test]
fn shipped_policy_keeps_its_hash_and_has_default_factory() {
    let snapshot = Snapshot::parse(DEFAULT_POLICY).unwrap();
    assert_eq!(snapshot.hash, SHIPPED_HASH);
    let rules = snapshot.policy.factory();
    assert_eq!(rules, FactoryRules::default());
    assert!(!rules.join.enabled);
    assert_eq!(rules.switch.intake, Mode::Off);
    assert_eq!(rules.switch.tier0_coding, Mode::Off);
    assert_eq!(rules.switch.candidate_builds, Mode::Off);
    assert!(rules.review.is_empty());
    let policy = snapshot.response()["policy"].clone();
    for key in ["join", "review", "verify", "test", "switch"] {
        assert!(policy.get(key).is_none(), "{key} must not be serialised");
    }
    assert!(policy["trust"].get("tier0").is_none());
}

#[test]
fn example_loads_with_the_values_of_the_outline() {
    let snapshot = Snapshot::parse(EXAMPLE).unwrap();
    let rules = snapshot.policy.factory();
    assert_eq!(rules.tier0.leases, 1);
    assert_eq!(rules.tier0.submissions_per_day, 4);
    assert_eq!(rules.tier0.max_bundle_kib, 256);
    assert_eq!(rules.tier0.verify_share, 0.20);
    assert_eq!(rules.tier0.review_share, 0.20);
    assert_eq!(rules.tier0.kinds, ["run", "fix", "port"]);
    assert!(!rules.tier0.free_text);
    assert_eq!(rules.tier1.leases, 3);
    assert_eq!(rules.tier1.reach.landed, 5);
    assert_eq!(rules.tier1.reach.run_agreed, 20);
    assert_eq!(rules.tier1.reach.key_age_days, 7);
    assert_eq!(rules.tier1.reach.reverted_for_cause, 0);
    assert_eq!(rules.tier2.reach.landed, 25);
    assert_eq!(rules.tier2.reach.review_confirmed, 20);
    assert_eq!(rules.join.proof_of_work_bits, 22);
    assert_eq!(rules.join.per_block_per_hour, 5);
    let default = &rules.review["default"];
    assert_eq!(
        (
            default.tier0_author.reviewers,
            default.tier0_author.min_tier
        ),
        (1, 2)
    );
    assert_eq!(
        (
            default.tier1_author.reviewers,
            default.tier1_author.min_tier
        ),
        (1, 1)
    );
    assert_eq!(default.audit_share, 0.10);
    assert_eq!(default.other_family, OtherFamily::Prefer);
    assert_eq!(rules.verify.workflow, "factory-verify.yml");
    assert_eq!(rules.verify.max_parallel, 16);
    assert_eq!(rules.test.quorum, 3);
    assert_eq!(rules.test.candidate_min_tier, 2);
    assert_eq!(rules.switch.intake, Mode::Off);
    // A file with the sections written out agrees with the defaults for tier 0.
    assert_eq!(rules.tier0, TierRules::for_tier(0));
    // The guest table of the shipped file survives next to the tier tables.
    assert!(snapshot.response()["policy"]["trust"]["guest"].is_object());
}

#[test]
fn example_round_trips_and_matches_the_schema() {
    let snapshot = Snapshot::parse(EXAMPLE).unwrap();
    let validator = validator();
    assert!(validator.is_valid(&snapshot.response()["policy"]));
    let untyped: toml::Table = toml::from_str(EXAMPLE).unwrap();
    assert!(validator.is_valid(&serde_json::to_value(untyped).unwrap()));
    let again = Snapshot::verify(snapshot.response()).unwrap();
    assert_eq!(again.hash, snapshot.hash);
    assert_ne!(snapshot.hash, SHIPPED_HASH);
}

#[test]
fn partial_tier_tables_keep_the_defaults() {
    let source = format!("{DEFAULT_POLICY}\n[trust.tier1]\nleases = 9\n");
    let rules = Snapshot::parse(&source).unwrap().policy.factory();
    assert_eq!(rules.tier1.leases, 9);
    assert_eq!(rules.tier1.reach, TierRules::for_tier(1).reach);
    assert_eq!(rules.tier0, TierRules::for_tier(0));
}

#[test]
fn bad_values_are_refused_with_the_field_name() {
    let cases: [(&str, &str, &str); 9] = [
        (
            "[trust.tier0]\nverify_share = 1.5\n",
            "trust.tier0.verify_share",
            "between 0 and 1",
        ),
        (
            "[trust.tier2]\nreview_share = -0.1\n",
            "trust.tier2.review_share",
            "between 0 and 1",
        ),
        (
            "[join]\nproof_of_work_bits = 33\n",
            "join.proof_of_work_bits",
            "between 0 and 32",
        ),
        (
            "[review.default]\naudit_share = 2.0\n",
            "review.default.audit_share",
            "between 0 and 1",
        ),
        (
            "[review.default]\ntier0_author = { reviewers = 0, min_tier = 2 }\n",
            "review.default.tier0_author.reviewers",
            "at least 1",
        ),
        (
            "[review.default]\ntier1_author = { reviewers = 1, min_tier = 4 }\n",
            "review.default.tier1_author.min_tier",
            "between 0 and 3",
        ),
        (
            "[review.strict]\naudit_share = 0.5\n",
            "review",
            "named default",
        ),
        ("[test]\nquorum = 0\n", "test.quorum", "at least 1"),
        (
            "[test]\ncandidate_min_tier = 9\n",
            "test.candidate_min_tier",
            "between 0 and 3",
        ),
    ];
    for (section, field, hint) in cases {
        let message = error_of(&format!("{DEFAULT_POLICY}\n{section}"));
        assert!(message.contains(field), "{message}");
        assert!(message.contains(hint), "{message}");
    }
    // The edges are allowed.
    let edge = "[join]\nproof_of_work_bits = 32\n[test]\nquorum = 1\ncandidate_min_tier = 3\n";
    assert!(Snapshot::parse(&format!("{DEFAULT_POLICY}\n{edge}")).is_ok());
}

#[test]
fn unknown_keys_in_factory_tables_are_refused() {
    let sections = [
        "[join]\nextra = 1\n",
        "[verify]\nextra = 1\n",
        "[test]\nextra = 1\n",
        "[switch]\nextra = \"off\"\n",
        "[review.default]\nextra = 1\n",
        "[review.default]\ntier0_author = { reviewers = 1, min_tier = 2, extra = 1 }\n",
        "[trust.tier0]\nextra = 1\n",
        "[trust.tier1]\nreach = { extra = 1 }\n",
        "[trust.tier1]\nmay_claim = [\"fix\"]\n",
        "[switch]\nintake = \"maybe\"\n",
    ];
    for section in sections {
        let message = error_of(&format!("{DEFAULT_POLICY}\n{section}"));
        assert!(!message.is_empty(), "{section}");
    }
    // A role table still refuses unknown keys and keeps its required keys.
    assert!(
        error_of(&format!("{DEFAULT_POLICY}\n[trust.guest2]\nextra = 1\n"))
            .contains("trust.guest2")
    );
}

#[test]
fn rollback_style_toml_round_trip_keeps_the_factory_sections() {
    let policy = Snapshot::parse(EXAMPLE).unwrap().policy;
    let text = toml::to_string(&policy).unwrap();
    let back = Snapshot::parse(&text).unwrap();
    assert_eq!(back.policy.factory(), policy.factory());
    assert_eq!(back.hash, Snapshot::parse(EXAMPLE).unwrap().hash);
}
