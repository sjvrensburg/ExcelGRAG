//! Core data model for ExcelGRAG: addressing, cell values, and the graph schema.
//!
//! This crate is deliberately dependency-light and side-effect free. Every other
//! crate in the workspace speaks in these types.

pub mod address;
pub mod cell;
pub mod formula;
pub mod workbook;

pub use address::{
    col_to_letters, letters_to_col, parse_a1, quote_sheet_name, AddressError, CellRef, ParsedRef,
    R1C1Coord, R1C1Ref, RangeRef, SheetId, MAX_COL, MAX_ROW,
};
pub use cell::{Cell, CellFormat, CellValue, ErrorKind, ValueKind};
pub use formula::{scan_references, to_r1c1_shape, ReferenceSpan};
pub use workbook::{DefinedName, ExcelTable, Sheet, Visibility, Workbook, WorkbookFormat};
