//! Answering the people who comment on the band's own posts.
//!
//! A band that posts and never answers reads as a bot, and the comment under
//! its own post is the warmest contact it will ever get: somebody who saw the
//! work and said something. This module decides, deterministically, which
//! comments are the band's to answer, where a drafted answer goes, and how
//! fast answers may leave.
//!
//! - **Which comments.** Someone else's comment, still up, not a bot, either
//!   directly under the band's post or a reply to the band's own comment — a
//!   conversation with the band. Fans talking to each other are left alone,
//!   and anything the band already answered is not answered twice.
//! - **Where a draft goes.** Every mechanical hold, a failed or unreadable
//!   independent review, or a reviewer nobody could reach puts the draft in
//!   front of a person with the reason. Only a clean draft with a passing
//!   review may skip that queue, and only when the operator switched
//!   unattended replies on.
//! - **How fast.** Never instantly (an answer seconds after a comment is the
//!   clearest bot tell), never a burst, and a daily ceiling.

use time::{Duration, OffsetDateTime};

/// One comment under one of the band's posts, as the harvest sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarvestedComment {
    /// `t1_…`
    pub id: String,
    /// `t3_…` (under the post) or `t1_…` (under a comment).
    pub parent_id: String,
    pub author: String,
    pub body: String,
    /// Written by the band's own account.
    pub by_band: bool,
    /// Removed or deleted.
    pub gone: bool,
}

/// Accounts that are not people. Lowercased.
const KNOWN_BOTS: &[&str] = &["automoderator", "[deleted]", "savevideo", "remindmebot"];

/// Which harvested comments are the band's to answer, in thread order.
///
/// `post_fullname` is the band's post (`t3_…`). A comment qualifies when it is
/// someone else's, still up, not a bot, has words in it, sits directly under
/// the post or under one of the band's own comments, and no comment of the
/// band's already answers it.
#[must_use]
pub fn comments_to_answer<'a>(
    post_fullname: &str,
    comments: &'a [HarvestedComment],
) -> Vec<&'a HarvestedComment> {
    let band_comment_ids: Vec<&str> = comments
        .iter()
        .filter(|c| c.by_band)
        .map(|c| c.id.as_str())
        .collect();
    let answered: Vec<&str> = comments
        .iter()
        .filter(|c| c.by_band)
        .map(|c| c.parent_id.as_str())
        .collect();
    comments
        .iter()
        .filter(|c| {
            let author = c.author.to_lowercase();
            !c.by_band
                && !c.gone
                && !KNOWN_BOTS.contains(&author.as_str())
                && !author.ends_with("bot")
                && c.body.chars().any(char::is_alphanumeric)
                && (c.parent_id == post_fullname
                    || band_comment_ids.contains(&c.parent_id.as_str()))
                && !answered.contains(&c.id.as_str())
        })
        .collect()
}

/// What the independent reviewer said, as far as the router needs it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewOutcome {
    Passed {
        score: u8,
    },
    Failed {
        score: u8,
    },
    /// No free lane answered. Not a pass.
    Unavailable,
}

/// Where a drafted reply goes next.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplyRoute {
    /// Clean, reviewed, and unattended replies are on: approved to send.
    Approve,
    /// A person reads it first. `reason` is `None` for an ordinary approval
    /// (unattended replies off); `Some` when something held it.
    AwaitApproval { reason: Option<String> },
}

/// Routes a drafted reply. `guard_hold` is the first mechanical hold reason
/// (publish guard, register guard), if any.
#[must_use]
pub fn route_reply(
    guard_hold: Option<&str>,
    review: ReviewOutcome,
    unattended_replies: bool,
) -> ReplyRoute {
    if let Some(reason) = guard_hold {
        return ReplyRoute::AwaitApproval {
            reason: Some(reason.to_owned()),
        };
    }
    match review {
        ReviewOutcome::Failed { score } => ReplyRoute::AwaitApproval {
            reason: Some(format!(
                "held: the independent review scored it {score}/10 — rewrite before it goes out"
            )),
        },
        ReviewOutcome::Unavailable => ReplyRoute::AwaitApproval {
            reason: Some(
                "held: no reviewer could be reached — read it before it goes out".to_owned(),
            ),
        },
        ReviewOutcome::Passed { .. } if unattended_replies => ReplyRoute::Approve,
        ReviewOutcome::Passed { .. } => ReplyRoute::AwaitApproval { reason: None },
    }
}

/// Replies the account may send in 24 hours.
pub const MAX_REPLIES_PER_24H: i64 = 12;
/// Least time between two replies from the account.
pub const MIN_REPLY_GAP: Duration = Duration::minutes(10);
/// An approved reply waits at least this long after approval…
pub const REPLY_DELAY_MIN: Duration = Duration::minutes(12);
/// …and at most this long: a person gets to their notifications eventually.
pub const REPLY_DELAY_MAX: Duration = Duration::minutes(75);

/// When an approved reply may leave: a delay drawn from the window, where
/// `unit` is a uniform draw in `[0, 1)` supplied by the caller (so this stays
/// deterministic and testable).
#[must_use]
pub fn reply_not_before(approved_at: OffsetDateTime, unit: f64) -> OffsetDateTime {
    let unit = if unit.is_finite() {
        unit.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let span = (REPLY_DELAY_MAX - REPLY_DELAY_MIN).whole_seconds();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "span is a few thousand seconds; the product fits i64 exactly"
    )]
    let offset = (span as f64 * unit) as i64;
    approved_at + REPLY_DELAY_MIN + Duration::seconds(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: &str, parent: &str, author: &str, by_band: bool) -> HarvestedComment {
        HarvestedComment {
            id: id.to_owned(),
            parent_id: parent.to_owned(),
            author: author.to_owned(),
            body: "what tuning is this?".to_owned(),
            by_band,
            gone: false,
        }
    }

    #[test]
    fn the_band_answers_its_own_conversations_once() {
        let comments = vec![
            comment("t1_a", "t3_p", "fan1", false),
            comment("t1_b", "t1_a", "virya", true), // answered t1_a
            comment("t1_c", "t1_b", "fan1", false), // reply to the band: ours
            comment("t1_d", "t3_p", "fan2", false), // unanswered top-level: ours
            comment("t1_e", "t1_d", "fan3", false), // fans talking: not ours
            comment("t1_f", "t3_p", "AutoModerator", false),
            comment("t1_g", "t3_p", "SaveVideoBot", false),
            HarvestedComment {
                gone: true,
                ..comment("t1_h", "t3_p", "fan4", false)
            },
            HarvestedComment {
                body: "🔥🔥".to_owned(),
                ..comment("t1_i", "t3_p", "fan5", false)
            },
        ];
        let ids: Vec<&str> = comments_to_answer("t3_p", &comments)
            .into_iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, vec!["t1_c", "t1_d"]);
    }

    #[test]
    fn only_a_clean_reviewed_draft_skips_the_queue_and_only_when_switched_on() {
        assert_eq!(
            route_reply(None, ReviewOutcome::Passed { score: 8 }, true),
            ReplyRoute::Approve
        );
        assert_eq!(
            route_reply(None, ReviewOutcome::Passed { score: 8 }, false),
            ReplyRoute::AwaitApproval { reason: None }
        );
        assert!(matches!(
            route_reply(
                Some("held: hashtags"),
                ReviewOutcome::Passed { score: 9 },
                true
            ),
            ReplyRoute::AwaitApproval { reason: Some(_) }
        ));
        assert!(matches!(
            route_reply(None, ReviewOutcome::Failed { score: 4 }, true),
            ReplyRoute::AwaitApproval { reason: Some(_) }
        ));
        assert!(matches!(
            route_reply(None, ReviewOutcome::Unavailable, true),
            ReplyRoute::AwaitApproval { reason: Some(_) }
        ));
    }

    #[test]
    fn a_reply_never_leaves_instantly_and_never_waits_forever() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
        assert_eq!(reply_not_before(now, 0.0), now + REPLY_DELAY_MIN);
        assert!(reply_not_before(now, 0.999) < now + REPLY_DELAY_MAX);
        assert_eq!(reply_not_before(now, f64::NAN), reply_not_before(now, 0.5));
    }
}
