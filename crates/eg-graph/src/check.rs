//! Invariants the graph must satisfy, and a plain statement of what they miss.
//!
//! These are structural checks. They prove the graph is *self-consistent*: that
//! nothing is orphaned, that every node sits on one sheet, that every lifted
//! edge stands for at least one real reference. Passing them is necessary and
//! nowhere near sufficient.
//!
//! **What they do not cover**, stated because an earlier phase shipped a bug
//! that every invariant it had was structurally blind to:
//!
//! - **Whether an edge points at the right region.** A reference lifted to the
//!   wrong region still yields a reachable, single-sheet, positively weighted
//!   edge. Only comparing against what a reader would draw catches that, which
//!   is P8's job.
//! - **Whether the weights are right.** A lifting bug that double-counts every
//!   reference passes every check here; `references_scanned` against the summed
//!   edge weight in the report is the number that would move.
//! - **Anything about the cells themselves.** These checks read the graph, not
//!   the workbook. If ingest lost a formula, the graph is consistently missing
//!   its edges.

use petgraph::Direction;
use rustc_hash::FxHashSet;

use crate::build::{reachable_from, BuiltGraph};
use crate::node::{EdgeKind, Node, NodeKind};

/// A violated invariant, phrased as what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

/// Check every invariant, returning what failed. Empty means all held.
pub fn check(built: &BuiltGraph) -> Vec<Violation> {
    let mut out = Vec::new();
    let graph = &built.graph;

    // Exactly one root, and it is the one the build reported.
    let roots: Vec<_> = graph
        .node_indices()
        .filter(|&i| graph[i].kind() == NodeKind::Workbook)
        .collect();
    if roots.len() != 1 || roots[0] != built.root {
        out.push(Violation {
            invariant: "one workbook root",
            detail: format!("found {} workbook nodes", roots.len()),
        });
    }

    // Nothing is orphaned: an unreachable node can never be returned by a
    // traversal, so it is invisible however good retrieval gets.
    let reachable: FxHashSet<_> = reachable_from(graph, built.root);
    let orphans = graph.node_count() - reachable.len();
    if orphans > 0 {
        let example = graph
            .node_indices()
            .find(|i| !reachable.contains(i))
            .map(|i| format!("{} {:?}", graph[i].kind().as_str(), graph[i].label()))
            .unwrap_or_default();
        out.push(Violation {
            invariant: "every node reachable from the root",
            detail: format!("{orphans} unreachable, e.g. {example}"),
        });
    }

    // Every region, column and formula group has exactly one structural parent,
    // so "which sheet is this on?" has one answer.
    for node in graph.node_indices() {
        let kind = graph[node].kind();
        if !matches!(
            kind,
            NodeKind::Region | NodeKind::Column | NodeKind::FormulaGroup
        ) {
            continue;
        }
        let parents = graph
            .edges_directed(node, Direction::Incoming)
            .filter(|e| e.weight().kind == EdgeKind::Contains)
            .count();
        if parents != 1 {
            out.push(Violation {
                invariant: "one CONTAINS parent per contained node",
                detail: format!("{} {:?} has {parents}", kind.as_str(), graph[node].label()),
            });
            break;
        }
    }

    // A structural edge never leaves its sheet, and a same-sheet dependency
    // never crosses one. Both would make `CROSS_SHEET_REF` meaningless.
    for edge in graph.edge_indices() {
        let (a, b) = graph.edge_endpoints(edge).expect("edge has endpoints");
        let weight = graph[edge];
        let (sa, sb) = (graph[a].sheet(), graph[b].sheet());

        if weight.weight == 0 {
            out.push(Violation {
                invariant: "every edge stands for at least one reference",
                detail: format!("{} edge of weight 0", weight.kind.as_str()),
            });
            break;
        }

        let same_sheet_required = match weight.kind {
            EdgeKind::DependsOn => true,
            EdgeKind::HeaderOf => true,
            EdgeKind::Contains => matches!(
                (&graph[a], &graph[b]),
                (Node::Region(_), Node::Column(_)) | (Node::Region(_), Node::FormulaGroup(_))
            ),
            _ => false,
        };
        if same_sheet_required && sa != sb {
            out.push(Violation {
                invariant: "same-sheet edges stay on one sheet",
                detail: format!(
                    "{} from {:?} to {:?} crosses sheets",
                    weight.kind.as_str(),
                    graph[a].label(),
                    graph[b].label()
                ),
            });
            break;
        }
        if weight.kind == EdgeKind::CrossSheetRef && sa == sb {
            out.push(Violation {
                invariant: "CROSS_SHEET_REF actually crosses a sheet",
                detail: format!("{:?} to {:?}", graph[a].label(), graph[b].label()),
            });
            break;
        }
    }

    // Region detection covers every populated cell, so nothing derived from a
    // populated cell may have missed it.
    let r = &built.report;
    if r.formula_cells_outside_any_region > 0 || r.formula_groups_outside_any_region > 0 {
        out.push(Violation {
            invariant: "every formula cell falls in a region",
            detail: format!(
                "{} cells and {} groups outside any region",
                r.formula_cells_outside_any_region, r.formula_groups_outside_any_region
            ),
        });
    }

    // Every scanned reference is accounted for exactly once, so a reference
    // cannot be quietly dropped between scanning and lifting.
    let accounted = r.references_lifted
        + r.references_within_source_region
        + r.references_external
        + r.references_dangling
        + r.references_unpopulated_target;
    if accounted != r.references_scanned {
        out.push(Violation {
            invariant: "every reference is accounted for",
            detail: format!(
                "scanned {} but accounted for {accounted}",
                r.references_scanned
            ),
        });
    }

    out
}
