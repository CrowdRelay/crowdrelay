//! Opportunity graph — a directed graph of growth opportunities and their
//! dependencies.
//!
//! The brain uses this to plan multi-step pathways (e.g., "scan Reddit" →
//! "engage community" → "invite to Signal"). Each node is an opportunity keyed
//! by its stable [`OpportunityId`](crate::opportunity::OpportunityId) string,
//! and each edge describes how one opportunity depends on or reinforces
//! another.
//!
//! # Edge semantics
//!
//! - [`DependencyKind::Prerequisite`]: `from` must be done before `to`.
//! - [`DependencyKind::Enables`]: doing `from` makes `to` more effective.
//! - [`DependencyKind::Reinforces`]: doing both is better than either alone.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

/// The lifecycle status of a node in the opportunity graph.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// The opportunity has not been started yet.
    #[default]
    Open,
    /// The opportunity is currently being pursued.
    InProgress,
    /// The opportunity has been completed successfully.
    Completed,
    /// The opportunity failed or was abandoned.
    Failed,
}

/// How one opportunity depends on or relates to another.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    /// `from` must be done before `to`.
    Prerequisite,
    /// Doing `from` makes `to` more effective.
    Enables,
    /// Doing both is better than either alone.
    Reinforces,
}

/// A node in the opportunity graph — a single growth opportunity.
#[derive(Clone, Debug, Serialize)]
pub struct OpportunityNode {
    /// The stable opportunity ID string (see [`OpportunityId`](crate::opportunity::OpportunityId)).
    pub key: String,
    /// The worker template that would address this opportunity.
    pub template_id: String,
    /// Expected number of incremental fans if this opportunity is pursued.
    pub expected_fans: f64,
    /// Current lifecycle status of the node.
    pub status: NodeStatus,
}

/// A directed edge between two opportunities in the graph.
#[derive(Clone, Debug, Serialize)]
pub struct OpportunityEdge {
    /// The source opportunity key.
    pub from: String,
    /// The target opportunity key.
    pub to: String,
    /// How `from` relates to `to`.
    pub dependency_kind: DependencyKind,
}

/// A directed graph of growth opportunities and their dependencies.
///
/// Nodes are keyed by their opportunity ID string. Edges describe
/// prerequisite, enabling, or reinforcing relationships between opportunities.
/// The graph supports predecessor/successor lookup and reachability checks
/// (`has_path`) that are safe against cycles.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OpportunityGraph {
    /// Nodes keyed by opportunity key.
    pub nodes: HashMap<String, OpportunityNode>,
    /// Directed edges between opportunities.
    pub edges: Vec<OpportunityEdge>,
}

impl OpportunityGraph {
    /// Creates a new empty opportunity graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a node to the graph, keyed by `node.key`.
    ///
    /// If a node with the same key already exists it is replaced.
    pub fn add_node(&mut self, node: OpportunityNode) {
        self.nodes.insert(node.key.clone(), node);
    }

    /// Adds a directed edge to the graph.
    ///
    /// The edge is appended even if one or both endpoints are not yet present
    /// as nodes; callers are responsible for ensuring endpoint keys exist when
    /// querying predecessors/successors.
    pub fn add_edge(&mut self, edge: OpportunityEdge) {
        self.edges.push(edge);
    }

    /// Returns the nodes that have an edge pointing *to* `key` (i.e., the
    /// predecessors of `key`).
    pub fn predecessors(&self, key: &str) -> Vec<&OpportunityNode> {
        self.edges
            .iter()
            .filter(|edge| edge.to == key)
            .filter_map(|edge| self.nodes.get(&edge.from))
            .collect()
    }

    /// Returns the nodes that `key` has an edge pointing *to* (i.e., the
    /// successors of `key`).
    pub fn successors(&self, key: &str) -> Vec<&OpportunityNode> {
        self.edges
            .iter()
            .filter(|edge| edge.from == key)
            .filter_map(|edge| self.nodes.get(&edge.to))
            .collect()
    }

    /// Returns `true` if there is a directed path of edges from `from` to `to`.
    ///
    /// Uses an iterative breadth-first traversal with a visited set, so cycles
    /// in the graph do not cause an infinite loop. A node is considered to
    /// have a path to itself only if there is a non-trivial cycle back to it.
    #[must_use]
    pub fn has_path(&self, from: &str, to: &str) -> bool {
        if from == to {
            // A trivial path (same node) is only true if a cycle returns to it.
            return self.successor_keys(from).any(|s| self.reaches(s, from));
        }
        self.reaches(from, to)
    }

    /// Internal helper: returns the keys of nodes that `key` points to.
    fn successor_keys(&self, key: &str) -> impl Iterator<Item = &str> {
        self.edges
            .iter()
            .filter(move |edge| edge.from == key)
            .map(|edge| edge.to.as_str())
    }

    /// Internal helper: iterative BFS reachability from `start` to `target`,
    /// safe against cycles.
    fn reaches(&self, start: &str, target: &str) -> bool {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = vec![start];

        while let Some(current) = stack.pop() {
            if !visited.insert(current) {
                continue;
            }
            for next in self.successor_keys(current) {
                if next == target {
                    return true;
                }
                if !visited.contains(next) {
                    stack.push(next);
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(key: &str, template_id: &str, expected_fans: f64) -> OpportunityNode {
        OpportunityNode {
            key: key.to_owned(),
            template_id: template_id.to_owned(),
            expected_fans,
            status: NodeStatus::Open,
        }
    }

    fn edge(from: &str, to: &str, kind: DependencyKind) -> OpportunityEdge {
        OpportunityEdge {
            from: from.to_owned(),
            to: to.to_owned(),
            dependency_kind: kind,
        }
    }

    #[test]
    fn graph_construction() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("scan_reddit", "reddit-scanner", 0.0));
        graph.add_node(node("engage", "community-engager", 5.0));
        graph.add_node(node("invite", "signal-inviter", 10.0));

        graph.add_edge(edge("scan_reddit", "engage", DependencyKind::Prerequisite));
        graph.add_edge(edge("engage", "invite", DependencyKind::Enables));

        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.edges.len(), 2);
        assert!(graph.nodes.contains_key("scan_reddit"));
    }

    #[test]
    fn add_node_replaces_existing_key() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("a", "t1", 1.0));
        graph.add_node(node("a", "t2", 2.0));

        assert_eq!(graph.nodes.len(), 1);
        let node = &graph.nodes["a"];
        assert_eq!(node.template_id, "t2");
        assert_eq!(node.expected_fans, 2.0);
    }

    #[test]
    fn predecessors_lookup() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("scan_reddit", "reddit-scanner", 0.0));
        graph.add_node(node("engage", "community-engager", 5.0));
        graph.add_node(node("invite", "signal-inviter", 10.0));

        graph.add_edge(edge("scan_reddit", "engage", DependencyKind::Prerequisite));
        graph.add_edge(edge("engage", "invite", DependencyKind::Enables));

        let preds: Vec<&str> = graph
            .predecessors("engage")
            .iter()
            .map(|n| n.key.as_str())
            .collect();
        assert_eq!(preds, vec!["scan_reddit"]);

        let preds_invite: Vec<&str> = graph
            .predecessors("invite")
            .iter()
            .map(|n| n.key.as_str())
            .collect();
        assert_eq!(preds_invite, vec!["engage"]);

        assert!(graph.predecessors("scan_reddit").is_empty());
    }

    #[test]
    fn successors_lookup() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("scan_reddit", "reddit-scanner", 0.0));
        graph.add_node(node("engage", "community-engager", 5.0));
        graph.add_node(node("invite", "signal-inviter", 10.0));

        graph.add_edge(edge("scan_reddit", "engage", DependencyKind::Prerequisite));
        graph.add_edge(edge("engage", "invite", DependencyKind::Enables));

        let succ: Vec<&str> = graph
            .successors("scan_reddit")
            .iter()
            .map(|n| n.key.as_str())
            .collect();
        assert_eq!(succ, vec!["engage"]);

        let succ_engage: Vec<&str> = graph
            .successors("engage")
            .iter()
            .map(|n| n.key.as_str())
            .collect();
        assert_eq!(succ_engage, vec!["invite"]);

        assert!(graph.successors("invite").is_empty());
    }

    #[test]
    fn has_path_direct_and_transitive() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("a", "t", 1.0));
        graph.add_node(node("b", "t", 2.0));
        graph.add_node(node("c", "t", 3.0));

        graph.add_edge(edge("a", "b", DependencyKind::Prerequisite));
        graph.add_edge(edge("b", "c", DependencyKind::Enables));

        assert!(graph.has_path("a", "b"));
        assert!(graph.has_path("b", "c"));
        assert!(graph.has_path("a", "c"));
        assert!(!graph.has_path("c", "a"));
        assert!(!graph.has_path("b", "a"));
    }

    #[test]
    fn has_path_no_path_for_disconnected_nodes() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("a", "t", 1.0));
        graph.add_node(node("b", "t", 2.0));
        graph.add_node(node("c", "t", 3.0));

        graph.add_edge(edge("a", "b", DependencyKind::Prerequisite));

        assert!(!graph.has_path("a", "c"));
        assert!(!graph.has_path("c", "b"));
    }

    #[test]
    fn has_path_terminates_on_cycles() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("a", "t", 1.0));
        graph.add_node(node("b", "t", 2.0));
        graph.add_node(node("c", "t", 3.0));

        // a -> b -> c -> b (cycle between b and c)
        graph.add_edge(edge("a", "b", DependencyKind::Prerequisite));
        graph.add_edge(edge("b", "c", DependencyKind::Enables));
        graph.add_edge(edge("c", "b", DependencyKind::Reinforces));

        // Should not infinite-loop; a reaches c via the cycle.
        assert!(graph.has_path("a", "c"));
        // b reaches c and c reaches b (cycle).
        assert!(graph.has_path("b", "c"));
        assert!(graph.has_path("c", "b"));
        // a cannot reach a (no cycle back to a).
        assert!(!graph.has_path("a", "a"));
        // b reaches itself via the b <-> c cycle.
        assert!(graph.has_path("b", "b"));
    }

    #[test]
    fn has_path_self_cycle_detected() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("a", "t", 1.0));
        graph.add_node(node("b", "t", 2.0));

        graph.add_edge(edge("a", "b", DependencyKind::Prerequisite));
        graph.add_edge(edge("b", "a", DependencyKind::Reinforces));

        // a -> b -> a is a cycle, so a has a path to itself.
        assert!(graph.has_path("a", "a"));
        assert!(graph.has_path("b", "b"));
    }

    #[test]
    fn has_path_unknown_key_returns_false() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("a", "t", 1.0));
        graph.add_edge(edge("a", "b", DependencyKind::Prerequisite));

        // "b" has no node but the edge exists; has_path should still traverse.
        assert!(graph.has_path("a", "b"));
        // "z" does not exist anywhere.
        assert!(!graph.has_path("a", "z"));
    }

    #[test]
    fn predecessors_and_successors_ignore_missing_nodes() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("a", "t", 1.0));
        // Edge to a node that was never added.
        graph.add_edge(edge("a", "ghost", DependencyKind::Prerequisite));

        let succ = graph.successors("a");
        assert!(succ.is_empty());

        // Predecessors of "ghost" should find "a".
        let preds: Vec<&str> = graph
            .predecessors("ghost")
            .iter()
            .map(|n| n.key.as_str())
            .collect();
        assert_eq!(preds, vec!["a"]);
    }

    #[test]
    fn node_status_default_is_open() {
        assert_eq!(NodeStatus::default(), NodeStatus::Open);
    }

    #[test]
    fn graph_serializes_to_json() {
        let mut graph = OpportunityGraph::new();
        graph.add_node(node("scan_reddit", "reddit-scanner", 0.0));
        graph.add_edge(edge("scan_reddit", "engage", DependencyKind::Prerequisite));

        let json = serde_json::to_string(&graph).expect("serialize");
        assert!(json.contains("\"nodes\""));
        assert!(json.contains("\"edges\""));
        assert!(json.contains("scan_reddit"));
        assert!(json.contains("prerequisite"));
    }

    #[test]
    fn dependency_kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&DependencyKind::Prerequisite).unwrap(),
            "\"prerequisite\""
        );
        assert_eq!(
            serde_json::to_string(&DependencyKind::Enables).unwrap(),
            "\"enables\""
        );
        assert_eq!(
            serde_json::to_string(&DependencyKind::Reinforces).unwrap(),
            "\"reinforces\""
        );
    }

    #[test]
    fn node_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&NodeStatus::Open).unwrap(),
            "\"open\""
        );
        assert_eq!(
            serde_json::to_string(&NodeStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::to_string(&NodeStatus::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&NodeStatus::Failed).unwrap(),
            "\"failed\""
        );
    }
}
