//! What one opportunity needs from another before it is worth anything.
//!
//! The portfolio ranks candidates against each other and treats them as
//! independent apart from audience overlap and fatigue. Some are not
//! independent: one has to happen before another can, and doing one can make a
//! second worth more than it was.
//!
//! The live case is joining a community before posting to it. The brain picks
//! posts by expected value; a separate worker picks joins by member count.
//! Neither knows about the other, so the brain can select a post to a
//! community nobody has joined — which many subreddits refuse outright, and
//! which is the pattern that gets an account flagged. The account it would be
//! flagged on is the one the whole discovery loop reads through.
//!
//! # Three edges, and why only three
//!
//! Ported from Kern's opportunity graph, which uses the same three:
//!
//! - [`DependencyKind::Prerequisite`] — `from` must be done before `to` can be
//!   attempted at all. An unmet prerequisite makes `to` ineligible, not
//!   cheaper: a post that will be refused is not a low-value post.
//! - [`DependencyKind::Reinforcement`] — doing `from` raises what `to` is
//!   worth. The prerequisite case reads backwards through this: a join is
//!   worth more when the brain has a post waiting behind it.
//! - [`DependencyKind::Exclusive`] — only one of the two may be done. The
//!   portfolio already expresses a soft version through audience overlap and
//!   fatigue; this is the hard version, for pairs that must never both run.
//!
//! # Deterministic
//!
//! Pure structure and pure arithmetic. The same graph and the same candidate
//! set produce the same verdict every time, which is what lets a decision
//! record say it was blocked by a named prerequisite rather than by a number
//! nobody can reconstruct.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// How one opportunity depends on another.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    /// `from` must be satisfied before `to` may be attempted.
    #[default]
    Prerequisite,
    /// `from` raises what `to` is worth, without gating it.
    Reinforcement,
    /// `from` and `to` must never both be done.
    Exclusive,
}

/// One directed edge between two opportunity keys.
///
/// Keys are opaque strings so the graph can span kinds that share no id type —
/// a `discovery_places` row and a growth-intelligence candidate are the live
/// pair and have nothing in common but the community they refer to.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Dependency {
    pub from: String,
    pub to: String,
    pub kind: DependencyKind,
}

/// The dependencies among a cycle's opportunities.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpportunityGraph {
    edges: Vec<Dependency>,
    /// Keys already satisfied — a community joined, a release announced.
    satisfied: BTreeSet<String>,
}

/// Why a candidate cannot be acted on this cycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockedCandidate {
    /// The candidate that cannot run.
    pub opportunity: String,
    /// What it is waiting for.
    pub waiting_on: String,
    /// One line for the decision record and the operator queue.
    pub reason: String,
}

/// How much a prerequisite's own value is raised per candidate waiting on it.
///
/// A join nobody is waiting behind is worth what it was. A join with three
/// posts queued behind it is the thing standing between the brain and three
/// dispatches, and ordering it below a larger community the brain has no plans
/// for is how the two workers end up pulling in different directions.
///
/// Additive and capped rather than multiplicative: a community with twenty
/// waiting posts is not twenty times more urgent than one with a single post,
/// and an unbounded multiplier would let one popular community starve every
/// other join forever.
const DEMAND_BONUS_PER_WAITER: f64 = 0.25;
/// The most a prerequisite's value may be raised by demand, whatever the
/// queue behind it.
const MAX_DEMAND_BONUS: f64 = 1.0;

impl OpportunityGraph {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an edge. Duplicate edges are ignored rather than doubled — the
    /// same prerequisite recorded twice is one prerequisite.
    pub fn add(&mut self, from: impl Into<String>, to: impl Into<String>, kind: DependencyKind) {
        let dependency = Dependency {
            from: from.into(),
            to: to.into(),
            kind,
        };
        if !self.edges.contains(&dependency) {
            self.edges.push(dependency);
        }
    }

    /// Records that a key is already satisfied.
    pub fn satisfy(&mut self, key: impl Into<String>) {
        self.satisfied.insert(key.into());
    }

    #[must_use]
    pub fn is_satisfied(&self, key: &str) -> bool {
        self.satisfied.contains(key)
    }

    /// Whether one candidate is held by an unmet prerequisite.
    ///
    /// The single-candidate form of [`Self::blocked`], for a caller walking a
    /// list and deciding one at a time. Same rule: only prerequisites gate.
    #[must_use]
    pub fn is_blocked(&self, candidate: &str) -> bool {
        self.edges.iter().any(|edge| {
            edge.kind == DependencyKind::Prerequisite
                && edge.to == candidate
                && !self.satisfied.contains(&edge.from)
        })
    }

    /// The candidates that cannot run, and what each is waiting for.
    ///
    /// Only prerequisites gate. A missing reinforcement costs value and blocks
    /// nothing, which is the difference between the two edges.
    #[must_use]
    pub fn blocked<'a>(
        &self,
        candidates: impl IntoIterator<Item = &'a str>,
    ) -> Vec<BlockedCandidate> {
        let candidates: BTreeSet<&str> = candidates.into_iter().collect();
        self.edges
            .iter()
            .filter(|edge| edge.kind == DependencyKind::Prerequisite)
            .filter(|edge| candidates.contains(edge.to.as_str()))
            .filter(|edge| !self.satisfied.contains(&edge.from))
            .map(|edge| BlockedCandidate {
                opportunity: edge.to.clone(),
                waiting_on: edge.from.clone(),
                reason: format!("waiting on {}", edge.from),
            })
            .collect()
    }

    /// How much each unmet prerequisite's own value should be raised, keyed by
    /// the prerequisite.
    ///
    /// This is the reinforcement edge read backwards. The brain does not pick
    /// joins — a separate worker does — so the graph cannot make the brain
    /// dispatch one. What it can do is tell that worker which unjoined
    /// community the brain is actually waiting on, so the two stop choosing
    /// independently.
    #[must_use]
    pub fn prerequisite_demand<'a>(
        &self,
        candidates: impl IntoIterator<Item = &'a str>,
    ) -> BTreeMap<String, f64> {
        let mut waiters: BTreeMap<String, u32> = BTreeMap::new();
        for blocked in self.blocked(candidates) {
            *waiters.entry(blocked.waiting_on).or_default() += 1;
        }
        waiters
            .into_iter()
            .map(|(key, count)| {
                let bonus = (f64::from(count) * DEMAND_BONUS_PER_WAITER).min(MAX_DEMAND_BONUS);
                (key, bonus)
            })
            .collect()
    }

    /// Candidates that must not run because an exclusive partner is already
    /// selected this cycle.
    ///
    /// Order matters and is the caller's: the portfolio selects greedily by
    /// value, so passing `selected` in selection order means the better
    /// candidate keeps the slot.
    #[must_use]
    pub fn excluded_by<'a>(&self, selected: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
        let selected: BTreeSet<&str> = selected.into_iter().collect();
        self.edges
            .iter()
            .filter(|edge| edge.kind == DependencyKind::Exclusive)
            .flat_map(|edge| {
                // Exclusion is symmetric even though the edge is stored
                // directed: "only one of these two" does not have a direction.
                [
                    (edge.from.as_str(), edge.to.as_str()),
                    (edge.to.as_str(), edge.from.as_str()),
                ]
            })
            .filter(|(one, _)| selected.contains(one))
            .map(|(_, other)| (*other).to_owned())
            .filter(|other| !selected.contains(other.as_str()))
            .collect()
    }
}

/// The graph key for a community the brain may post to.
///
/// One function rather than a format string at each site: the join worker and
/// the growth cycle have to agree on the key exactly, and they live in
/// different crates.
#[must_use]
pub fn community_membership_key(subreddit: &str) -> String {
    format!("community_joined:{}", subreddit.trim().to_lowercase())
}

/// The graph key for a post to a community.
#[must_use]
pub fn community_post_key(target_id: uuid::Uuid) -> String {
    format!("community_post:{target_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined_graph() -> (OpportunityGraph, String, String) {
        let target = uuid::Uuid::from_u128(1);
        let post = community_post_key(target);
        let membership = community_membership_key("r/Metal");
        let mut graph = OpportunityGraph::new();
        graph.add(
            membership.clone(),
            post.clone(),
            DependencyKind::Prerequisite,
        );
        (graph, post, membership)
    }

    #[test]
    fn a_post_to_an_unjoined_community_is_blocked() {
        let (graph, post, membership) = joined_graph();
        let blocked = graph.blocked([post.as_str()]);
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].opportunity, post);
        assert_eq!(blocked[0].waiting_on, membership);
    }

    #[test]
    fn joining_unblocks_the_post() {
        let (mut graph, post, membership) = joined_graph();
        graph.satisfy(membership);
        assert!(
            graph.blocked([post.as_str()]).is_empty(),
            "a joined community is a met prerequisite"
        );
    }

    /// The key is what the two crates agree on, so casing and whitespace must
    /// not be able to split it in two.
    #[test]
    fn the_membership_key_is_stable_across_spelling() {
        assert_eq!(
            community_membership_key(" r/Metal "),
            community_membership_key("r/metal")
        );
    }

    #[test]
    fn demand_rises_with_the_queue_and_stops_rising() {
        let membership = community_membership_key("r/metal");
        let mut graph = OpportunityGraph::new();
        let posts: Vec<String> = (0..10)
            .map(|n| community_post_key(uuid::Uuid::from_u128(n)))
            .collect();
        for post in &posts {
            graph.add(
                membership.clone(),
                post.clone(),
                DependencyKind::Prerequisite,
            );
        }

        let one = graph.prerequisite_demand([posts[0].as_str()]);
        let two = graph.prerequisite_demand(posts.iter().take(2).map(String::as_str));
        assert!(
            two[&membership] > one[&membership],
            "a second post waiting makes the join more urgent"
        );

        let all = graph.prerequisite_demand(posts.iter().map(String::as_str));
        assert!(
            (all[&membership] - MAX_DEMAND_BONUS).abs() < f64::EPSILON,
            "and the bonus stops, or one community starves every other join"
        );
    }

    #[test]
    fn a_satisfied_prerequisite_generates_no_demand() {
        let (mut graph, post, membership) = joined_graph();
        graph.satisfy(membership);
        assert!(
            graph.prerequisite_demand([post.as_str()]).is_empty(),
            "nothing is waiting on a community already joined"
        );
    }

    /// Reinforcement changes what something is worth; it does not gate.
    #[test]
    fn a_reinforcement_edge_blocks_nothing() {
        let mut graph = OpportunityGraph::new();
        graph.add(
            "press_coverage",
            "social_post",
            DependencyKind::Reinforcement,
        );
        assert!(graph.blocked(["social_post"]).is_empty());
    }

    #[test]
    fn an_exclusive_partner_is_excluded_in_either_direction() {
        let mut graph = OpportunityGraph::new();
        graph.add("post_a", "post_b", DependencyKind::Exclusive);
        assert_eq!(
            graph.excluded_by(["post_a"]),
            BTreeSet::from(["post_b".to_owned()]),
            "selecting one excludes the other"
        );
        assert_eq!(
            graph.excluded_by(["post_b"]),
            BTreeSet::from(["post_a".to_owned()]),
            "and the edge has no direction as far as exclusion is concerned"
        );
    }

    #[test]
    fn the_same_prerequisite_recorded_twice_is_one_prerequisite() {
        let (mut graph, post, membership) = joined_graph();
        graph.add(membership, post.clone(), DependencyKind::Prerequisite);
        assert_eq!(graph.blocked([post.as_str()]).len(), 1);
    }
}
