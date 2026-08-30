//! The workbook graph: aggregates, and the dependencies between them.
//!
//! A spreadsheet is a dependency graph already, but at the wrong granularity to
//! be useful. The reference workbook holds 43.5 million cells and 6.79 million
//! formulas; a cell-level graph of it is larger than the workbook and no easier
//! for an agent to reason about than the raw grid.
//!
//! So this crate builds the graph over *aggregates* — sheets, regions, columns,
//! groups of identical formulas — and lifts every cell reference to the region
//! containing it, keeping the number of references behind each edge as a weight.
//! What survives is a graph an agent can traverse and a reader can check, where
//! each node still names the exact cells it stands for, so cell-level detail is
//! one workbook read away rather than stored a million times over.
//!
//! ```no_run
//! let loaded = eg_ingest::load("book.xlsx")?;
//! let built = eg_graph::build(&loaded.workbook);
//! println!("{} nodes, {} edges", built.report.total_nodes(), built.report.total_edges());
//! assert!(eg_graph::check(&built).is_empty());
//! # Ok::<(), eg_ingest::IngestError>(())
//! ```

pub mod build;
pub mod check;
pub mod node;
pub mod report;

pub use build::{
    build, build_with, nodes_of_kind, reachable_from, BuiltGraph, Graph, GraphOptions,
};
pub use check::{check, Violation};
pub use node::{
    ColumnNode, DanglingReason, DanglingRef, DefinedNameNode, Edge, EdgeKind, ExternalWorkbookNode,
    FormulaGroupNode, Node, NodeKind, RegionNode, SheetNode, WorkbookNode,
};
pub use report::{degree_stats, BuildReport, DegreeStats};
