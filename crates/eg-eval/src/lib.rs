//! Recovering the cell layer the graph deliberately dropped.
//!
//! `eg-graph` lifts every reference to the region containing it, so a
//! dependency in the graph is between tables. That is the right granularity to
//! traverse and the wrong one to explain a number with. P6 is the other half:
//! reading the workbook itself to say which cell fed which.
//!
//! [`trace`] is that, and it is where evaluation will stand — recomputing a
//! formula needs the cells under it, and an evaluator that cannot say where a
//! number came from is a second opinion rather than an explanation.
//!
//! ```no_run
//! # use eg_eval::{precedents_of, Target};
//! # use eg_model::{CellRef, SheetId};
//! let loaded = eg_ingest::load("book.xlsx")?;
//! let at = CellRef::new(SheetId(0), 1, 5);
//! for reference in precedents_of(&loaded.workbook, at) {
//!     match reference.target {
//!         Target::Cells(range) => println!("{} reads {}", reference.text, range.to_a1()),
//!         other => println!("{} points outside the workbook: {other:?}", reference.text),
//!     }
//! }
//! # Ok::<(), eg_ingest::IngestError>(())
//! ```

pub mod trace;

pub use trace::{
    cell, cells_in, dependents_of, precedents_of, CellFact, Reference, ScanReport, Target,
};
