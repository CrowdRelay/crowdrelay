//! Where a fan actually came from, and when to admit we do not know.
//!
//! A zero-budget campaign across ten communities lives or dies on one question:
//! which of them produced people who stayed. Answering it needs the channel on
//! the record, and answering it *honestly* needs the discipline to say nothing
//! when the chain is broken.
//!
//! The one rule worth stating outright, because every analytics tool in
//! existence gets it wrong: **a signup with no click is not "direct traffic".**
//! It is unattributed. Bucketing unknowns as direct is a lie that makes the
//! channel you cannot see look like a channel that works, and it is exactly the
//! kind of plausible number this system refuses to produce.

use serde::{Deserialize, Serialize};

/// The identity a smart link carries, and therefore the identity every person
/// who arrived through it inherits.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChannelIdentity {
    /// Broad channel: reddit, facebook, discord, linkedin, venue, band.
    pub source: String,
    /// The specific place inside it: which subreddit, group or server. The
    /// field that answers "which community converts".
    pub community: Option<String>,
    /// Which post, image or wording, so two links can test one against the
    /// other without a testing framework.
    pub creative: Option<String>,
}

impl ChannelIdentity {
    /// A stable key for grouping, with the parts that are absent named as
    /// absent rather than blank.
    ///
    /// `reddit/r-metal/-` reads differently from `reddit/-/-`, and collapsing
    /// both to "reddit" would merge a targeted post with an untargeted one.
    #[must_use]
    pub fn grouping_key(&self) -> String {
        format!(
            "{}/{}/{}",
            self.source,
            self.community.as_deref().unwrap_or("-"),
            self.creative.as_deref().unwrap_or("-")
        )
    }
}

/// Why a fan could not be attributed to a channel.
///
/// Kept apart rather than merged into one "unknown", because they call for
/// different fixes: no visitor means the landing page is dropping the cookie,
/// no click means somebody arrived by a route with no link, and an unlabelled
/// link means whoever created it skipped the channel fields.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnattributedReason {
    /// The signup carries no visitor, so no click can be matched to it.
    NoVisitor,
    /// The visitor is known but never clicked a tracked link before signing up.
    NoClickBeforeSignup,
    /// The click is known but its link carries no channel identity.
    LinkNotLabelled,
}

impl UnattributedReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoVisitor => "no_visitor",
            Self::NoClickBeforeSignup => "no_click_before_signup",
            Self::LinkNotLabelled => "link_not_labelled",
        }
    }

    /// What to do about it. Each of these is a different fix, which is the
    /// whole reason they are not one bucket.
    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::NoVisitor => "the landing page is not carrying the visitor through to signup",
            Self::NoClickBeforeSignup => {
                "somebody arrived by a route with no tracked link; add a link for it"
            }
            Self::LinkNotLabelled => {
                "the link exists but was created without a channel, community or creative"
            }
        }
    }
}

/// Where one fan came from, or an honest refusal to say.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "evidence")]
pub enum ChannelAttribution {
    Attributed(ChannelIdentity),
    /// Not "direct". Unknown.
    Unattributed {
        reason: UnattributedReason,
    },
}

/// What the adapter could find when it walked back from a signup.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct AttributionEvidence {
    pub had_visitor: bool,
    pub had_click_before_signup: bool,
    /// The channel identity on the clicked link, when it had one.
    pub identity: Option<ChannelIdentity>,
}

/// Resolves one fan's channel from what the join actually produced.
///
/// Every branch that cannot produce a channel names why. There is deliberately
/// no fallback bucket: a system that always produces an answer produces a wrong
/// one the moment the chain breaks, and nobody notices because the number still
/// looks reasonable.
#[must_use]
pub fn attribute_channel(evidence: &AttributionEvidence) -> ChannelAttribution {
    if !evidence.had_visitor {
        return ChannelAttribution::Unattributed {
            reason: UnattributedReason::NoVisitor,
        };
    }
    if !evidence.had_click_before_signup {
        return ChannelAttribution::Unattributed {
            reason: UnattributedReason::NoClickBeforeSignup,
        };
    }
    match &evidence.identity {
        Some(identity) if !identity.source.trim().is_empty() => {
            ChannelAttribution::Attributed(identity.clone())
        }
        _ => ChannelAttribution::Unattributed {
            reason: UnattributedReason::LinkNotLabelled,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ChannelIdentity {
        ChannelIdentity {
            source: "reddit".to_owned(),
            community: Some("r-metal".to_owned()),
            creative: Some("discovery-playlist".to_owned()),
        }
    }

    fn clicked(identity: Option<ChannelIdentity>) -> AttributionEvidence {
        AttributionEvidence {
            had_visitor: true,
            had_click_before_signup: true,
            identity,
        }
    }

    #[test]
    fn a_labelled_click_attributes_the_fan() {
        assert_eq!(
            attribute_channel(&clicked(Some(identity()))),
            ChannelAttribution::Attributed(identity())
        );
    }

    #[test]
    fn a_signup_with_no_click_is_unknown_and_never_direct() {
        // Every analytics tool gets this wrong. Bucketing unknowns as direct
        // makes the channel you cannot see look like one that works.
        let attribution = attribute_channel(&AttributionEvidence {
            had_visitor: true,
            had_click_before_signup: false,
            identity: None,
        });
        assert_eq!(
            attribution,
            ChannelAttribution::Unattributed {
                reason: UnattributedReason::NoClickBeforeSignup
            }
        );
        // Nothing in the type system can produce a "direct" bucket.
        assert!(!format!("{attribution:?}").to_lowercase().contains("direct"));
    }

    #[test]
    fn each_broken_link_in_the_chain_is_reported_separately() {
        // They are three different fixes, which is why they are not one bucket.
        assert_eq!(
            attribute_channel(&AttributionEvidence::default()),
            ChannelAttribution::Unattributed {
                reason: UnattributedReason::NoVisitor
            }
        );
        assert_eq!(
            attribute_channel(&clicked(None)),
            ChannelAttribution::Unattributed {
                reason: UnattributedReason::LinkNotLabelled
            }
        );
    }

    #[test]
    fn an_unlabelled_link_is_not_attributed_to_an_empty_channel() {
        // A blank source would group every unlabelled link together under one
        // convincing-looking row.
        let blank = ChannelIdentity {
            source: "   ".to_owned(),
            ..identity()
        };
        assert_eq!(
            attribute_channel(&clicked(Some(blank))),
            ChannelAttribution::Unattributed {
                reason: UnattributedReason::LinkNotLabelled
            }
        );
    }

    #[test]
    fn grouping_names_absent_parts_rather_than_blanking_them() {
        assert_eq!(
            identity().grouping_key(),
            "reddit/r-metal/discovery-playlist"
        );
        let broad = ChannelIdentity {
            source: "reddit".to_owned(),
            community: None,
            creative: None,
        };
        assert_eq!(broad.grouping_key(), "reddit/-/-");
        // A targeted post and an untargeted one must not merge into one row.
        assert_ne!(identity().grouping_key(), broad.grouping_key());
    }

    #[test]
    fn every_reason_names_itself_and_its_fix() {
        for reason in [
            UnattributedReason::NoVisitor,
            UnattributedReason::NoClickBeforeSignup,
            UnattributedReason::LinkNotLabelled,
        ] {
            assert!(!reason.as_str().is_empty());
            assert!(!reason.remedy().is_empty());
        }
    }
}
