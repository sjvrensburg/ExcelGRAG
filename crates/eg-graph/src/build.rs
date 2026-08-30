//! Turning a loaded workbook into a graph.
//!
//! Two things happen here. The structural pass is mechanical: sheets, regions,
//! columns and formula groups become nodes wired by `CONTAINS` and `HEADER_OF`.
//!
//! The dependency pass is the interesting one. A formula's references are
//! between *cells*, but cell-level edges are exactly what we cannot afford —
//! 6.79 million formulas on the reference workbook, most of them naming two or
//! three cells each. So each reference is **lifted** to the region containing
//! it, and identical lifted edges merge, keeping their count as a weight. A
//! column of 100,000 formulas each reading the row above collapses to one
//! self-reference, counted and then dropped; a column reading a lookup table on
//! another sheet collapses to one `CROSS_SHEET_REF` of weight 100,000, which is
//! both small and a better answer than 100,000 edges would be, because the
//! weight says how much of the model rests on that table.
//!
//! Lifting is lossy by design. Recovering which *cell* fed which cell is P6's
//! job, done on demand against the workbook, and the node ranges make it cheap.

use std::time::Instant;

use eg_model::formula::{scan_names_into, scan_references_into};
use eg_model::{CellRef, NameSpan, RangeRef, ReferenceSpan, Sheet, SheetId, Workbook};
use eg_structure::{detect_regions_with, group_formulas, Region, RegionOptions};
use petgraph::graph::{DiGraph, NodeIndex};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::node::{
    ColumnNode, DanglingReason, DanglingRef, DefinedNameNode, Edge, EdgeKind, ExternalWorkbookNode,
    FormulaGroupNode, Node, NodeKind, RegionNode, SheetNode, WorkbookNode,
};
use crate::report::BuildReport;

/// The graph itself: a directed multigraph of aggregates.
pub type Graph = DiGraph<Node, Edge>;

/// A built graph, with the evidence for how it was built.
pub struct BuiltGraph {
    pub graph: Graph,
    /// The root. Every other node is reachable from here.
    pub root: NodeIndex,
    pub report: BuildReport,
}

/// How to build. The defaults are what the measurements in the plan used.
#[derive(Debug, Clone)]
pub struct GraphOptions {
    /// Passed through to region detection.
    pub regions: RegionOptions,
    /// Give every formula group its own node.
    ///
    /// On the reference workbook that is 464,131 nodes against 168 regions, so
    /// turning it off is the difference between a graph shaped like a workbook
    /// and one shaped like a spreadsheet. Dependency lifting is unaffected
    /// either way: it reads formula cells directly, not group nodes.
    pub formula_group_nodes: bool,
    /// Keep at most this many unresolved references as worked examples. The
    /// counts are exact regardless; only the examples are capped, because a
    /// workbook with one broken sheet reference tends to have millions of them.
    pub max_dangling_examples: usize,
}

impl Default for GraphOptions {
    fn default() -> Self {
        GraphOptions {
            regions: RegionOptions::default(),
            formula_group_nodes: true,
            max_dangling_examples: 32,
        }
    }
}

/// Build the graph for a workbook with the default options.
pub fn build(workbook: &Workbook) -> BuiltGraph {
    build_with(workbook, &GraphOptions::default())
}

/// Build the graph for a workbook.
pub fn build_with(workbook: &Workbook, opts: &GraphOptions) -> BuiltGraph {
    let started = Instant::now();
    let mut b = Builder::new(workbook, opts);
    b.add_root();
    b.add_defined_names();
    for sheet in &workbook.sheets {
        b.add_sheet(sheet);
    }
    b.lift_dependencies();
    b.flush_lifted_edges();
    b.report.count_graph(&b.graph);
    b.report.build_time = started.elapsed();
    BuiltGraph {
        graph: b.graph,
        root: b.root,
        report: b.report,
    }
}

/// A region on one sheet, and the node standing for it.
struct RegionEntry {
    range: RangeRef,
    node: NodeIndex,
    /// Column nodes, by the sheet column they cover.
    columns: FxHashMap<u16, NodeIndex>,
}

struct Builder<'a> {
    workbook: &'a Workbook,
    opts: &'a GraphOptions,
    graph: Graph,
    root: NodeIndex,
    /// Regions per sheet, indexed by [`SheetId`], which is the sheet's position
    /// in the workbook.
    regions: Vec<Vec<RegionEntry>>,
    sheet_nodes: Vec<NodeIndex>,
    /// Defined names by upper-cased name. A sheet-scoped name shadows a
    /// workbook-scoped one of the same name, so every scope is kept.
    names: FxHashMap<String, Vec<(Option<SheetId>, NodeIndex)>>,
    externals: FxHashMap<String, NodeIndex>,
    /// Sheet names that formulas referred to and the workbook does not have.
    unknown_sheets: FxHashMap<String, u64>,
    /// Lifted dependencies, accumulated before becoming edges so that repeats
    /// merge into a weight instead of a million parallel edges.
    lifted: FxHashMap<(NodeIndex, NodeIndex, EdgeKind), u64>,
    report: BuildReport,
}

impl<'a> Builder<'a> {
    fn new(workbook: &'a Workbook, opts: &'a GraphOptions) -> Self {
        Builder {
            workbook,
            opts,
            graph: DiGraph::new(),
            root: NodeIndex::end(),
            regions: Vec::with_capacity(workbook.sheets.len()),
            sheet_nodes: Vec::with_capacity(workbook.sheets.len()),
            names: FxHashMap::default(),
            externals: FxHashMap::default(),
            unknown_sheets: FxHashMap::default(),
            lifted: FxHashMap::default(),
            report: BuildReport::default(),
        }
    }

    fn add_root(&mut self) {
        self.root = self.graph.add_node(Node::Workbook(WorkbookNode {
            path: self.workbook.path.clone(),
            content_hash: self.workbook.content_hash.clone(),
            format: self.workbook.format.map(|f| f.as_str().to_string()),
        }));
    }

    fn add_defined_names(&mut self) {
        for name in &self.workbook.defined_names {
            let node = self.graph.add_node(Node::DefinedName(DefinedNameNode {
                name: name.name.clone(),
                refers_to: name.refers_to.clone(),
                scope: name.scope,
            }));
            self.graph
                .add_edge(self.root, node, Edge::new(EdgeKind::Contains));
            self.names
                .entry(name.name.to_uppercase())
                .or_default()
                .push((name.scope, node));
        }
    }

    fn add_sheet(&mut self, sheet: &Sheet) {
        let formula_cells = sheet.iter().filter(|(_, c)| c.formula.is_some()).count() as u64;
        let sheet_node = self.graph.add_node(Node::Sheet(SheetNode {
            id: sheet.id,
            name: sheet.name.clone(),
            visible: sheet.visibility.is_visible(),
            cells: sheet.len() as u64,
            formula_cells,
        }));
        self.graph
            .add_edge(self.root, sheet_node, Edge::new(EdgeKind::Contains));
        self.sheet_nodes.push(sheet_node);

        // `entries` stays a local until the groups are attached, so that
        // reading it and mutating the graph do not borrow `self` at once.
        let detected = detect_regions_with(sheet, &self.opts.regions);
        let mut entries = Vec::with_capacity(detected.len());
        for region in &detected {
            entries.push(self.add_region(sheet, sheet_node, region));
        }
        self.add_formula_groups(sheet, sheet_node, &entries);
        self.regions.push(entries);
    }

    fn add_region(&mut self, sheet: &Sheet, sheet_node: NodeIndex, region: &Region) -> RegionEntry {
        let node = self.graph.add_node(Node::Region(RegionNode {
            range: region.range,
            kind: region.kind,
            source: region.source,
            title: region.title.clone(),
            header_rows: region.header_rows,
            cell_count: region.cell_count,
        }));
        self.graph
            .add_edge(sheet_node, node, Edge::new(EdgeKind::Contains));

        // Headers run left to right from the first column that is not a row
        // label; the body is what lies below the title and header rows.
        let mut columns = FxHashMap::default();
        if let Some(body) = region.body() {
            let first = region.range.left + region.header_cols;
            for (offset, header) in region.headers.iter().enumerate() {
                // A blank header names nothing, so a node for it could not be
                // found by any search not already at the region. The column is
                // still covered, by the region itself.
                if header.is_empty() {
                    continue;
                }
                let Ok(offset) = u16::try_from(offset) else {
                    break;
                };
                let Some(col) = first.checked_add(offset) else {
                    break;
                };
                if col > region.range.right {
                    break;
                }
                let range = RangeRef::new(sheet.id, body.top, col, body.bottom, col);
                let column = self.graph.add_node(Node::Column(ColumnNode {
                    range,
                    header: header.clone(),
                }));
                self.graph
                    .add_edge(node, column, Edge::new(EdgeKind::Contains));
                columns.insert(col, column);
            }
        }

        RegionEntry {
            range: region.range,
            node,
            columns,
        }
    }

    fn add_formula_groups(
        &mut self,
        sheet: &Sheet,
        sheet_node: NodeIndex,
        entries: &[RegionEntry],
    ) {
        if !self.opts.formula_group_nodes {
            return;
        }
        let (groups, _) = group_formulas(sheet);
        let mut hint = 0usize;
        for group in groups {
            let range = group.range;
            let node = self.graph.add_node(Node::FormulaGroup(FormulaGroupNode {
                range,
                shape: group.shape,
                representative: group.representative,
                cell_count: group.cell_count,
            }));

            let parent = match find_region(entries, range.top_left(), &mut hint) {
                Some(entry) => {
                    // A group can straddle columns; each headed column that
                    // overlaps it heads it.
                    for col in range.left..=range.right {
                        if let Some(&column) = entry.columns.get(&col) {
                            self.graph
                                .add_edge(column, node, Edge::new(EdgeKind::HeaderOf));
                        }
                    }
                    entry.node
                }
                None => {
                    // Unreachable while region detection covers every populated
                    // cell, which `check_regions` asserts on each workbook we
                    // measure. Counted rather than asserted, so that a change to
                    // detection surfaces as a number and not a panic.
                    self.report.formula_groups_outside_any_region += 1;
                    sheet_node
                }
            };
            self.graph
                .add_edge(parent, node, Edge::new(EdgeKind::Contains));
        }
    }

    /// Walk every formula cell, lift its references, and accumulate weights.
    fn lift_dependencies(&mut self) {
        let mut refs: Vec<ReferenceSpan> = Vec::new();
        let mut names: Vec<NameSpan> = Vec::new();
        // Reused across every name token of every formula. `to_uppercase`
        // allocates, and there are several name-shaped tokens — mostly function
        // names — in each of millions of formulas.
        let mut upper = String::new();
        let resolve_names = !self.names.is_empty();

        // Moved out for the duration so that reading regions and writing edges
        // are not two borrows of `self` at the same time. Put back at the end.
        let regions = std::mem::take(&mut self.regions);

        // Keyed by position, which is how `add_sheet` filled both vectors.
        // `SheetId` is that position for every workbook ingest produces, but
        // indexing by it here would panic on one where it is not.
        for (index, sheet) in self.workbook.sheets.iter().enumerate() {
            let mut source_hint = 0usize;
            let mut target_hint = 0usize;
            for (at, cell) in sheet.iter() {
                let Some(formula) = cell.formula.as_deref() else {
                    continue;
                };
                let source = match regions
                    .get(index)
                    .and_then(|e| find_region(e, at, &mut source_hint))
                {
                    Some(entry) => entry.node,
                    None => {
                        self.report.formula_cells_outside_any_region += 1;
                        self.sheet_nodes[index]
                    }
                };

                scan_references_into(formula, &mut refs);
                for span in &refs {
                    self.report.references_scanned += 1;
                    self.lift_reference(
                        &regions,
                        sheet,
                        at,
                        source,
                        span,
                        formula,
                        &mut target_hint,
                    );
                }

                if resolve_names {
                    scan_names_into(formula, &mut names);
                    for span in &names {
                        self.lift_name(source, span, formula, &mut upper);
                    }
                }
            }
        }

        self.regions = regions;
    }

    #[allow(clippy::too_many_arguments)]
    fn lift_reference(
        &mut self,
        regions: &[Vec<RegionEntry>],
        sheet: &Sheet,
        at: CellRef,
        source: NodeIndex,
        span: &ReferenceSpan,
        formula: &str,
        hint: &mut usize,
    ) {
        // An external reference names a workbook we cannot open, so it is
        // lifted to that workbook and no further.
        if let Some(book) = &span.parsed.workbook {
            let target = self.external_node(book);
            *self
                .lifted
                .entry((source, target, EdgeKind::CrossWorkbookRef))
                .or_default() += 1;
            self.report.references_external += 1;
            return;
        }

        let target_sheet = match &span.parsed.sheet_name {
            None => sheet.id,
            Some(name) => match self.workbook.sheet_id_by_name(name) {
                Some(id) => id,
                None => {
                    self.report.references_dangling += 1;
                    // Cloned only the first time the name is seen: one deleted
                    // sheet accounts for millions of these.
                    match self.unknown_sheets.get_mut(name) {
                        Some(count) => *count += 1,
                        None => {
                            self.unknown_sheets.insert(name.clone(), 1);
                        }
                    }
                    self.record_dangling(|| DanglingRef {
                        from: at,
                        text: span.text(formula).to_string(),
                        reason: DanglingReason::UnknownSheet(name.clone()),
                    });
                    return;
                }
            },
        };

        let range = span.parsed.resolve(target_sheet);
        let cross = target_sheet != sheet.id;
        let kind = if cross {
            EdgeKind::CrossSheetRef
        } else {
            EdgeKind::DependsOn
        };

        // A range can straddle regions, and every region it touches is a real
        // dependency, so every one gets an edge. The hint makes the common case
        // — the same target as the previous formula — a single range test.
        let entries: &[RegionEntry] = regions.get(target_sheet.0 as usize).map_or(&[], |e| e);
        let mut hit = false;
        let mut edges = 0u32;
        let start = if *hint < entries.len() { *hint } else { 0 };
        for k in 0..entries.len() {
            let i = (start + k) % entries.len();
            let entry = &entries[i];
            if !entry.range.intersects(&range) {
                continue;
            }
            if !hit {
                *hint = i;
                hit = true;
            }
            // A region depending on itself is the ordinary case — a filled
            // column reads the row above — and a self-loop on every region
            // tells a traversal nothing it can use. Counted, then dropped.
            if entry.node == source {
                continue;
            }
            *self.lifted.entry((source, entry.node, kind)).or_default() += 1;
            edges += 1;
        }

        // Each reference lands in exactly one of these, which is what makes
        // the totals checkable against `references_scanned`.
        if edges > 0 {
            self.report.references_lifted += 1;
            if cross {
                self.report.references_cross_sheet += 1;
            }
        } else if hit {
            self.report.references_within_source_region += 1;
        } else {
            // Legal and common: a formula reading cells that hold nothing.
            self.report.references_unpopulated_target += 1;
            // Cited with its sheet: an unqualified `D21` in the report reads as
            // a reference to the source sheet, which is exactly wrong when the
            // formula wrote `Other!$D$21`. Built lazily — formatting a citation
            // per reference, for millions of references whose examples are
            // long since capped, is pure waste.
            let workbook = self.workbook;
            self.record_dangling(|| DanglingRef {
                from: at,
                text: workbook.cite_range(range),
                reason: DanglingReason::UnpopulatedTarget,
            });
        }
    }

    fn external_node(&mut self, book: &str) -> NodeIndex {
        if let Some(&node) = self.externals.get(book) {
            return node;
        }
        let node = self
            .graph
            .add_node(Node::ExternalWorkbook(ExternalWorkbookNode {
                token: book.to_string(),
            }));
        self.graph
            .add_edge(self.root, node, Edge::new(EdgeKind::Contains));
        self.externals.insert(book.to_string(), node);
        node
    }

    fn lift_name(&mut self, source: NodeIndex, span: &NameSpan, formula: &str, upper: &mut String) {
        // `Rates!Tax_Rate` names the definition scoped to `Rates`, wherever the
        // formula lives. Only an unqualified name is resolved against the
        // formula's own sheet.
        let scope = match &span.sheet_name {
            Some(name) => match self.workbook.sheet_id_by_name(name) {
                Some(id) => Some(id),
                None => {
                    // Qualified by a sheet the workbook does not have, so no
                    // definition it could name exists.
                    self.report.names_not_defined += 1;
                    return;
                }
            },
            None => self.graph[source].sheet(),
        };

        upper.clear();
        upper.extend(span.text(formula).chars().flat_map(char::to_uppercase));
        // A sheet-scoped name shadows a workbook-scoped one, so prefer the
        // scope the reference asks for and fall back to global, which is
        // visible from every sheet.
        let target = self.names.get(upper.as_str()).and_then(|candidates| {
            candidates
                .iter()
                .find(|(s, _)| *s == scope)
                .or_else(|| candidates.iter().find(|(s, _)| s.is_none()))
                .map(|&(_, node)| node)
        });
        let Some(target) = target else {
            // Almost certainly a function this scanner does not know. Names
            // that resolve are the signal; the rest is vocabulary.
            self.report.names_not_defined += 1;
            return;
        };
        *self
            .lifted
            .entry((source, target, EdgeKind::ReferencesName))
            .or_default() += 1;
        self.report.names_resolved += 1;
    }

    /// Keep an unresolved reference as a worked example, if there is room.
    ///
    /// The example is built by the closure only when it will be kept. The
    /// counts are exact either way, and a workbook with one broken sheet
    /// reference has millions of these to decline.
    fn record_dangling(&mut self, make: impl FnOnce() -> DanglingRef) {
        if self.report.dangling_examples.len() < self.opts.max_dangling_examples {
            let example = make();
            self.report.dangling_examples.push(example);
        }
    }

    fn flush_lifted_edges(&mut self) {
        // Sorted so that two runs over the same workbook produce the same
        // graph. A hash map's order is not stable, and a graph that differs run
        // to run cannot be diffed when a number moves.
        let mut unknown: Vec<(String, u64)> = self.unknown_sheets.drain().collect();
        unknown.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        self.report.unknown_sheets = unknown;

        let mut edges: Vec<_> = self.lifted.drain().collect();
        edges.sort_unstable_by_key(|&((a, b, kind), _)| (a.index(), b.index(), kind));
        for ((source, target, kind), weight) in edges {
            self.graph.add_edge(source, target, Edge { kind, weight });
        }
    }
}

/// Find the region containing `cell`, trying `hint` first and updating it.
///
/// Formula cells arrive in row-major order and neighbouring cells nearly always
/// share a region, so the first probe usually hits. The scan behind it is
/// linear, which is fine at the few dozen regions a sheet has.
fn find_region<'e>(
    entries: &'e [RegionEntry],
    cell: CellRef,
    hint: &mut usize,
) -> Option<&'e RegionEntry> {
    if entries.is_empty() {
        return None;
    }
    let start = if *hint < entries.len() { *hint } else { 0 };
    for k in 0..entries.len() {
        let i = (start + k) % entries.len();
        if entries[i].range.contains(cell) {
            *hint = i;
            return Some(&entries[i]);
        }
    }
    None
}

/// Nodes of one kind, for callers walking a single layer.
pub fn nodes_of_kind(graph: &Graph, kind: NodeKind) -> impl Iterator<Item = NodeIndex> + '_ {
    graph
        .node_indices()
        .filter(move |&i| graph[i].kind() == kind)
}

/// Everything reachable from `root` by following edges forwards.
pub fn reachable_from(graph: &Graph, root: NodeIndex) -> FxHashSet<NodeIndex> {
    let mut seen = FxHashSet::default();
    let mut stack = vec![root];
    seen.insert(root);
    while let Some(n) = stack.pop() {
        for next in graph.neighbors(n) {
            if seen.insert(next) {
                stack.push(next);
            }
        }
    }
    seen
}
