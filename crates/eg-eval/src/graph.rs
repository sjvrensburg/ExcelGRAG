//! A bounded formula-dependency subgraph, as nodes and edges.
//!
//! `precedents_of`/`dependents_of` in [`crate::trace`] answer one hop from one
//! cell or range. An agent building a dependency visualization wants several
//! hops from a citation, already deduplicated into a graph — which is exactly
//! the walk `eg trace` does once, done N times and assembled. This module is
//! that walk, not a new kind of analysis: every edge here is a `Reference`
//! `trace` could already print, just kept and linked instead of printed and
//! discarded.
//!
//! Two things bound it, because both directions can grow without limit
//! otherwise:
//!
//! - `max_nodes` caps the walk outright. Dependents costs a full workbook scan
//!   *per level* — unlike precedents, which reads one cell's own text — so an
//!   unbounded depth is not safe to offer.
//! - A reference that resolves to more than one cell (`SUM(A1:A100)`, a
//!   `VLOOKUP` table) becomes a single terminal node rather than being
//!   expanded cell by cell. A lookup table is where a chain of reasoning ends,
//!   not a fork into hundreds of cells that happen to sit in its rows.

use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;

use eg_model::formula::scan_references_into;
use eg_model::{CellRef, CellValue, RangeRef, ReferenceSpan, Workbook};

use crate::trace::{cell as cell_fact, cells_in, overlaps, resolve, sheet_ids, Reference, Target};

/// A cell's value, as JSON — `null` for an empty cell, the value itself
/// otherwise, or its kind under redaction, the same trade `eg`'s
/// `--redact-values` makes for text output.
///
/// Shared by the CLI's `graph` verb and the MCP `graph` tool, which both
/// render a [`GraphExport`] to JSON and previously kept their own identical
/// copy of this.
pub fn value_json(value: &CellValue, redact: bool) -> serde_json::Value {
    if redact {
        return serde_json::Value::String(format!("<{}>", value.kind().as_str()));
    }
    match value {
        CellValue::Empty => serde_json::Value::Null,
        CellValue::Number(n) => serde_json::json!(n),
        CellValue::Text(text) => serde_json::Value::String(text.clone()),
        CellValue::Bool(b) => serde_json::Value::Bool(*b),
        CellValue::Error(e) => serde_json::Value::String(e.to_string()),
    }
}

/// Which way to walk from the seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// What the seed's formulas read.
    Precedents,
    /// What reads the seed. Expensive — see [`crate::trace::dependents_of`].
    Dependents,
    Both,
}

/// What a node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A cell holding a formula.
    Formula,
    /// A populated cell holding a literal.
    Input,
    /// A cell the walk reached by reference but which holds nothing.
    Empty,
    /// More than one cell, reached as a single reference (`A1:A100`, a 3-D
    /// span) and kept as one node rather than expanded — see the module docs.
    Range,
    /// A `#REF!` — a sheet the workbook does not have.
    UnknownSheet,
    /// A reference into another workbook, not resolved.
    ExternalWorkbook,
}

/// One cell, range, or dangling reference reached by the walk.
#[derive(Debug, Clone, Serialize)]
pub struct GraphNode {
    /// Unique within the export, and the same string `eg cells`/`eg trace`
    /// would print for the same address — a caller can hand it straight back
    /// to another verb.
    pub id: String,
    pub kind: NodeKind,
    /// The formula as written, for a [`NodeKind::Formula`] node.
    pub formula: Option<String>,
    /// The cell's value, for a node that stands for exactly one cell.
    ///
    /// This is the workbook's data, the same as everywhere else in this crate
    /// — a caller rendering for anyone but the workbook's owner should redact
    /// it, the way `eg`'s `--redact-values` does.
    pub value: Option<CellValue>,
    /// Hops from the seed. 0 for a seed node itself.
    pub depth: usize,
}

/// What kind of reference an edge is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// `from` reads `to`.
    Precedent,
    /// `from` reads `to` — recorded in the other direction because the walk
    /// found it by asking who reads `to`, not by reading `from`'s text.
    Dependent,
}

/// One reference, kept rather than printed.
#[derive(Debug, Clone, Serialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    /// The reference exactly as written, e.g. `'Q3 Sales'!$B$2:$B$99`.
    pub text: String,
    pub kind: EdgeKind,
}

/// What the walk did, so a caller can tell a small graph from a capped one.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphReport {
    /// Formula cells scanned while looking for dependents. Zero unless
    /// [`Direction::Dependents`] or [`Direction::Both`] was asked for.
    pub formulas_scanned: u64,
    /// Whether `max_nodes` cut the walk short.
    pub capped: bool,
}

pub struct GraphExport {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub report: GraphReport,
}

pub struct GraphOptions {
    /// Hops from the seed. 0 exports only the seed's own populated cells.
    pub depth: usize,
    pub direction: Direction,
    /// Ceiling on the total number of nodes. Reached, the walk stops adding
    /// new nodes and edges rather than growing further.
    pub max_nodes: usize,
    /// Per-level cap on how many dependents a single scan returns — the same
    /// role `limit` plays for `dependents_of`.
    pub dependents_limit: usize,
}

impl Default for GraphOptions {
    fn default() -> Self {
        GraphOptions {
            depth: 2,
            direction: Direction::Both,
            max_nodes: 200,
            dependents_limit: 200,
        }
    }
}

struct Builder<'a> {
    workbook: &'a Workbook,
    nodes: Vec<GraphNode>,
    index: FxHashMap<String, usize>,
    edges: Vec<GraphEdge>,
    edge_seen: FxHashSet<(String, String, EdgeKind)>,
    max_nodes: usize,
    capped: bool,
    /// Cells already placed on a dependents frontier, at this level or an
    /// earlier one. A circular formula reference (`A1=B1+1`, `B1=A1+1`,
    /// legal under iterative calculation) would otherwise put the same cell
    /// back on the frontier every level: dependents costs a full workbook
    /// scan per level, so re-scanning a cell whose dependents were already
    /// found would cost that scan again for nothing new. Checked and
    /// populated only against cells that have actually served as a
    /// dependents-scan target, which is a stronger requirement than "already
    /// has a node" — a cell can get a node from the precedents walk without
    /// ever having been scanned for its own dependents.
    dep_scanned: FxHashSet<CellRef>,
}

impl<'a> Builder<'a> {
    fn new(workbook: &'a Workbook, max_nodes: usize) -> Self {
        Builder {
            workbook,
            nodes: Vec::new(),
            index: FxHashMap::default(),
            edges: Vec::new(),
            edge_seen: FxHashSet::default(),
            max_nodes,
            capped: false,
            dep_scanned: FxHashSet::default(),
        }
    }

    fn has_room(&mut self) -> bool {
        if self.nodes.len() >= self.max_nodes {
            self.capped = true;
        }
        !self.capped
    }

    /// A node for one cell. Idempotent — a cell reached twice keeps the depth
    /// it was first found at, which under a breadth-first walk is the
    /// smallest.
    fn cell_node(&mut self, at: CellRef, depth: usize) -> Option<String> {
        let id = self.workbook.cite(at);
        if self.index.contains_key(&id) {
            return Some(id);
        }
        if !self.has_room() {
            return None;
        }
        let node = match cell_fact(self.workbook, at) {
            Some(fact) if fact.formula.is_some() => GraphNode {
                id: id.clone(),
                kind: NodeKind::Formula,
                formula: fact.formula,
                value: Some(fact.value),
                depth,
            },
            Some(fact) => GraphNode {
                id: id.clone(),
                kind: NodeKind::Input,
                formula: None,
                value: Some(fact.value),
                depth,
            },
            None => GraphNode {
                id: id.clone(),
                kind: NodeKind::Empty,
                formula: None,
                value: None,
                depth,
            },
        };
        self.index.insert(id.clone(), self.nodes.len());
        self.nodes.push(node);
        Some(id)
    }

    /// A node for a whole range, kept as one terminal node — see the module
    /// docs on why a multi-cell reference is not expanded.
    fn range_node(&mut self, range: RangeRef, depth: usize) -> Option<String> {
        let id = self.workbook.cite_range(range);
        if self.index.contains_key(&id) {
            return Some(id);
        }
        if !self.has_room() {
            return None;
        }
        self.index.insert(id.clone(), self.nodes.len());
        self.nodes.push(GraphNode {
            id: id.clone(),
            kind: NodeKind::Range,
            formula: None,
            value: None,
            depth,
        });
        Some(id)
    }

    /// A node for a target this workbook cannot resolve any further.
    fn dangling_node(&mut self, id: String, kind: NodeKind, depth: usize) -> Option<String> {
        if self.index.contains_key(&id) {
            return Some(id);
        }
        if !self.has_room() {
            return None;
        }
        self.index.insert(id.clone(), self.nodes.len());
        self.nodes.push(GraphNode {
            id: id.clone(),
            kind,
            formula: None,
            value: None,
            depth,
        });
        Some(id)
    }

    fn add_edge(&mut self, from: String, to: String, text: String, kind: EdgeKind) {
        if self.edge_seen.insert((from.clone(), to.clone(), kind)) {
            self.edges.push(GraphEdge {
                from,
                to,
                text,
                kind,
            });
        }
    }

    /// A node for a resolved reference's target, and — for a single unexplored
    /// cell only — the cell to keep walking from.
    fn target_node(&mut self, target: &Target, depth: usize) -> (Option<String>, Option<CellRef>) {
        match target {
            Target::Cells(range) if range.cell_count() == 1 => {
                let cell = range.top_left();
                let already_known = self.index.contains_key(&self.workbook.cite(cell));
                let id = self.cell_node(cell, depth);
                let expand = if already_known {
                    None
                } else {
                    id.clone().and(Some(cell))
                };
                (id, expand)
            }
            Target::Cells(range) => (self.range_node(*range, depth), None),
            Target::Spanned(_) => (
                self.dangling_node(target.cite(self.workbook), NodeKind::Range, depth),
                None,
            ),
            Target::UnknownSheet(_) => (
                self.dangling_node(target.cite(self.workbook), NodeKind::UnknownSheet, depth),
                None,
            ),
            Target::ExternalWorkbook(_) => (
                self.dangling_node(
                    target.cite(self.workbook),
                    NodeKind::ExternalWorkbook,
                    depth,
                ),
                None,
            ),
        }
    }
}

/// One scan of the workbook's formulas, checking each reference against every
/// range in `targets` rather than one — the batching that lets a level of the
/// walk cost one scan regardless of how many cells are on its frontier,
/// instead of one scan per frontier cell.
fn dependents_of_many(
    workbook: &Workbook,
    targets: &[RangeRef],
    limit: usize,
) -> (Vec<Reference>, u64, bool) {
    let sheets = sheet_ids(workbook);
    let mut out = Vec::new();
    let mut scanned = 0u64;
    let mut capped = false;
    let mut spans: Vec<ReferenceSpan> = Vec::new();

    for sheet in &workbook.sheets {
        for (at, cell) in sheet.iter() {
            let Some(formula) = cell.formula.as_deref() else {
                continue;
            };
            scanned += 1;
            scan_references_into(formula, &mut spans);
            for span in &spans {
                let reference = resolve(at, span, formula, &sheets);
                let hits = reference
                    .target
                    .ranges()
                    .iter()
                    .any(|t| targets.iter().any(|seed| overlaps(*t, *seed)));
                if hits {
                    if out.len() < limit {
                        out.push(reference);
                    } else {
                        capped = true;
                    }
                }
            }
        }
    }
    (out, scanned, capped)
}

/// Walk outward from `seed`, following precedents and/or dependents up to
/// `options.depth` hops, and hand back what was found as a graph.
///
/// Depth 0 exports the seed's own populated cells and no edges — useful as
/// `eg cells` with a node/edge shape rather than a reason to call this with
/// a real depth.
pub fn subgraph(workbook: &Workbook, seed: RangeRef, options: &GraphOptions) -> GraphExport {
    let mut b = Builder::new(workbook, options.max_nodes);
    let mut report = GraphReport::default();
    let want_precedents = matches!(options.direction, Direction::Precedents | Direction::Both);
    let want_dependents = matches!(options.direction, Direction::Dependents | Direction::Both);

    let (seed_cells, _) = cells_in(workbook, seed, options.max_nodes);
    let mut prec_frontier: Vec<CellRef> = Vec::new();
    let mut dep_frontier: Vec<RangeRef> = Vec::new();

    for fact in &seed_cells {
        if b.cell_node(fact.cell, 0).is_none() {
            break;
        }
        if fact.formula.is_some() {
            prec_frontier.push(fact.cell);
        }
        b.dep_scanned.insert(fact.cell);
        dep_frontier.push(RangeRef::single(fact.cell));
    }
    // A citation naming exactly one, currently blank cell is still a
    // legitimate thing to have pointed at — worth a node, so the export is
    // not silently empty for it.
    if seed_cells.is_empty() && seed.cell_count() == 1 {
        b.cell_node(seed.top_left(), 0);
    }

    for depth in 1..=options.depth {
        if b.capped {
            break;
        }
        let mut next_prec: Vec<CellRef> = Vec::new();
        let mut next_dep: Vec<RangeRef> = Vec::new();

        if want_precedents {
            'prec: for at in prec_frontier.drain(..) {
                if b.capped {
                    break 'prec;
                }
                let from_id = b.workbook.cite(at);
                for reference in crate::trace::precedents_of(workbook, at) {
                    if b.capped {
                        break 'prec;
                    }
                    let (to_id, expand) = b.target_node(&reference.target, depth);
                    let Some(to_id) = to_id else {
                        break 'prec;
                    };
                    b.add_edge(from_id.clone(), to_id, reference.text, EdgeKind::Precedent);
                    if let Some(cell) = expand {
                        next_prec.push(cell);
                    }
                }
            }
        }

        if want_dependents && !dep_frontier.is_empty() {
            let (refs, scanned, capped) =
                dependents_of_many(workbook, &dep_frontier, options.dependents_limit);
            report.formulas_scanned += scanned;
            report.capped = report.capped || capped;
            // Every range on this level's frontier already has a node — it was
            // added as a cell/range node on a previous level (or the seed) —
            // so this only needs the citation `dependents_of_many` matched
            // against, not a fresh lookup.
            'dep: for reference in refs {
                if b.capped {
                    break 'dep;
                }
                let Some(from_id) = b.cell_node(reference.from, depth) else {
                    break 'dep;
                };
                for target in dep_frontier.iter().filter(|t| {
                    reference
                        .target
                        .ranges()
                        .iter()
                        .any(|found| overlaps(*found, **t))
                }) {
                    let to_id = workbook.cite_range(*target);
                    b.add_edge(
                        from_id.clone(),
                        to_id,
                        reference.text.clone(),
                        EdgeKind::Dependent,
                    );
                }
                // Queue this cell's own dependents for the next level only the
                // first time it is reached — a cycle (`A1=B1+1`, `B1=A1+1`,
                // legal under iterative calculation) would otherwise put it
                // back on the frontier every level, and a dependents level
                // costs a full workbook scan regardless of how few cells are
                // on it.
                if b.dep_scanned.insert(reference.from) {
                    next_dep.push(RangeRef::single(reference.from));
                }
            }
        }

        prec_frontier = next_prec;
        dep_frontier = next_dep;
        if prec_frontier.is_empty() && dep_frontier.is_empty() {
            break;
        }
    }

    report.capped = report.capped || b.capped;
    GraphExport {
        nodes: b.nodes,
        edges: b.edges,
        report,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_model::{Cell, CellFormat, Sheet, SheetId};

    fn formula(text: &str, value: CellValue) -> Cell {
        Cell {
            value,
            formula: Some(text.to_string()),
            format: CellFormat::default(),
        }
    }

    fn workbook() -> Workbook {
        let mut sheet = Sheet::new(SheetId(0), "Sheet1");
        // A1 = B1 + C1 ; B1 = 2 ; C1 = D1 ; D1 = 5
        sheet.set(0, 0, formula("B1+C1", CellValue::Number(7.0)));
        sheet.set(0, 1, Cell::literal(CellValue::Number(2.0)));
        sheet.set(0, 2, formula("D1", CellValue::Number(5.0)));
        sheet.set(0, 3, Cell::literal(CellValue::Number(5.0)));
        Workbook {
            sheets: vec![sheet],
            ..Default::default()
        }
    }

    fn seed(workbook: &Workbook, a1: &str) -> RangeRef {
        eg_model::parse_a1(a1)
            .expect("a1")
            .resolve(workbook.sheet_id_by_name("Sheet1").unwrap())
    }

    #[test]
    fn precedents_walk_stops_at_a_literal_and_follows_a_formula_chain() {
        let wb = workbook();
        let export = subgraph(
            &wb,
            seed(&wb, "Sheet1!A1"),
            &GraphOptions {
                depth: 3,
                direction: Direction::Precedents,
                ..GraphOptions::default()
            },
        );
        let ids: Vec<&str> = export.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"Sheet1!A1"));
        assert!(ids.contains(&"Sheet1!B1"));
        assert!(ids.contains(&"Sheet1!C1"));
        assert!(ids.contains(&"Sheet1!D1"), "{ids:?}");
        assert_eq!(export.edges.len(), 3);
    }

    #[test]
    fn depth_zero_exports_the_seed_alone() {
        let wb = workbook();
        let export = subgraph(
            &wb,
            seed(&wb, "Sheet1!A1"),
            &GraphOptions {
                depth: 0,
                ..GraphOptions::default()
            },
        );
        assert_eq!(export.nodes.len(), 1);
        assert!(export.edges.is_empty());
    }

    #[test]
    fn dependents_walk_finds_what_reads_the_seed() {
        let wb = workbook();
        let export = subgraph(
            &wb,
            seed(&wb, "Sheet1!D1"),
            &GraphOptions {
                depth: 2,
                direction: Direction::Dependents,
                ..GraphOptions::default()
            },
        );
        let ids: Vec<&str> = export.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"Sheet1!C1"), "{ids:?}");
        assert!(ids.contains(&"Sheet1!A1"), "{ids:?}");
    }

    #[test]
    fn a_range_reference_becomes_one_terminal_node_rather_than_being_expanded() {
        let mut sheet = Sheet::new(SheetId(0), "Sheet1");
        sheet.set(0, 0, formula("SUM(B1:B3)", CellValue::Number(6.0)));
        sheet.set(0, 1, Cell::literal(CellValue::Number(1.0)));
        sheet.set(1, 1, Cell::literal(CellValue::Number(2.0)));
        sheet.set(2, 1, Cell::literal(CellValue::Number(3.0)));
        let wb = Workbook {
            sheets: vec![sheet],
            ..Default::default()
        };
        let export = subgraph(
            &wb,
            seed(&wb, "Sheet1!A1"),
            &GraphOptions {
                depth: 2,
                direction: Direction::Precedents,
                ..GraphOptions::default()
            },
        );
        assert!(export
            .nodes
            .iter()
            .any(|n| n.id == "Sheet1!B1:B3" && matches!(n.kind, NodeKind::Range)));
        assert!(!export.nodes.iter().any(|n| n.id == "Sheet1!B1"));
    }

    #[test]
    fn max_nodes_caps_the_walk_rather_than_panicking() {
        let mut sheet = Sheet::new(SheetId(0), "Sheet1");
        for row in 0..10 {
            let text = format!("A{}", row + 2);
            sheet.set(row, 0, formula(&text, CellValue::Number(0.0)));
        }
        sheet.set(10, 0, Cell::literal(CellValue::Number(1.0)));
        let wb = Workbook {
            sheets: vec![sheet],
            ..Default::default()
        };
        let export = subgraph(
            &wb,
            seed(&wb, "Sheet1!A1"),
            &GraphOptions {
                depth: 20,
                direction: Direction::Precedents,
                max_nodes: 3,
                ..GraphOptions::default()
            },
        );
        assert!(export.nodes.len() <= 3);
        assert!(export.report.capped || export.nodes.len() < 3);
    }

    #[test]
    fn a_dependents_cycle_scans_each_cell_once_rather_than_once_per_depth() {
        // A1 = B1 + 1 ; B1 = A1 + 1 — a circular reference, legal under
        // iterative calculation. Walking dependents from A1 should discover
        // the two-cell cycle and then stop growing the frontier, rather than
        // re-scanning the same two cells at every remaining depth.
        let mut sheet = Sheet::new(SheetId(0), "Sheet1");
        sheet.set(0, 0, formula("B1+1", CellValue::Number(1.0)));
        sheet.set(0, 1, formula("A1+1", CellValue::Number(1.0)));
        let wb = Workbook {
            sheets: vec![sheet],
            ..Default::default()
        };
        let export = subgraph(
            &wb,
            seed(&wb, "Sheet1!A1"),
            &GraphOptions {
                depth: 20,
                direction: Direction::Dependents,
                ..GraphOptions::default()
            },
        );
        assert_eq!(export.nodes.len(), 2, "{:?}", export.nodes);
        // Every level after the cycle is discovered costs nothing further —
        // one formula each for A1 and B1's own dependents scan.
        assert_eq!(export.report.formulas_scanned, 4, "{:?}", export.report);
        assert!(!export.report.capped);
    }
}
