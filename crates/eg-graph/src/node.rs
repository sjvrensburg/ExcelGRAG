//! What the graph is made of.
//!
//! Every node is an *aggregate* — a sheet, a region, a column, a group of
//! identical formulas. Individual cells are deliberately absent: there are 43.5
//! million of them in the reference workbook, and an agent asking "where does
//! this number come from?" wants the column and the table, not a cell soup.
//! Cell-level detail is recovered on demand by re-reading the workbook, which is
//! cheap because every node carries the range it covers.

use eg_model::{CellRef, RangeRef, SheetId};
use eg_structure::{RegionKind, RegionSource};
use serde::{Deserialize, Serialize};

/// A node of the workbook graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Node {
    /// The root. One per workbook.
    Workbook(WorkbookNode),
    Sheet(SheetNode),
    /// A table or block found by [`eg_structure::detect_regions`].
    Region(RegionNode),
    /// One column of a region, under its header.
    Column(ColumnNode),
    /// A rectangle of cells sharing one formula shape.
    FormulaGroup(FormulaGroupNode),
    /// A name defined in the workbook, whether or not anything uses it.
    DefinedName(DefinedNameNode),
    /// Another workbook this one reads from. Held as the token the formula
    /// wrote, because no reader we have resolves it to a path.
    ExternalWorkbook(ExternalWorkbookNode),
}

/// The kind of a node, without its payload. Used for counting and filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum NodeKind {
    Workbook,
    Sheet,
    Region,
    Column,
    FormulaGroup,
    DefinedName,
    ExternalWorkbook,
}

impl NodeKind {
    /// Every kind, in the order a report should list them.
    pub const ALL: [NodeKind; 7] = [
        NodeKind::Workbook,
        NodeKind::Sheet,
        NodeKind::Region,
        NodeKind::Column,
        NodeKind::FormulaGroup,
        NodeKind::DefinedName,
        NodeKind::ExternalWorkbook,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Workbook => "workbook",
            NodeKind::Sheet => "sheet",
            NodeKind::Region => "region",
            NodeKind::Column => "column",
            NodeKind::FormulaGroup => "formula group",
            NodeKind::DefinedName => "defined name",
            NodeKind::ExternalWorkbook => "external workbook",
        }
    }

    /// The inverse of [`NodeKind::as_str`].
    ///
    /// `None` rather than a default, because a kind read back from a store or
    /// an index that we do not recognise means the file is not ours, and
    /// guessing `Workbook` would put a stranger at the root of the graph.
    pub fn parse(s: &str) -> Option<NodeKind> {
        NodeKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl Node {
    pub fn kind(&self) -> NodeKind {
        match self {
            Node::Workbook(_) => NodeKind::Workbook,
            Node::Sheet(_) => NodeKind::Sheet,
            Node::Region(_) => NodeKind::Region,
            Node::Column(_) => NodeKind::Column,
            Node::FormulaGroup(_) => NodeKind::FormulaGroup,
            Node::DefinedName(_) => NodeKind::DefinedName,
            Node::ExternalWorkbook(_) => NodeKind::ExternalWorkbook,
        }
    }

    /// The sheet this node lives on, if it lives on exactly one.
    ///
    /// `None` for the workbook root, for external workbooks, and for a defined
    /// name of workbook scope.
    pub fn sheet(&self) -> Option<SheetId> {
        match self {
            Node::Workbook(_) | Node::ExternalWorkbook(_) => None,
            Node::Sheet(s) => Some(s.id),
            Node::Region(r) => Some(r.range.sheet),
            Node::Column(c) => Some(c.range.sheet),
            Node::FormulaGroup(g) => Some(g.range.sheet),
            Node::DefinedName(n) => n.scope,
        }
    }

    /// The cells this node covers, for nodes that cover a rectangle.
    pub fn range(&self) -> Option<RangeRef> {
        match self {
            Node::Region(r) => Some(r.range),
            Node::Column(c) => Some(c.range),
            Node::FormulaGroup(g) => Some(g.range),
            _ => None,
        }
    }

    /// A short label for a person or an agent to read.
    ///
    /// Not a citation: it names the node, and carries an A1 range only where
    /// the range *is* the identity. Use [`eg_model::Workbook::cite_range`] with
    /// [`Node::range`] when the answer needs a citation.
    pub fn label(&self) -> String {
        match self {
            Node::Workbook(w) => w.path.clone(),
            Node::Sheet(s) => s.name.clone(),
            Node::Region(r) => match &r.title {
                Some(t) => t.clone(),
                None => r.range.to_a1(),
            },
            Node::Column(c) => c.header.clone(),
            Node::FormulaGroup(g) => g.representative.clone(),
            Node::DefinedName(n) => n.name.clone(),
            Node::ExternalWorkbook(x) => x.token.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkbookNode {
    pub path: String,
    /// blake3 of the source file, so an unchanged workbook is recognisable.
    pub content_hash: String,
    pub format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SheetNode {
    pub id: SheetId,
    pub name: String,
    pub visible: bool,
    pub cells: u64,
    pub formula_cells: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegionNode {
    pub range: RangeRef,
    pub kind: RegionKind,
    pub source: RegionSource,
    pub title: Option<String>,
    pub header_rows: u32,
    pub cell_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnNode {
    /// The column's body, excluding the header rows above it.
    pub range: RangeRef,
    /// The header text, with stacked header rows joined by `.`.
    pub header: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormulaGroupNode {
    pub range: RangeRef,
    /// The shared shape, in R1C1.
    pub shape: String,
    /// The formula as written at the group's top-left cell, in A1.
    pub representative: String,
    pub cell_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinedNameNode {
    pub name: String,
    pub refers_to: String,
    /// `None` for a workbook-scoped name.
    pub scope: Option<SheetId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalWorkbookNode {
    /// The qualifier as the formula wrote it, e.g. `1` from `[1]Sheet1!A1`.
    ///
    /// Resolving that index to a filename needs the workbook's external-link
    /// table, which no reader available to us exposes. Recorded rather than
    /// dropped: an agent must be able to say "this depends on a workbook I
    /// cannot see", which is very different from "this depends on nothing".
    pub token: String,
}

/// An edge, and how much evidence stands behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub kind: EdgeKind,
    /// How many underlying cell references produced this edge, always at
    /// least 1. Structural edges carry 1; a lifted dependency carries the
    /// number of real references it stands for, which is what makes it
    /// rankable.
    pub weight: u64,
}

impl Edge {
    pub fn new(kind: EdgeKind) -> Self {
        Edge { kind, weight: 1 }
    }
}

/// The kinds of relation the graph records.
///
/// The three dependency kinds partition every lifted reference: a reference is
/// within one sheet, across two sheets of this workbook, or into another
/// workbook. Splitting them by kind rather than by a flag means a traversal can
/// decide to stay on one sheet without inspecting node payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EdgeKind {
    /// Structural nesting: workbook → sheet → region → column, and region →
    /// formula group.
    Contains,
    /// A column heads a formula group that lies within it.
    HeaderOf,
    /// A formula in the source reads a cell in the target, same sheet.
    DependsOn,
    /// As `DependsOn`, but the target is on another sheet.
    CrossSheetRef,
    /// A formula in the source reads another workbook.
    CrossWorkbookRef,
    /// A formula in the source uses a defined name.
    ReferencesName,
}

impl EdgeKind {
    pub const ALL: [EdgeKind; 6] = [
        EdgeKind::Contains,
        EdgeKind::HeaderOf,
        EdgeKind::DependsOn,
        EdgeKind::CrossSheetRef,
        EdgeKind::CrossWorkbookRef,
        EdgeKind::ReferencesName,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Contains => "CONTAINS",
            EdgeKind::HeaderOf => "HEADER_OF",
            EdgeKind::DependsOn => "DEPENDS_ON",
            EdgeKind::CrossSheetRef => "CROSS_SHEET_REF",
            EdgeKind::CrossWorkbookRef => "CROSS_WORKBOOK_REF",
            EdgeKind::ReferencesName => "REFERENCES_NAME",
        }
    }

    /// Whether the edge is structural rather than derived from a formula.
    pub fn is_structural(self) -> bool {
        matches!(self, EdgeKind::Contains | EdgeKind::HeaderOf)
    }
}

/// A reference that could not be resolved to a node.
///
/// Kept, never discarded. A formula pointing at a deleted sheet is a real
/// finding about the workbook, and an agent that silently ignores it will
/// describe a broken model as a working one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DanglingRef {
    /// The cell whose formula wrote it.
    pub from: CellRef,
    /// The reference as written.
    pub text: String,
    pub reason: DanglingReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DanglingReason {
    /// The formula names a sheet the workbook does not have — a `#REF!` break.
    UnknownSheet(String),
    /// The reference resolves to a sheet, but lands where the sheet holds no
    /// cells, so there is no region to point at. Not damage: a formula reading
    /// an empty cell is legal and common. Recorded because a dependency that
    /// leads nowhere still explains why a number is zero.
    UnpopulatedTarget,
}
