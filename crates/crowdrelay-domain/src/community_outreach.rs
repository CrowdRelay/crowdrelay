//! Community outreach: researched target communities get packaged into
//! weekly assignments for the social-skill team member.
//!
//! The community list is operator-curated (seeded once, maintained by hand).
//! The brain's job is narrow: pick what's due this week, generate tracked
//! links, compose the post in the right language, and route to whoever has
//! the social skill. It does not post autonomously — platform ToS forbid it.

use serde::{Deserialize, Serialize};

/// One researched community target. Seeded by the operator; the brain reads,
/// ranks and packages but never invents communities.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OutreachTarget {
    pub id: uuid::Uuid,
    pub community_name: String,
    pub platform: String,
    pub url: String,
    pub country_code: String,
    pub language: String,
    pub self_promo_policy: SelfPromoPolicy,
    pub priority: u16,
    pub active: bool,
    /// Days since last assignment. `None` = never assigned.
    pub days_since_last: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelfPromoPolicy {
    Tolerant,
    Strict,
    MegathreadOnly,
    Prohibited,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct OutreachCadencePolicy {
    /// Re-assign a community after this many days since last touch.
    pub reassign_after_days: u32,
    /// Max communities per weekly pack.
    pub pack_size: u16,
    pub minimum_priority: u16,
}

impl Default for OutreachCadencePolicy {
    fn default() -> Self {
        Self {
            reassign_after_days: 7,
            pack_size: 3,
            minimum_priority: 30,
        }
    }
}

/// The complete package delivered to the social person.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OutreachPack {
    pub targets: Vec<PackEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PackEntry {
    pub community_name: String,
    pub url: String,
    pub country_code: String,
    pub tracked_link_slug: String,
    pub tracked_link_url: String,
    pub post_title_pl: String,
    pub post_body_pl: String,
    pub policy_note: String,
}

/// Selects which communities are due for outreach this week.
///
/// Due means: active, priority above floor, not assigned within the cooldown.
/// Higher priority first; ties broken alphabetically for determinism.
#[must_use]
pub fn select_due_communities(
    targets: &[OutreachTarget],
    policy: OutreachCadencePolicy,
) -> Vec<&OutreachTarget> {
    let mut due: Vec<&OutreachTarget> = targets
        .iter()
        .filter(|target| {
            target.active
                && target.priority >= policy.minimum_priority
                && match target.days_since_last {
                    None => true,
                    Some(days) => days >= policy.reassign_after_days,
                }
                && !matches!(target.self_promo_policy, SelfPromoPolicy::Prohibited)
        })
        .collect();
    due.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.community_name.cmp(&b.community_name))
    });
    due.truncate(policy.pack_size as usize);
    due
}

/// Generates a tracked smart-link slug from the community name + rotation week.
#[must_use]
pub fn tracked_slug(community_platform: &str, week_number: u32) -> String {
    format!("outreach-{community_platform}-w{week_number}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, priority: u16, days_since: Option<u32>) -> OutreachTarget {
        OutreachTarget {
            id: uuid::Uuid::now_v7(),
            community_name: name.into(),
            platform: "reddit".into(),
            url: "https://reddit.com/r/test".into(),
            country_code: "PL".into(),
            language: "pl".into(),
            self_promo_policy: SelfPromoPolicy::Tolerant,
            priority,
            active: true,
            days_since_last: days_since,
        }
    }

    #[test]
    fn picks_highest_priority_unassigned_first() {
        let policy = OutreachCadencePolicy::default();
        let targets = vec![
            target("low", 40, None),
            target("high", 80, None),
            target("medium", 60, Some(1)),
        ];
        let due = select_due_communities(&targets, policy);
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].community_name, "high");
        assert_eq!(due[1].community_name, "low");
    }

    #[test]
    fn recently_assigned_communities_are_skipped() {
        let policy = OutreachCadencePolicy::default();
        let fresh = vec![target("fresh", 90, Some(2))];
        assert!(select_due_communities(&fresh, policy).is_empty());
    }

    #[test]
    fn prohibited_communities_never_appear() {
        let policy = OutreachCadencePolicy::default();
        let mut banned = target("banned", 100, None);
        banned.self_promo_policy = SelfPromoPolicy::Prohibited;
        assert!(select_due_communities(&[banned], policy).is_empty());
    }

    #[test]
    fn pack_size_caps_the_batch() {
        let policy = OutreachCadencePolicy {
            pack_size: 2,
            ..Default::default()
        };
        let many: Vec<OutreachTarget> = (0..5)
            .map(|i| target(&format!("c{i}"), 50 + i as u16 * 10, None))
            .collect();
        assert_eq!(select_due_communities(&many, policy).len(), 2);
    }
}
