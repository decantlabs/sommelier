//! Who reviews a submission. Pure: the caller passes the secret and the candidates.

use std::fmt;

use super::hmac::hmac_sha256_parts;

/// What the picker knows about the submission.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubmissionView {
    /// SHA 256 of the bundle, as hex text.
    pub bundle_sha256: String,
    pub author: String,
    pub author_tier: u8,
    /// Opaque address block of the author, `None` when unknown.
    pub author_block: Option<String>,
    pub author_account: Option<String>,
    pub author_family: Option<String>,
}

/// A member who could review.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Candidate {
    pub member: String,
    pub tier: u8,
    pub block: Option<String>,
    pub account: Option<String>,
    pub family: Option<String>,
    pub online: bool,
    pub open_reviews: u32,
}

/// Whether a reviewer of another model family comes first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OtherFamily {
    Prefer,
    Ignore,
}

/// Settings of the picker. They arrive as a struct, the policy file is read elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewRule {
    /// Number of reviewers per author tier 0 to 3.
    pub reviewers: [usize; 4],
    /// Minimum reviewer tier per author tier 0 to 3. The floor is also the author tier and 1.
    pub min_tier: [u8; 4],
    pub other_family: OtherFamily,
    /// A reviewer with this many open reviews or more is not picked.
    pub max_open: u32,
}

impl Default for ReviewRule {
    fn default() -> Self {
        ReviewRule {
            reviewers: [1, 1, 1, 1],
            min_tier: [2, 1, 1, 1],
            other_family: OtherFamily::Prefer,
            max_open: 3,
        }
    }
}

/// Why a candidate was left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exclusion {
    Author,
    SameBlock,
    SameAccount,
    Offline,
    TierBelowAuthor,
    TierBelowOne,
    /// Below the minimum of the rule, for a tier 0 author this means below tier 2.
    TierBelowMinimum,
    TooManyOpen,
}

impl fmt::Display for Exclusion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Exclusion::Author => "is the author",
            Exclusion::SameBlock => "same address block as the author",
            Exclusion::SameAccount => "same linked account as the author",
            Exclusion::Offline => "offline",
            Exclusion::TierBelowAuthor => "tier below the author's",
            Exclusion::TierBelowOne => "tier below 1",
            Exclusion::TierBelowMinimum => "tier below the minimum for this author",
            Exclusion::TooManyOpen => "too many open reviews",
        })
    }
}

/// The result of a pick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pick {
    /// Chosen member ids, best first.
    pub picked: Vec<String>,
    /// Every excluded candidate with the first reason that applied, in candidate order.
    pub excluded: Vec<(String, Exclusion)>,
    /// How many reviewers are missing. The caller escalates when this is above 0.
    pub short_by: usize,
}

fn same(a: &Option<String>, b: &Option<String>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if x == y)
}

fn exclusion(sub: &SubmissionView, c: &Candidate, rule: &ReviewRule) -> Option<Exclusion> {
    let idx = usize::from(sub.author_tier.min(3));
    if c.member == sub.author {
        Some(Exclusion::Author)
    } else if same(&sub.author_block, &c.block) {
        Some(Exclusion::SameBlock)
    } else if same(&sub.author_account, &c.account) {
        Some(Exclusion::SameAccount)
    } else if !c.online {
        Some(Exclusion::Offline)
    } else if c.tier < sub.author_tier {
        Some(Exclusion::TierBelowAuthor)
    } else if c.tier < 1 {
        Some(Exclusion::TierBelowOne)
    } else if c.tier < rule.min_tier[idx] {
        Some(Exclusion::TierBelowMinimum)
    } else if c.open_reviews >= rule.max_open {
        Some(Exclusion::TooManyOpen)
    } else {
        None
    }
}

/// Picks the reviewers. The order among eligible candidates is the HMAC SHA 256 of the bundle hash
/// and the candidate id under `secret`, so the author can neither predict nor steer it.
pub fn pick_reviewers(
    sub: &SubmissionView,
    candidates: &[Candidate],
    rule: &ReviewRule,
    secret: &[u8],
) -> Pick {
    let mut excluded = Vec::new();
    let mut ranked: Vec<([u8; 32], bool, &Candidate)> = Vec::new();
    for c in candidates {
        if let Some(why) = exclusion(sub, c, rule) {
            excluded.push((c.member.clone(), why));
            continue;
        }
        let tag = hmac_sha256_parts(
            secret,
            &[
                b"pick\0",
                sub.bundle_sha256.as_bytes(),
                b"\0",
                c.member.as_bytes(),
            ],
        );
        let other = rule.other_family == OtherFamily::Prefer
            && matches!((&sub.author_family, &c.family), (Some(a), Some(b)) if a != b);
        ranked.push((tag, !other, c));
    }
    ranked.sort_by(|a, b| (a.1, a.0, &a.2.member).cmp(&(b.1, b.0, &b.2.member)));
    let want = rule.reviewers[usize::from(sub.author_tier.min(3))];
    let picked: Vec<String> = ranked
        .iter()
        .take(want)
        .map(|r| r.2.member.clone())
        .collect();
    let short_by = want - picked.len();
    Pick {
        picked,
        excluded,
        short_by,
    }
}

/// Whether a passed review gets a second look by a maintainer seat. `audit_share` is 0.0 to 1.0.
pub fn is_audited(review_id: &str, secret: &[u8], audit_share: f64) -> bool {
    if audit_share.is_nan() || audit_share <= 0.0 {
        return false;
    }
    if audit_share >= 1.0 {
        return true;
    }
    let tag = hmac_sha256_parts(secret, &[b"audit\0", review_id.as_bytes()]);
    let mut eight = [0u8; 8];
    eight.copy_from_slice(&tag[..8]);
    (u64::from_be_bytes(eight) as f64 / 18_446_744_073_709_551_616.0) < audit_share
}
