//! What a build found, and what it could not resolve.
//!
//! P3a exists to produce these numbers rather than to guess at them: the shape
//! of the graph decides whether it needs a store at all, and no estimate made
//! from node counts survived contact with a real workbook in earlier phases.
//!
//! Every counter here is exact. The only capped field is `dangling_examples`,
//! and it is labelled as examples for that reason.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::build::Graph;
use crate::node::{DanglingRef, EdgeKind, NodeKind};

/// Counts and timings for one build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildReport {
    /// Node totals, in [`NodeKind::ALL`] order.
    pub nodes: [u64; NodeKind::ALL.len()],
    /// Edge totals after merging, in [`EdgeKind::ALL`] order.
    pub edges: [u64; EdgeKind::ALL.len()],
    /// Summed weight per edge kind: the number of underlying references each
    /// kind stands for. The ratio against `edges` is what lifting bought.
    pub edge_weight: [u64; EdgeKind::ALL.len()],

    /// Cell references seen in formula text.
    pub references_scanned: u64,
    /// References that produced at least one edge.
    pub references_lifted: u64,
    /// References whose target region is the one the formula already sits in.
    /// Counted and then dropped: a self-loop on every region carries no
    /// information a traversal can use, and this is the bulk of a spreadsheet.
    pub references_within_source_region: u64,
    /// Lifted references landing on another sheet of this workbook.
    pub references_cross_sheet: u64,
    /// References into another workbook.
    pub references_external: u64,
    /// References to a sheet the workbook does not have — a `#REF!` break.
    pub references_dangling: u64,
    /// The missing sheet names, with how many references named each, most
    /// referenced first. Naming them is what makes the count actionable: one
    /// deleted sheet referenced from a filled-down column produces hundreds of
    /// thousands of breaks that are all the same break.
    pub unknown_sheets: Vec<(String, u64)>,
    /// References resolving to a sheet but to no region on it, because the
    /// cells they name are empty. Legal, and not damage.
    pub references_unpopulated_target: u64,

    /// Name tokens matched against a defined name.
    pub names_resolved: u64,
    /// Name-shaped tokens matching nothing the workbook defines. Overwhelmingly
    /// function names, so this is a vocabulary count, not a fault count.
    pub names_not_defined: u64,

    /// Formula cells found in no region. Should be zero: region detection
    /// covers every populated cell.
    pub formula_cells_outside_any_region: u64,
    /// Formula groups whose anchor fell in no region. Should be zero, for the
    /// same reason.
    pub formula_groups_outside_any_region: u64,
    /// Formula groups whose rectangle reaches past the region that owns their
    /// top-left cell — two regions abutting with no gap between them, most
    /// often. CONTAINS still wires the group to the one region alone (V2,
    /// `docs/audit-2026-08-31.md`), so retrieval walking in from the second
    /// region never finds it. Region detection does not emit abutting
    /// regions today, so this should be zero; if it ever is not, this number
    /// is what changed.
    pub formula_groups_spanning_regions: u64,

    /// Up to `GraphOptions::max_dangling_examples` worked examples. The counts
    /// above are exact; this is for reading, not for arithmetic.
    pub dangling_examples: Vec<DanglingRef>,

    pub build_time: Duration,
}

impl BuildReport {
    pub fn nodes_of(&self, kind: NodeKind) -> u64 {
        self.nodes[NodeKind::ALL.iter().position(|&k| k == kind).unwrap()]
    }

    pub fn edges_of(&self, kind: EdgeKind) -> u64 {
        self.edges[EdgeKind::ALL.iter().position(|&k| k == kind).unwrap()]
    }

    pub fn edge_weight_of(&self, kind: EdgeKind) -> u64 {
        self.edge_weight[EdgeKind::ALL.iter().position(|&k| k == kind).unwrap()]
    }

    pub fn total_nodes(&self) -> u64 {
        self.nodes.iter().sum()
    }

    pub fn total_edges(&self) -> u64 {
        self.edges.iter().sum()
    }

    /// References per edge across the dependency kinds — how much lifting
    /// compressed the dependency structure.
    pub fn lifting_ratio(&self) -> f64 {
        let dependency = [
            EdgeKind::DependsOn,
            EdgeKind::CrossSheetRef,
            EdgeKind::CrossWorkbookRef,
        ];
        let edges: u64 = dependency.iter().map(|&k| self.edges_of(k)).sum();
        let weight: u64 = dependency.iter().map(|&k| self.edge_weight_of(k)).sum();
        if edges == 0 {
            return 0.0;
        }
        weight as f64 / edges as f64
    }

    /// Fill the node and edge totals from a finished graph.
    pub(crate) fn count_graph(&mut self, graph: &Graph) {
        self.nodes = [0; NodeKind::ALL.len()];
        self.edges = [0; EdgeKind::ALL.len()];
        self.edge_weight = [0; EdgeKind::ALL.len()];
        for node in graph.node_weights() {
            let i = NodeKind::ALL
                .iter()
                .position(|&k| k == node.kind())
                .expect("every node kind is listed in NodeKind::ALL");
            self.nodes[i] += 1;
        }
        for edge in graph.edge_weights() {
            let i = EdgeKind::ALL
                .iter()
                .position(|&k| k == edge.kind)
                .expect("every edge kind is listed in EdgeKind::ALL");
            self.edges[i] += 1;
            self.edge_weight[i] += edge.weight;
        }
    }
}

/// How connected the graph is.
///
/// Degree distribution decides whether a bounded k-hop expansion (P5) is cheap
/// or explosive: one region referenced by everything makes a 2-hop walk from
/// anywhere reach the whole workbook, which is a retrieval problem long before
/// it is a storage problem.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DegreeStats {
    /// `(lower bound, node count)` for buckets 0, 1, 2–3, 4–7, 8–15, …
    pub buckets: Vec<(usize, u64)>,
    pub max_out: usize,
    pub max_in: usize,
    /// The highest-degree nodes, as `(label, in, out)`, most connected first.
    pub hubs: Vec<(String, usize, usize)>,
}

/// Compute the degree distribution and the top `hubs` nodes by total degree.
pub fn degree_stats(graph: &Graph, hubs: usize) -> DegreeStats {
    use petgraph::Direction;

    let mut buckets: Vec<u64> = Vec::new();
    let mut stats = DegreeStats::default();
    let mut ranked: Vec<(usize, usize, usize)> = Vec::with_capacity(graph.node_count());

    for node in graph.node_indices() {
        let out = graph.neighbors_directed(node, Direction::Outgoing).count();
        let inc = graph.neighbors_directed(node, Direction::Incoming).count();
        stats.max_out = stats.max_out.max(out);
        stats.max_in = stats.max_in.max(inc);

        let total = out + inc;
        // Bucket 0 is degree 0, bucket n>0 covers 2^(n-1)..2^n.
        let bucket = if total == 0 {
            0
        } else {
            usize::BITS as usize - total.leading_zeros() as usize
        };
        if buckets.len() <= bucket {
            buckets.resize(bucket + 1, 0);
        }
        buckets[bucket] += 1;
        ranked.push((total, node.index(), out));
    }

    stats.buckets = buckets
        .into_iter()
        .enumerate()
        .map(|(i, count)| (if i == 0 { 0 } else { 1 << (i - 1) }, count))
        .collect();

    // Ties broken by node index, so two runs over one workbook report the same
    // hubs. An unstable sort on degree alone left a field of equal-degree
    // nodes in whatever order the sort happened to leave them.
    ranked.sort_unstable_by_key(|&(total, index, _)| (std::cmp::Reverse(total), index));
    stats.hubs = ranked
        .into_iter()
        .take(hubs)
        .map(|(total, index, out)| {
            let node = &graph[petgraph::graph::NodeIndex::new(index)];
            (node.label(), total - out, out)
        })
        .collect();
    stats
}
