//! Whether the Reddit account may post on its own right now, and how often.
//!
//! The account the community executor publishes through is the only Reddit
//! access the system has: observation, subreddit metrics and post engagement
//! all read through the same logged-in session. A post a moderator reads as
//! spam does not cost a post, it costs every read the growth loop depends on.
//!
//! So autonomy is earned the way sending volume is earned in
//! `deliverability`, and lost faster than it is earned:
//!
//! 1. **A removal is the signal.** Reddit tells the author when a moderator,
//!    AutoModerator or its own site-wide filters removed a post
//!    (`removed_by_category`). That is the moderator's verdict on our judgement,
//!    and it is read, not guessed at from low scores.
//! 2. **A community that removed us is not posted to again unattended.** Its
//!    drafts go to a person.
//! 3. **Reddit's own filters, or repeated removals, halt the account.** A
//!    site-filter removal is how a shadowban or spam flag first shows itself;
//!    two moderator removals in a month say the posts are wrong, not the
//!    moderators. Either holds every draft for a person until the window
//!    passes.
//! 4. **The daily ceiling is earned.** One post a day until posts have
//!    demonstrably survived: each block of posts that stayed up for a week —
//!    seen live by a removal-aware read, not merely unreported — earns one
//!    more, to a hard ceiling. Any removal in the window drops it back.
//!
//! Fail closed: a post whose removal state was never read has not survived.
//! It earns nothing and proves nothing.

use time::{Duration, OffsetDateTime};

/// Who removed a post, from Reddit's `removed_by_category`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemovalCause {
    /// A subreddit moderator removed it.
    Moderator,
    /// The subreddit's AutoModerator filtered it — the community's own rules,
    /// applied by a bot. Counted like a moderator: the rules said no.
    AutoModerator,
    /// Reddit itself — the spam filter, anti-evil operations, a takedown.
    /// The account-level signal: this is how a shadowban first shows.
    SiteFilter,
    /// The author deleted it. Not a verdict on the post by anybody else.
    AuthorDeleted,
}

impl RemovalCause {
    /// Maps Reddit's `removed_by_category`. An unknown non-empty category is
    /// treated as a moderator removal: an unrecognised "removed" is still
    /// removed, and reading it as harmless is the unsafe direction.
    #[must_use]
    pub fn from_category(category: &str) -> Option<Self> {
        match category.trim().to_ascii_lowercase().as_str() {
            "" => None,
            "moderator" => Some(Self::Moderator),
            "automod_filtered" => Some(Self::AutoModerator),
            "reddit" | "anti_evil_ops" | "content_takedown" | "copyright_takedown"
            | "community_ops" | "legal_operations" => Some(Self::SiteFilter),
            "deleted" | "author" => Some(Self::AuthorDeleted),
            _ => Some(Self::Moderator),
        }
    }

    /// Whether this removal is somebody else's verdict on the post.
    #[must_use]
    pub const fn is_verdict(self) -> bool {
        !matches!(self, Self::AuthorDeleted)
    }
}

/// One published post, as the standing reads it.
#[derive(Clone, Debug)]
pub struct PostRecord {
    /// Normalised subreddit name.
    pub subreddit: String,
    pub posted_at: OffsetDateTime,
    /// Set once a read saw the post removed.
    pub removal: Option<RemovalCause>,
    /// When the removal was first seen. Falls back to `posted_at` if absent.
    pub removal_seen_at: Option<OffsetDateTime>,
    /// The latest removal-aware read that saw the post live. `None` when no
    /// read could establish removal state at all.
    pub last_seen_live_at: Option<OffsetDateTime>,
}

/// How long a removal keeps the account halted or the ceiling at its floor.
pub const REMOVAL_WINDOW: Duration = Duration::days(30);
/// Moderator removals inside the window that halt the account.
pub const REPEATED_REMOVALS_HALT: usize = 2;
/// How long a community that removed us stays unattended-off.
pub const SUBREDDIT_MEMORY: Duration = Duration::days(180);
/// A post must be at least this old to count as survived.
pub const SURVIVAL_AGE: Duration = Duration::days(7);
/// …and must have been seen live at least this long after posting. Automod
/// acts at once and moderators within a day or two; a post last checked an
/// hour after going up has not survived anything.
pub const SURVIVAL_OBSERVED_AFTER: Duration = Duration::hours(48);
/// A fresh automation does not get to experiment on the band's public
/// identity. At least this many posts must have been published and then
/// observed alive past the moderation window before unattended posting is
/// earned. Those seed posts may be published manually; the point is that a
/// person proves the room/copy fit before the machine is trusted with it.
pub const MIN_SURVIVED_POSTS_FOR_AUTONOMY: usize = 3;
/// Survived posts that earn one more post per day.
pub const SURVIVED_POSTS_PER_STEP: usize = 5;
/// The floor: one post a day, which reads as somebody who posts occasionally.
pub const BASE_DAILY_CAP: u32 = 1;
/// The ceiling, however long the record. Reddit reads volume from one account
/// as a pattern long before it reads any single post.
pub const MAX_DAILY_CAP: u32 = 3;
/// Only survivals this recent count toward the ceiling.
const SURVIVAL_LOOKBACK: Duration = Duration::days(60);

/// Why unattended posting is halted. The sentence is what an operator reads
/// beside every draft it holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HaltReason {
    SiteFilterRemoval,
    RepeatedRemovals,
}

impl HaltReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SiteFilterRemoval => {
                "held: Reddit's own filters removed one of our posts in the last 30 days \
                 — unattended posting is halted to protect the account; check it is not \
                 shadowbanned before posting by hand"
            }
            Self::RepeatedRemovals => {
                "held: moderators removed two or more of our posts in the last 30 days \
                 — unattended posting is halted until the posts change, not the moderators"
            }
        }
    }
}

/// Whether the executor may post unattended, and how many a day.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedditStanding {
    Open { daily_cap: u32 },
    Halted(HaltReason),
}

/// The account's standing from its post history.
#[must_use]
pub fn reddit_standing(history: &[PostRecord], now: OffsetDateTime) -> RedditStanding {
    let recent_verdicts: Vec<RemovalCause> = history
        .iter()
        .filter_map(|post| {
            let cause = post.removal.filter(|cause| cause.is_verdict())?;
            let seen = post.removal_seen_at.unwrap_or(post.posted_at);
            (now - seen <= REMOVAL_WINDOW).then_some(cause)
        })
        .collect();
    if recent_verdicts.contains(&RemovalCause::SiteFilter) {
        return RedditStanding::Halted(HaltReason::SiteFilterRemoval);
    }
    if recent_verdicts.len() >= REPEATED_REMOVALS_HALT {
        return RedditStanding::Halted(HaltReason::RepeatedRemovals);
    }
    if !recent_verdicts.is_empty() {
        return RedditStanding::Open {
            daily_cap: BASE_DAILY_CAP,
        };
    }
    let survived = history.iter().filter(|post| survived(post, now)).count();
    let earned = u32::try_from(survived / SURVIVED_POSTS_PER_STEP).unwrap_or(u32::MAX);
    RedditStanding::Open {
        daily_cap: BASE_DAILY_CAP.saturating_add(earned).min(MAX_DAILY_CAP),
    }
}

/// Whether unattended posting has earned the right to use the account.
///
/// Rate limits answer "how much"; this answers the prior question "may the
/// machine publish at all yet?". A clean slate is not evidence of judgement.
/// The first posts are the calibration set a person publishes and the worker
/// observes for removals. Only then may automation take over.
#[must_use]
pub fn autonomy_proven(history: &[PostRecord], now: OffsetDateTime) -> bool {
    history.iter().filter(|post| survived(post, now)).count() >= MIN_SURVIVED_POSTS_FOR_AUTONOMY
}

/// Whether a community's moderators (or its AutoModerator) removed one of our
/// posts recently enough that its drafts should go to a person instead.
#[must_use]
pub fn community_removed_us(history: &[PostRecord], subreddit: &str, now: OffsetDateTime) -> bool {
    history.iter().any(|post| {
        post.subreddit.eq_ignore_ascii_case(subreddit)
            && post.removal.is_some_and(RemovalCause::is_verdict)
            && now - post.removal_seen_at.unwrap_or(post.posted_at) <= SUBREDDIT_MEMORY
    })
}

/// The sentence an operator reads on a draft held by [`community_removed_us`].
pub const COMMUNITY_REMOVED_US: &str = "held: this community's moderators removed one of our \
     posts in the last 180 days — post here by hand, after reading its rules, or not at all";

fn survived(post: &PostRecord, now: OffsetDateTime) -> bool {
    post.removal.is_none()
        && now - post.posted_at >= SURVIVAL_AGE
        && now - post.posted_at <= SURVIVAL_LOOKBACK
        && post
            .last_seen_live_at
            .is_some_and(|seen| seen - post.posted_at >= SURVIVAL_OBSERVED_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn live(days_ago: i64) -> PostRecord {
        let posted_at = now() - Duration::days(days_ago);
        PostRecord {
            subreddit: "metal".to_owned(),
            posted_at,
            removal: None,
            removal_seen_at: None,
            last_seen_live_at: Some(posted_at + Duration::hours(60)),
        }
    }

    fn removed(days_ago: i64, cause: RemovalCause, subreddit: &str) -> PostRecord {
        let posted_at = now() - Duration::days(days_ago);
        PostRecord {
            subreddit: subreddit.to_owned(),
            posted_at,
            removal: Some(cause),
            removal_seen_at: Some(posted_at + Duration::hours(3)),
            last_seen_live_at: None,
        }
    }

    #[test]
    fn a_fresh_account_posts_once_a_day() {
        assert_eq!(
            reddit_standing(&[], now()),
            RedditStanding::Open { daily_cap: 1 }
        );
    }

    #[test]
    fn a_fresh_account_has_not_earned_unattended_posting() {
        assert!(!autonomy_proven(&[], now()));

        let two: Vec<_> = (8..10).map(live).collect();
        assert!(!autonomy_proven(&two, now()));

        let three: Vec<_> = (8..11).map(live).collect();
        assert!(autonomy_proven(&three, now()));
    }

    #[test]
    fn unchecked_posts_do_not_earn_autonomy() {
        let history: Vec<_> = (8..20)
            .map(|days| PostRecord {
                last_seen_live_at: None,
                ..live(days)
            })
            .collect();
        assert!(!autonomy_proven(&history, now()));
    }

    #[test]
    fn survived_posts_earn_more_up_to_the_ceiling() {
        let five: Vec<_> = (8..13).map(live).collect();
        assert_eq!(
            reddit_standing(&five, now()),
            RedditStanding::Open { daily_cap: 2 }
        );
        let many: Vec<_> = (8..40).map(live).collect();
        assert_eq!(
            reddit_standing(&many, now()),
            RedditStanding::Open {
                daily_cap: MAX_DAILY_CAP
            }
        );
    }

    #[test]
    fn a_post_nobody_could_check_has_not_survived() {
        let unchecked: Vec<_> = (8..20)
            .map(|days| PostRecord {
                last_seen_live_at: None,
                ..live(days)
            })
            .collect();
        assert_eq!(
            reddit_standing(&unchecked, now()),
            RedditStanding::Open { daily_cap: 1 }
        );
        // Seen live only an hour after posting: not long enough to count.
        let early: Vec<_> = (8..20)
            .map(|days| {
                let post = live(days);
                PostRecord {
                    last_seen_live_at: Some(post.posted_at + Duration::hours(1)),
                    ..post
                }
            })
            .collect();
        assert_eq!(
            reddit_standing(&early, now()),
            RedditStanding::Open { daily_cap: 1 }
        );
    }

    #[test]
    fn one_moderator_removal_drops_to_the_floor() {
        let mut history: Vec<_> = (8..40).map(live).collect();
        history.push(removed(5, RemovalCause::Moderator, "doom"));
        assert_eq!(
            reddit_standing(&history, now()),
            RedditStanding::Open { daily_cap: 1 }
        );
    }

    #[test]
    fn two_moderator_removals_halt() {
        let history = [
            removed(5, RemovalCause::Moderator, "doom"),
            removed(12, RemovalCause::AutoModerator, "sludge"),
        ];
        assert_eq!(
            reddit_standing(&history, now()),
            RedditStanding::Halted(HaltReason::RepeatedRemovals)
        );
    }

    #[test]
    fn one_site_filter_removal_halts() {
        assert_eq!(
            reddit_standing(&[removed(2, RemovalCause::SiteFilter, "metal")], now()),
            RedditStanding::Halted(HaltReason::SiteFilterRemoval)
        );
    }

    #[test]
    fn removals_age_out_and_author_deletions_never_count() {
        let history = [
            removed(45, RemovalCause::SiteFilter, "metal"),
            removed(3, RemovalCause::AuthorDeleted, "doom"),
            removed(4, RemovalCause::AuthorDeleted, "doom"),
        ];
        assert_eq!(
            reddit_standing(&history, now()),
            RedditStanding::Open { daily_cap: 1 }
        );
        assert!(!community_removed_us(&history, "doom", now()));
    }

    #[test]
    fn a_community_that_removed_us_is_remembered_for_half_a_year() {
        let history = [removed(100, RemovalCause::Moderator, "Doom")];
        assert!(community_removed_us(&history, "doom", now()));
        assert!(!community_removed_us(&history, "sludge", now()));
        let old = [removed(200, RemovalCause::Moderator, "doom")];
        assert!(!community_removed_us(&old, "doom", now()));
    }

    #[test]
    fn categories_map_conservatively() {
        assert_eq!(RemovalCause::from_category(""), None);
        assert_eq!(
            RemovalCause::from_category("anti_evil_ops"),
            Some(RemovalCause::SiteFilter)
        );
        assert_eq!(
            RemovalCause::from_category("automod_filtered"),
            Some(RemovalCause::AutoModerator)
        );
        assert_eq!(
            RemovalCause::from_category("deleted"),
            Some(RemovalCause::AuthorDeleted)
        );
        // Unknown is removed, not harmless.
        assert_eq!(
            RemovalCause::from_category("something_new"),
            Some(RemovalCause::Moderator)
        );
    }
}
