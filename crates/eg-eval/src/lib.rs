//! Recovering the cell layer the graph deliberately dropped.
//!
//! `eg-graph` lifts every reference to the region containing it, so a
//! dependency in the graph is between tables. That is the right granularity to
//! traverse and the wrong one to explain a number with. P6 is the other half:
//! reading the workbook itself to say which cell fed which.
//!
//! [`trace`] is that. [`calc`] is the other half of P6: recomputing a formula
//! from the values under it and comparing that with the number the workbook
//! stored. The order matters — an evaluator that cannot say where a number came
//! from is a second opinion rather than an explanation, so tracing came first
//! and recomputing hands back the cells it read.
//!
//! Precedents are read as stored values and never recursively recomputed, so a
//! disagreement is about one formula. What [`calc`] does not model it refuses
//! by name rather than guessing.
//!
//! ```no_run
//! # use eg_eval::precedents_of;
//! # use eg_model::{CellRef, SheetId};
//! let loaded = eg_ingest::load("book.xlsx")?;
//! let at = CellRef::new(SheetId(0), 1, 5);
//! for reference in precedents_of(&loaded.workbook, at) {
//!     // `ranges` is every range named — one, or one per sheet of a 3-D
//!     // span, or none at all for a reference out of this workbook.
//!     for range in reference.target.ranges() {
//!         println!("{} reads {}", reference.text, range.to_a1());
//!     }
//! }
//! # Ok::<(), eg_ingest::IngestError>(())
//! ```

pub mod calc;
pub mod parse;
pub mod query;
pub mod schema;
pub mod trace;
pub mod whatif;

pub use calc::{
    check, evaluate, evaluate_over, recompute, recompute_over, CheckReport, Evaluator, Input,
    Outcome, Overrides, Recomputed, Unsupported,
};
pub use parse::{parse, BinOp, Expr, ParseError, UnaryOp};
pub use query::{query, Aggregate, Answer, Filter, Group, Query, QueryError, Test};
pub use schema::{infer_schema, Lookup, LookupKind, Schema};
pub use trace::{
    cell, cells_in, dependents_of, precedents_of, CellFact, Reference, ScanReport, Target,
};
pub use whatif::{
    what_if, Applied, Blocked, Change, Impact, ImpactReport, Moved, Stopped, WhatIfOptions,
};
