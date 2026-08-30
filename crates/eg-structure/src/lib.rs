//! Structural analysis: recovering the shapes a spreadsheet was built from.
//!
//! A workbook is a grid of cells, but people write it as tables, filled-down
//! columns and totals rows. Recovering that structure is what lets the graph
//! stay small enough to index and readable enough to cite.

pub mod formula_group;
pub mod region;

pub use formula_group::{
    find_shape_exceptions, group_formulas, FormulaGroup, GroupingStats, ShapeException,
};
pub use region::{
    detect_regions, detect_regions_with, detect_workbook_regions, Region, RegionKind,
    RegionOptions, RegionSource,
};
