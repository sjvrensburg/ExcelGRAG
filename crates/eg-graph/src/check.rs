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
//!   is [`crate::audit`]'s job — it re-derives every dependency edge from the
//!   cells and reports where the two disagree. It costs a pass over every
//!   formula in the workbook, which is why it is a separate call and not a
//!   sixth invariant here.
//! - **Whether the weights are right**, for the two dependency kinds that can
//!   fan out. `CROSS_WORKBOOK_REF` and `REFERENCES_NAME` are pinned exactly to
//!   the counts that produced them, so double- or under-counting either is
//!   caught — but only in total, summed over every edge of that kind in the
//!   graph. A name resolved to the *wrong* defined-name node still leaves the
//!   sum untouched if the edge it should have carried lands on some other edge
//!   of the same kind instead, so this catches a global miscount, not a
//!   misdirected individual edge; only [`crate::audit`], re-deriving each edge
//!   by its own endpoints, catches that (V1). A `DEPENDS_ON` or
//!   `CROSS_SHEET_REF` reference may legitimately land on several regions, so
//!   only a lower bound is checkable here; the ratio of summed weight to
//!   `references_scanned` is the number that would move.
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
    //
    // Three independent invariants share this one pass over the edges, so a
    // `break` the moment any one of them fired used to stop the other two
    // from ever being checked again — one edge with a zero weight silenced
    // every later edge's sheet-crossing violation, understating how broken
    // the graph actually was. Each invariant is instead reported once (its
    // first example) and the scan runs to completion regardless.
    let (mut zero_weight_reported, mut same_sheet_reported, mut cross_sheet_reported) =
        (false, false, false);
    for edge in graph.edge_indices() {
        let (a, b) = graph.edge_endpoints(edge).expect("edge has endpoints");
        let weight = graph[edge];
        let (sa, sb) = (graph[a].sheet(), graph[b].sheet());

        if weight.weight == 0 && !zero_weight_reported {
            zero_weight_reported = true;
            out.push(Violation {
                invariant: "every edge stands for at least one reference",
                detail: format!("{} edge of weight 0", weight.kind.as_str()),
            });
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
        if same_sheet_required && sa != sb && !same_sheet_reported {
            same_sheet_reported = true;
            out.push(Violation {
                invariant: "same-sheet edges stay on one sheet",
                detail: format!(
                    "{} from {:?} to {:?} crosses sheets",
                    weight.kind.as_str(),
                    graph[a].label(),
                    graph[b].label()
                ),
            });
        }
        if weight.kind == EdgeKind::CrossSheetRef && sa == sb && !cross_sheet_reported {
            cross_sheet_reported = true;
            out.push(Violation {
                invariant: "CROSS_SHEET_REF actually crosses a sheet",
                detail: format!("{:?} to {:?}", graph[a].label(), graph[b].label()),
            });
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
    //
    // On its own this is weak: the buckets are incremented on mutually
    // exclusive paths of one function, so it holds by construction and would
    // only ever break under an edit that added a sixth outcome. The checks
    // below are the ones with teeth, because they measure the report against
    // the *graph* — counted independently by `BuildReport::count_graph` — and
    // so a lifting bug that dropped or doubled an edge would move one side
    // without the other.
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

    // An external reference lifts to exactly one edge — the workbook it names —
    // and a resolved name to exactly one, the definition. Neither can be
    // dropped as a self-loop and neither can fan out, so the summed weight in
    // the graph must equal the count in the report, exactly.
    for (kind, counted, what) in [
        (
            EdgeKind::CrossWorkbookRef,
            r.references_external,
            "external references",
        ),
        (EdgeKind::ReferencesName, r.names_resolved, "resolved names"),
    ] {
        let weight = r.edge_weight_of(kind);
        if weight != counted {
            out.push(Violation {
                invariant: "lifted weight equals the references behind it",
                detail: format!(
                    "{counted} {what} but {} of {} weight",
                    weight,
                    kind.as_str()
                ),
            });
        }
    }

    // A cell reference can straddle regions, so these cannot be equalities: one
    // reference may carry several edges. It can never carry fewer than one,
    // which is what makes the bound a real constraint on lifting.
    let dependency =
        r.edge_weight_of(EdgeKind::DependsOn) + r.edge_weight_of(EdgeKind::CrossSheetRef);
    if dependency < r.references_lifted {
        out.push(Violation {
            invariant: "every lifted reference carries at least one edge",
            detail: format!(
                "{} lifted but only {dependency} of dependency weight",
                r.references_lifted
            ),
        });
    }
    if r.edge_weight_of(EdgeKind::CrossSheetRef) < r.references_cross_sheet {
        out.push(Violation {
            invariant: "every lifted reference carries at least one edge",
            detail: format!(
                "{} cross-sheet but only {} of CROSS_SHEET_REF weight",
                r.references_cross_sheet,
                r.edge_weight_of(EdgeKind::CrossSheetRef)
            ),
        });
    }

    out
}
