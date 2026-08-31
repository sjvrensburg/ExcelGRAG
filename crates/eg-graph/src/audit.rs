//! Checking the lifted edges against the cells they were lifted from.
//!
//! [`crate::check`] proves the graph is self-consistent and says plainly what
//! that misses: an edge lifted to the *wrong* region is still reachable, still
//! on one sheet, still positively weighted, and every invariant there passes.
//! Catching that needs the workbook — a second reader drawing the dependency
//! edges from the formulas and comparing them with the ones the graph holds.
//!
//! That is what this does, exhaustively rather than by sample. Every formula in
//! the workbook is read, every reference resolved, and every reference attached
//! to the regions it lands in — reading those regions out of the **graph's own
//! nodes**, not out of the builder's internals. The result is the multiset of
//! dependency edges the workbook demands, compared with the multiset the graph
//! has, both ways round:
//!
//! - an expected edge the graph lacks means a reference lost its edge;
//! - an edge nothing expects means an edge points where no formula does, which
//!   is the failure `check` is blind to, seen from the other side;
//! - a weight that disagrees means the edge is right and the evidence behind it
//!   is not, which decides rankings in retrieval.
//!
//! **What it shares with the thing it checks**, stated because an audit whose
//! independence is overstated is worse than none: reference scanning
//! ([`eg_model::formula::scan_references_into`]), range geometry
//! ([`eg_model::RangeRef`]) and region detection are each one implementation,
//! used by both sides. A defect in any of them is invisible here — parity
//! against a second reader and the recompute sweep are what cover those. What
//! is genuinely re-derived is the lifting itself: which region a formula
//! belongs to, which regions its references land in, whether a self-reference
//! is dropped, and how the counts accumulate. Those are the steps that chose a
//! region, and choosing the wrong one was the bug that shipped.
//!
//! It audits the two dependency kinds whose target is chosen by *geometry* —
//! `DEPENDS_ON` and `CROSS_SHEET_REF`. `CROSS_WORKBOOK_REF` and
//! `REFERENCES_NAME` resolve by name rather than by rectangle, and `check`
//! already pins both to the exact reference counts that produced them.
//!
//! Because it reads regions and edges out of a [`Graph`], it audits a graph
//! read back from a [`crate::store::Corpus`] just as well as a freshly built
//! one — which puts the store's round-trip under the same check.

use std::time::{Duration, Instant};

use eg_model::formula::scan_references_into;
use eg_model::{CellRef, RangeRef, ReferenceSpan, SheetId, Workbook};
use petgraph::graph::NodeIndex;
use rustc_hash::FxHashMap;

use crate::build::Graph;
use crate::node::{EdgeKind, Node};

/// How to audit.
#[derive(Debug, Clone)]
pub struct AuditOptions {
    /// Keep at most this many findings as worked examples. The counts are exact
    /// regardless; one mis-lifted region can account for millions of findings.
    pub max_findings: usize,
}

impl Default for AuditOptions {
    fn default() -> Self {
        AuditOptions { max_findings: 32 }
    }
}

/// What kind of disagreement was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// The workbook has references the graph has no edge for.
    MissingEdge,
    /// The graph has an edge no reference in the workbook accounts for.
    UnaccountedEdge,
    /// The edge is there and points where it should; its weight is not the
    /// number of references behind it.
    WeightDisagrees,
    /// The same source, target and kind appear on more than one edge, so the
    /// evidence for one dependency is split across parallel edges.
    ParallelEdges,
    /// More than one region contains a formula cell, so which region owns it —
    /// and therefore where its edges start — depends on which was tried first.
    AmbiguousContainment,
}

impl FindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingKind::MissingEdge => "missing edge",
            FindingKind::UnaccountedEdge => "unaccounted edge",
            FindingKind::WeightDisagrees => "weight disagrees",
            FindingKind::ParallelEdges => "parallel edges",
            FindingKind::AmbiguousContainment => "ambiguous containment",
        }
    }
}

/// One disagreement between the graph and the workbook, phrased as what differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: FindingKind,
    pub detail: String,
}

/// What the audit read, what it expected, and where the two parted.
#[derive(Debug, Clone, Default)]
pub struct AuditReport {
    pub formulas_read: u64,
    pub references_read: u64,
    /// References that resolved to a sheet of this workbook and landed on at
    /// least one region. Not the same as [`crate::BuildReport::references_lifted`],
    /// which counts only those that went on to *make* an edge: a reference
    /// landing solely in its own region is counted here and dropped there,
    /// because a region depending on itself tells a traversal nothing. The
    /// remainder — external, dangling, or landing where the sheet holds no
    /// region — is not this audit's business either way.
    pub references_landed: u64,
    /// Distinct `(source, target, kind)` triples the workbook demands.
    pub edges_expected: u64,
    /// Distinct triples the graph holds, over the audited kinds.
    pub edges_in_graph: u64,
    /// Triples present on both sides with equal weight.
    pub edges_agreed: u64,
    pub weight_expected: u64,
    pub weight_in_graph: u64,
    /// Every finding, counted exactly.
    pub findings_total: u64,
    /// Findings kept as worked examples, capped by [`AuditOptions`].
    pub findings: Vec<Finding>,
    pub audit_time: Duration,
}

impl AuditReport {
    /// Whether the graph and the workbook agree on every audited edge.
    pub fn agrees(&self) -> bool {
        self.findings_total == 0
    }

    /// The share of expected edges the graph reproduced exactly, as a fraction.
    ///
    /// `1.0` when there was nothing to expect, because a workbook with no
    /// dependencies is reproduced perfectly by a graph with no edges.
    pub fn agreement(&self) -> f64 {
        if self.edges_expected == 0 {
            return 1.0;
        }
        self.edges_agreed as f64 / self.edges_expected as f64
    }
}

/// Audit a graph's lifted dependency edges against the workbook it came from.
pub fn audit(workbook: &Workbook, graph: &Graph, opts: &AuditOptions) -> AuditReport {
    let started = Instant::now();
    let mut report = AuditReport::default();

    let index = GraphIndex::of(graph);
    let mut expected: FxHashMap<Key, Expectation> = FxHashMap::default();
    let mut spans: Vec<ReferenceSpan> = Vec::new();

    for sheet in &workbook.sheets {
        for (at, cell) in sheet.iter() {
            let Some(formula) = cell.formula.as_deref() else {
                continue;
            };
            report.formulas_read += 1;

            let (source, ambiguous) = index.owner_of(at);
            if ambiguous {
                report.findings_total += 1;
                keep(
                    &mut report,
                    opts,
                    FindingKind::AmbiguousContainment,
                    format!(
                        "{} lies in {} regions at once",
                        workbook.cite(at),
                        index.containing(at)
                    ),
                );
            }
            let Some(source) = source else {
                // No node stands for this cell at all. The graph is built from
                // a different workbook, or from one without this sheet.
                continue;
            };

            scan_references_into(formula, &mut spans);
            for span in &spans {
                report.references_read += 1;
                let Some(range) = resolve(workbook, sheet.id, span) else {
                    continue;
                };
                let kind = if range.sheet == sheet.id {
                    EdgeKind::DependsOn
                } else {
                    EdgeKind::CrossSheetRef
                };

                let mut landed = false;
                for &(region, node) in index.regions_on(range.sheet) {
                    if !region.intersects(&range) {
                        continue;
                    }
                    landed = true;
                    // A region depending on itself is the ordinary case and
                    // tells a traversal nothing, so the build drops it and so
                    // does the expectation.
                    if node == source {
                        continue;
                    }
                    let entry = expected.entry((source, node, kind)).or_default();
                    entry.weight += 1;
                    if entry.example.is_none() {
                        entry.example = Some((at, span.text(formula).to_string()));
                    }
                }
                if landed {
                    report.references_landed += 1;
                }
            }
        }
    }

    let actual = index.dependency_edges(&mut report, opts);
    report.edges_expected = expected.len() as u64;
    report.edges_in_graph = actual.len() as u64;

    // Sorted so that two audits of the same pair report the same findings in
    // the same order. Findings are capped, and a cap over a hash map's order
    // keeps a different handful each run.
    let mut keys: Vec<&Key> = expected.keys().chain(actual.keys()).collect();
    keys.sort_unstable_by_key(|&&(a, b, kind)| (a.index(), b.index(), kind));
    keys.dedup();

    for key in keys {
        let want = expected.get(key);
        let have = actual.get(key);
        report.weight_expected += want.map_or(0, |e| e.weight);
        report.weight_in_graph += have.map_or(0, |e| e.weight);
        match (want, have) {
            (Some(want), Some(have)) if want.weight == have.weight => report.edges_agreed += 1,
            (Some(want), Some(have)) => {
                report.findings_total += 1;
                keep(
                    &mut report,
                    opts,
                    FindingKind::WeightDisagrees,
                    format!(
                        "{} stands for {} references, workbook has {} ({})",
                        edge_label(graph, workbook, key),
                        have.weight,
                        want.weight,
                        cite_example(workbook, want),
                    ),
                );
            }
            (Some(want), None) => {
                report.findings_total += 1;
                keep(
                    &mut report,
                    opts,
                    FindingKind::MissingEdge,
                    format!(
                        "{} is missing, {} references expect it ({})",
                        edge_label(graph, workbook, key),
                        want.weight,
                        cite_example(workbook, want),
                    ),
                );
            }
            (None, Some(have)) => {
                report.findings_total += 1;
                keep(
                    &mut report,
                    opts,
                    FindingKind::UnaccountedEdge,
                    format!(
                        "{} stands for {} references, workbook has none",
                        edge_label(graph, workbook, key),
                        have.weight,
                    ),
                );
            }
            (None, None) => unreachable!("key came from one of the two maps"),
        }
    }

    report.audit_time = started.elapsed();
    report
}

type Key = (NodeIndex, NodeIndex, EdgeKind);

/// An expected edge, and the first reference that asked for it.
#[derive(Debug, Default)]
struct Expectation {
    weight: u64,
    example: Option<(CellRef, String)>,
}

fn keep(report: &mut AuditReport, opts: &AuditOptions, kind: FindingKind, detail: String) {
    if report.findings.len() < opts.max_findings {
        report.findings.push(Finding { kind, detail });
    }
}

/// Resolve a scanned reference to the cells of this workbook it names.
///
/// `None` for a reference into another workbook or onto a sheet this one does
/// not have. Both are real findings about the workbook and neither is a finding
/// about lifting: they become their own node kinds, whose weights `check`
/// already pins exactly.
fn resolve(workbook: &Workbook, from: SheetId, span: &ReferenceSpan) -> Option<RangeRef> {
    if span.parsed.workbook.is_some() {
        return None;
    }
    let sheet = match &span.parsed.sheet_name {
        None => from,
        Some(name) => workbook.sheet_id_by_name(name)?,
    };
    Some(span.parsed.resolve(sheet))
}

fn edge_label(graph: &Graph, workbook: &Workbook, &(a, b, kind): &Key) -> String {
    format!(
        "{} {} → {}",
        kind.as_str(),
        node_label(graph, workbook, a),
        node_label(graph, workbook, b)
    )
}

fn node_label(graph: &Graph, workbook: &Workbook, node: NodeIndex) -> String {
    let n = &graph[node];
    match n.range() {
        Some(range) => format!("{:?} at {}", n.label(), workbook.cite_range(range)),
        None => format!("{:?}", n.label()),
    }
}

fn cite_example(workbook: &Workbook, want: &Expectation) -> String {
    match &want.example {
        Some((at, text)) => format!("e.g. {} writes {text}", workbook.cite(*at)),
        None => "no example".to_string(),
    }
}

/// The geometry of the graph, read out of its nodes.
struct GraphIndex {
    /// Region ranges and their nodes, per sheet. Not indexed by [`SheetId`] as
    /// a position: a graph read back from a store need not hold every sheet.
    regions: FxHashMap<SheetId, Vec<(RangeRef, NodeIndex)>>,
    sheets: FxHashMap<SheetId, NodeIndex>,
    dependency: Vec<(Key, u64)>,
}

impl GraphIndex {
    fn of(graph: &Graph) -> GraphIndex {
        let mut regions: FxHashMap<SheetId, Vec<(RangeRef, NodeIndex)>> = FxHashMap::default();
        let mut sheets = FxHashMap::default();
        for node in graph.node_indices() {
            match &graph[node] {
                Node::Region(r) => regions
                    .entry(r.range.sheet)
                    .or_default()
                    .push((r.range, node)),
                Node::Sheet(s) => {
                    sheets.insert(s.id, node);
                }
                _ => {}
            }
        }
        // Sorted so that "the first region containing this cell" is a property
        // of the graph and not of node insertion order.
        for list in regions.values_mut() {
            list.sort_unstable_by_key(|&(r, node)| {
                (r.top, r.left, r.bottom, r.right, node.index())
            });
        }

        let mut dependency = Vec::new();
        for edge in graph.edge_indices() {
            let w = graph[edge];
            if !matches!(w.kind, EdgeKind::DependsOn | EdgeKind::CrossSheetRef) {
                continue;
            }
            let (a, b) = graph.edge_endpoints(edge).expect("edge has endpoints");
            dependency.push(((a, b, w.kind), w.weight));
        }

        GraphIndex {
            regions,
            sheets,
            dependency,
        }
    }

    fn regions_on(&self, sheet: SheetId) -> &[(RangeRef, NodeIndex)] {
        self.regions.get(&sheet).map_or(&[], |v| v.as_slice())
    }

    /// Which node a formula cell's edges start from, and whether that was a
    /// choice between regions rather than a fact.
    ///
    /// The build falls back to the sheet node for a cell no region covers, and
    /// counts it; this mirrors that so the two sides can be compared at all.
    fn owner_of(&self, at: CellRef) -> (Option<NodeIndex>, bool) {
        let mut found = None;
        let mut count = 0u32;
        for &(range, node) in self.regions_on(at.sheet) {
            if range.contains(at) {
                count += 1;
                if found.is_none() {
                    found = Some(node);
                }
            }
        }
        match found {
            Some(node) => (Some(node), count > 1),
            None => (self.sheets.get(&at.sheet).copied(), false),
        }
    }

    fn containing(&self, at: CellRef) -> usize {
        self.regions_on(at.sheet)
            .iter()
            .filter(|(range, _)| range.contains(at))
            .count()
    }

    /// The audited edges, summed per triple, flagging any triple carried by
    /// more than one edge.
    fn dependency_edges(
        &self,
        report: &mut AuditReport,
        opts: &AuditOptions,
    ) -> FxHashMap<Key, Expectation> {
        let mut out: FxHashMap<Key, Expectation> = FxHashMap::default();
        let mut seen: FxHashMap<Key, u32> = FxHashMap::default();
        for &(key, weight) in &self.dependency {
            out.entry(key).or_default().weight += weight;
            let count = seen.entry(key).or_default();
            *count += 1;
            if *count == 2 {
                report.findings_total += 1;
                keep(
                    report,
                    opts,
                    FindingKind::ParallelEdges,
                    format!(
                        "{} appears on more than one edge",
                        // Labelled by index: this runs before the workbook-side
                        // labels are available, and a duplicated triple is
                        // identified by its endpoints, not by its text.
                        format_args!("{} {:?}→{:?}", key.2.as_str(), key.0, key.1)
                    ),
                );
            }
        }
        out
    }
}
