//! What a community's own published rules say about self-promotion.
//!
//! `discovery_place_rules` could hold a measured self-promo stance for as
//! long as the table has existed, and the screening policy has refused
//! `self_promo_ratio_percent = 0` communities for just as long — but nothing
//! ever produced a measurement, so the gate never fired and promotion-hostile
//! subreddits kept being admitted blind. Two of them removed the account's
//! posts inside a week and halted Reddit standing for thirty days.
//!
//! This is the classifier the rules-refresh sweep runs over the rule texts
//! Reddit publishes at `about/rules`. It is deliberately conservative:
//!
//! - It only ever refuses on an explicit signal — "no self promotion",
//!   thread-confined promotion, promotion gated on participation the account
//!   does not have. Ambiguity is `Unknown`, not a ban.
//! - A community that allows promotion but only with something automation
//!   cannot do (a required post flair, a mod check-in) is `ApprovalRequired`:
//!   admissible, but the send lane must not touch it — a person does.
//! - An explicit allowance still reports a modest ratio: every community
//!   that permits self-promotion couches it in "keep it to a minimum"
//!   language, and treating "allowed" as "unlimited" is how accounts get
//!   reported for spam.

/// The classified self-promotion stance of one community's rule texts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelfPromoStance {
    /// The rules permit self-promotion outright (with the usual "don't spam"
    /// proviso). `ratio_percent` is the conservative share of our posting
    /// that may be promotional — never more than 10 even when the rule
    /// allows more, because every such rule pairs the allowance with an
    /// anti-spam expectation.
    Allowed { ratio_percent: i16 },
    /// Self-promotion is banned, confined to a designated thread/day, or
    /// gated on participation a fresh account cannot have. For the posting
    /// lane all three are the same fact: a post is removal bait.
    Banned,
    /// Self-promotion is permitted only with something the executor cannot
    /// supply — a required post flair, or a rule that asks promotion to be
    /// cleared with the mods first. A person can satisfy it by hand; the
    /// send lane cannot.
    ApprovalRequired { ratio_percent: i16 },
    /// The rules say nothing about self-promotion either way.
    Unknown,
}

impl SelfPromoStance {
    /// The `discovery_place_rules` write for the stance: the measured ratio,
    /// and whether a person must clear the post.
    #[must_use]
    pub const fn columns(self) -> (Option<i16>, bool) {
        match self {
            Self::Allowed { ratio_percent } => (Some(ratio_percent), false),
            Self::Banned => (Some(0), false),
            Self::ApprovalRequired { ratio_percent } => (Some(ratio_percent), true),
            Self::Unknown => (None, false),
        }
    }
}

/// Phrases that mean "posting our own video here is against the rules",
/// checked case-insensitively against each rule's title and body.
///
/// Three shapes of the same verdict:
///
/// - outright: "no self promotion", "don't post your own music"
/// - confined: promotion belongs in a designated thread/day — the weekly
///   promote thread, Saturdays only. A delivery that posts a thread of its
///   own violates it just as an outright ban does.
/// - gated: "only if you've participated", "must be an active member",
///   "established users", the 10%-of-activity guideline. Reddit's own
///   self-promotion guideline asks that promo stay under 10% of an
///   account's activity; a promotion-only account fails it by definition,
///   and the communities that cite it enforce it.
const BAN_SIGNALS: &[&str] = &[
    "no self promotion",
    "no self-promotion",
    "don't post your own",
    "do not post your own",
    "self-promotion is not allowed",
    "self promotion is not allowed",
    "self-promotional posts are not allowed",
    "promotion is not allowed",
    "no advertising your own",
    "except in dedicated",
    "except in the dedicated",
    "except in our",
    "promote thread",
    "self-promo thread",
    "self promo thread",
    "weekly thread",
    "saturdays only",
    "saturday only",
    "must be an active member",
    "active member of the community",
    "if you've participated",
    "you've participated",
    "established community participant",
    "established users",
    "semi-regular contributor",
    "excessive self-promotion",
    "excessive self promotion",
    "obnoxious self-promotion",
    "10% of your",
    "one out of every ten",
    "self-promotion are encouraged to be active",
    "self-promo that is not real engagement",
];

/// Phrases that allow promotion but gate it on something the send lane
/// cannot produce — a required flair tag, or a rule that routes promotion
/// through the mods. A human operator can satisfy either by hand.
const GATE_SIGNALS: &[&str] = &[
    "post flair is required",
    "flair is required",
    "must be marked",
    "marked with the post flair",
    "use 'self promotion' flair",
    "self-promotion flair",
    "self promo flair",
    "check with the mods",
    "message the mods",
    "modmail first",
    "prior mod approval",
];

/// Phrases that say self-promotion is allowed. Checked only when no ban or
/// gate signal fired — "allowed, but must be marked" is the gate, and
/// "allowed except in the thread" is the ban.
const ALLOW_SIGNALS: &[&str] = &[
    "posting your own band is allowed",
    "posting your own band is permitted",
    "your own band is allowed",
    "self-promotion is allowed",
    "self promotion is allowed",
    "self-promotion is not forbidden",
    "self-promotion is permitted",
    "feel free to share your own band",
    "feel free to post your own",
    "we don't mind you sharing",
    "sharing your own band",
    "share your own band",
    "original music that you've made",
];

/// Classifies a community's rule texts into a self-promotion stance.
///
/// `rule_texts` is the flattened rule set — each entry one rule's title or
/// body — in the community's own words. The check is per-rule first: a ban
/// phrase inside any single rule decides the community, because moderators
/// enforce the strictest rule, not the average one. Gate phrases are
/// aggregated across the rules (the flair rule and the self-promo rule are
/// usually separate rules).
#[must_use]
pub fn classify_self_promo(rule_texts: &[&str]) -> SelfPromoStance {
    let texts: Vec<String> = rule_texts.iter().map(|t| t.to_lowercase()).collect();
    if texts
        .iter()
        .any(|t| BAN_SIGNALS.iter().any(|signal| t.contains(signal)))
    {
        return SelfPromoStance::Banned;
    }
    if texts
        .iter()
        .any(|t| GATE_SIGNALS.iter().any(|signal| t.contains(signal)))
    {
        return SelfPromoStance::ApprovalRequired { ratio_percent: 10 };
    }
    if texts
        .iter()
        .any(|t| ALLOW_SIGNALS.iter().any(|signal| t.contains(signal)))
    {
        return SelfPromoStance::Allowed { ratio_percent: 10 };
    }
    SelfPromoStance::Unknown
}

/// Builds the `rules_summary` a person reads at a glance — the rule titles,
/// joined, truncated to the column's 4000-char bound.
#[must_use]
pub fn summarize(rule_titles: &[&str]) -> Option<String> {
    let joined = rule_titles
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" || ");
    if joined.is_empty() {
        None
    } else {
        Some(joined.chars().take(3900).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outright_ban_is_banned() {
        let stance = classify_self_promo(&[
            "No Self Promotion (Except in dedicated Saturday thread)",
            "No Full Releases",
        ]);
        assert_eq!(stance, SelfPromoStance::Banned);
    }

    #[test]
    fn participation_gate_is_banned_for_a_fresh_account() {
        let stance = classify_self_promo(&[
            "About self-promotion :: You can self promote, but ONLY if you've \
             participated in our community before",
        ]);
        assert_eq!(stance, SelfPromoStance::Banned);
    }

    #[test]
    fn excessive_promo_guideline_is_banned() {
        let stance = classify_self_promo(&[
            "No obvious and/or excessive self-promotion :: Users interested in \
             self-promotion are encouraged to be active in the community",
        ]);
        assert_eq!(stance, SelfPromoStance::Banned);
    }

    #[test]
    fn flair_gate_is_approval_required_not_banned() {
        let stance = classify_self_promo(&[
            "Post flair is required",
            "Self-promotion is allowed, but must be marked a such",
        ]);
        assert_eq!(
            stance,
            SelfPromoStance::ApprovalRequired { ratio_percent: 10 }
        );
    }

    #[test]
    fn explicit_allowance_reports_a_modest_ratio() {
        let stance = classify_self_promo(&[
            "Self-promotional spam :: Posting your own band is allowed, but \
             please read our self-promotion post first",
        ]);
        assert_eq!(stance, SelfPromoStance::Allowed { ratio_percent: 10 });
    }

    #[test]
    fn silent_rules_are_unknown_never_a_ban() {
        let stance = classify_self_promo(&["Be kind", "No piracy"]);
        assert_eq!(stance, SelfPromoStance::Unknown);
    }

    #[test]
    fn ban_wins_over_allowance_in_the_same_ruleset() {
        let stance = classify_self_promo(&[
            "Posting your own band is allowed",
            "No self promotion except in the weekly thread",
        ]);
        assert_eq!(stance, SelfPromoStance::Banned);
    }

    #[test]
    fn summary_joins_titles_and_respects_empty() {
        assert_eq!(
            summarize(&["Title format", "", "  ", "No memes"]).as_deref(),
            Some("Title format || No memes")
        );
        assert_eq!(summarize(&["", " "]), None);
    }
}
