//! A cached, graph-wide measure of how heavily a node is depended upon.
//!
//! graphrag-rs caches a PageRank pass over its entity graph so a query-time
//! walk can prefer globally important nodes without recomputing centrality
//! per query. The same idea applies to a workbook graph's dependency edges:
//! a column referenced by hundreds of formula groups is a more useful place
//! for [`crate`]'s consumers to land than one referenced once, all else
//! equal. [`compute_importance`] is the one place this is computed — once
//! per (re)index, in [`crate::store::Corpus::put`] — and the result is
//! stored alongside the graph so a query-time walk only ever looks it up.
//!
//! Only dependency edges count: [`crate::node::EdgeKind::Contains`] and
//! [`crate::node::EdgeKind::HeaderOf`] are structural nesting, not evidence
//! that one node relies on another, and folding them in would reward a node
//! for having many children rather than for being relied upon. This mirrors
//! `expand.rs`'s own split between the containment walk and the dependency
//! walk.
//!
//! `petgraph` itself ships `petgraph::algo::page_rank`, but it does not fit:
//! it has no notion of edge weight (every edge counts as one link, so the
//! "a heavier edge contributes more" case this module is tested against
//! could not be expressed), and its complexity is `O(n|V|²|E|)` — quadratic
//! in node count per iteration, against this module's `O(n|E|)` — because it
//! recomputes each target's inbound set from scratch every iteration rather
//! than accumulating rank along edges once. A fixed power iteration over
//! this crate's own weighted edges is a few dozen lines and fully auditable,
//! which also fits this project's offline, reproducible ethos better than a
//! dependency would.

use petgraph::visit::EdgeRef;

use crate::build::Graph;

/// The standard damping factor: the probability a "random surfer" follows an
/// edge rather than jumping to a uniformly random node.
const DAMPING: f64 = 0.85;

/// Iterations are capped, not just epsilon-gated: a pathological graph (a
/// long cycle with unusual weights) should still terminate in bounded time
/// during `eg index`, not stall it.
const MAX_ITERATIONS: usize = 100;

/// Stop once no node's rank moved by more than this between iterations.
const CONVERGENCE_EPSILON: f64 = 1e-9;

/// One importance score per node, indexed by [`petgraph::graph::NodeIndex::index`].
///
/// Standard PageRank normalisation: the returned values sum to (very nearly)
/// 1 across the whole graph, not rescaled to `[0, 1]` per node or to a mean
/// of 1. A caller comparing two scores only needs them on the same scale as
/// each other, which sum-to-one already guarantees.
pub fn compute_importance(graph: &Graph) -> Vec<f32> {
    let n = graph.node_count();
    if n == 0 {
        return Vec::new();
    }

    // Dependency edges only, with the weight already carried on `Edge`
    // (the number of real references a lifted edge stands for).
    let mut out_weight = vec![0.0_f64; n];
    let mut edges: Vec<(usize, usize, f64)> = Vec::new();
    for edge in graph.edge_references() {
        if edge.weight().kind.is_structural() {
            continue;
        }
        let source = edge.source().index();
        let target = edge.target().index();
        let weight = edge.weight().weight as f64;
        out_weight[source] += weight;
        edges.push((source, target, weight));
    }

    let base = (1.0 - DAMPING) / n as f64;
    let mut rank = vec![1.0 / n as f64; n];
    for _ in 0..MAX_ITERATIONS {
        let mut next = vec![base; n];
        // A node with no outgoing dependency edge is a dangling node: its
        // rank would otherwise simply vanish from the system rather than
        // redistributing, which understates everything else's importance.
        let dangling_mass: f64 = (0..n)
            .filter(|&i| out_weight[i] == 0.0)
            .map(|i| rank[i])
            .sum();
        for slot in &mut next {
            *slot += DAMPING * dangling_mass / n as f64;
        }
        for &(source, target, weight) in &edges {
            next[target] += DAMPING * rank[source] * (weight / out_weight[source]);
        }
        let delta: f64 = next.iter().zip(&rank).map(|(a, b)| (a - b).abs()).sum();
        rank = next;
        if delta < CONVERGENCE_EPSILON {
            break;
        }
    }
    rank.into_iter().map(|r| r as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{DefinedNameNode, Edge, EdgeKind, Node};
    use petgraph::graph::DiGraph;

    fn name_node(name: &str) -> Node {
        Node::DefinedName(DefinedNameNode {
            name: name.to_string(),
            refers_to: "A1".to_string(),
            scope: None,
        })
    }

    #[test]
    fn an_empty_graph_has_no_scores() {
        let graph: Graph = DiGraph::new();
        assert!(compute_importance(&graph).is_empty());
    }

    #[test]
    fn scores_sum_to_roughly_one() {
        let mut graph: Graph = DiGraph::new();
        let a = graph.add_node(name_node("a"));
        let b = graph.add_node(name_node("b"));
        let c = graph.add_node(name_node("c"));
        graph.add_edge(a, b, Edge::new(EdgeKind::DependsOn));
        graph.add_edge(b, c, Edge::new(EdgeKind::DependsOn));
        let scores = compute_importance(&graph);
        let total: f32 = scores.iter().sum();
        assert!((total - 1.0).abs() < 0.01, "{total}");
    }

    #[test]
    fn a_hub_referenced_by_many_outranks_a_leaf_referenced_by_none() {
        let mut graph: Graph = DiGraph::new();
        let hub = graph.add_node(name_node("hub"));
        let leaf = graph.add_node(name_node("leaf"));
        // Several independent nodes all depend on `hub`.
        for i in 0..5 {
            let n = graph.add_node(name_node(&format!("leaf{i}")));
            graph.add_edge(n, hub, Edge::new(EdgeKind::DependsOn));
        }
        let scores = compute_importance(&graph);
        assert!(
            scores[hub.index()] > scores[leaf.index()],
            "hub={} leaf={}",
            scores[hub.index()],
            scores[leaf.index()]
        );
    }

    #[test]
    fn structural_edges_are_not_evidence_of_dependency() {
        let mut graph: Graph = DiGraph::new();
        let parent = graph.add_node(name_node("parent"));
        let child = graph.add_node(name_node("child"));
        graph.add_edge(parent, child, Edge::new(EdgeKind::Contains));
        let scores = compute_importance(&graph);
        // With no dependency edges at all, every node is a dangling node and
        // the distribution stays uniform.
        assert!((scores[parent.index()] - scores[child.index()]).abs() < 1e-6);
    }

    #[test]
    fn a_heavier_edge_contributes_more_than_a_lighter_one() {
        let mut graph: Graph = DiGraph::new();
        let source = graph.add_node(name_node("source"));
        let heavy = graph.add_node(name_node("heavy"));
        let light = graph.add_node(name_node("light"));
        graph.add_edge(
            source,
            heavy,
            Edge {
                kind: EdgeKind::DependsOn,
                weight: 9,
            },
        );
        graph.add_edge(
            source,
            light,
            Edge {
                kind: EdgeKind::DependsOn,
                weight: 1,
            },
        );
        let scores = compute_importance(&graph);
        assert!(scores[heavy.index()] > scores[light.index()]);
    }
}
