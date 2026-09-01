//! The in-memory workbook: sparse sheets, defined names, tables, and merges.
//!
//! Sheets are stored sparsely. A worksheet nominally addresses a million rows by
//! sixteen thousand columns, but a real one is overwhelmingly blank, so only
//! populated cells are materialised. The backing map is ordered by `(row, col)`
//! so that row-major scans — which is how region detection reads a sheet — come
//! for free without a sort.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::address::{CellRef, RangeRef, SheetId};
use crate::cell::{Cell, CellValue, ValueKind};

/// Source file format, which determines what operations are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkbookFormat {
    Xlsx,
    Xlsm,
    Xlsb,
    Xls,
    Ods,
}

impl WorkbookFormat {
    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "xlsx" => WorkbookFormat::Xlsx,
            "xlsm" => WorkbookFormat::Xlsm,
            "xlsb" => WorkbookFormat::Xlsb,
            "xls" => WorkbookFormat::Xls,
            "ods" => WorkbookFormat::Ods,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            WorkbookFormat::Xlsx => "xlsx",
            WorkbookFormat::Xlsm => "xlsm",
            WorkbookFormat::Xlsb => "xlsb",
            WorkbookFormat::Xls => "xls",
            WorkbookFormat::Ods => "ods",
        }
    }

    /// Whether a modified workbook can be written back in this format.
    ///
    /// Only `xlsx` can be written. XLSB in particular is read-only: no Rust
    /// crate can serialise the binary format, so what-if analysis on an XLSB
    /// stays in memory. Callers must surface this rather than failing at save.
    pub fn is_writable(&self) -> bool {
        matches!(self, WorkbookFormat::Xlsx)
    }
}

impl std::fmt::Display for WorkbookFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a sheet is visible in the Excel UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
    /// Hidden and not un-hideable from the UI; usually machinery, not content.
    VeryHidden,
}

impl Visibility {
    pub fn is_visible(&self) -> bool {
        matches!(self, Visibility::Visible)
    }
}

/// A name defined at workbook or sheet scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DefinedName {
    pub name: String,
    /// The raw formula text the name refers to, e.g. `Sheet1!$A$1:$B$9`.
    pub refers_to: String,
    /// `None` for workbook scope, otherwise the sheet the name is local to.
    pub scope: Option<SheetId>,
}

/// A structured Excel table (`ListObject`) declared in the workbook.
///
/// These are ground truth for region detection: where Excel says a table is, we
/// do not guess.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExcelTable {
    pub name: String,
    pub range: RangeRef,
    /// Column names in left-to-right order, as declared by the table.
    pub columns: Vec<String>,
    pub has_header_row: bool,
    pub has_totals_row: bool,
}

/// One sheet's contents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sheet {
    pub id: SheetId,
    pub name: String,
    pub visibility: Visibility,
    /// Populated cells, ordered row-major by the map's key ordering.
    cells: BTreeMap<(u32, u16), Cell>,
    pub merges: Vec<RangeRef>,
    /// Tables declared on this sheet.
    pub tables: Vec<ExcelTable>,
    /// Columns explicitly hidden in the UI.
    pub hidden_cols: Vec<u16>,
    pub hidden_rows: Vec<u32>,
    used: Option<RangeRef>,
}

impl Sheet {
    pub fn new(id: SheetId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            ..Default::default()
        }
    }

    /// Insert a cell, growing the cached used-range. Vacant cells are dropped.
    pub fn set(&mut self, row: u32, col: u16, cell: Cell) {
        if cell.is_vacant() && cell.format == Default::default() {
            if self.cells.remove(&(row, col)).is_some()
                && self.used.is_some_and(|used| {
                    row == used.top || row == used.bottom || col == used.left || col == used.right
                })
            {
                self.recompute_used_range();
            }
            return;
        }
        let r = RangeRef::new(self.id, row, col, row, col);
        self.used = Some(match self.used {
            Some(u) => u.union(&r),
            None => r,
        });
        self.cells.insert((row, col), cell);
    }

    pub fn get(&self, row: u32, col: u16) -> Option<&Cell> {
        self.cells.get(&(row, col))
    }

    pub fn get_ref(&self, cell: CellRef) -> Option<&Cell> {
        (cell.sheet == self.id)
            .then(|| self.get(cell.row, cell.col))
            .flatten()
    }

    pub fn value(&self, row: u32, col: u16) -> CellValue {
        self.get(row, col)
            .map(|c| c.value.clone())
            .unwrap_or(CellValue::Empty)
    }

    pub fn kind(&self, row: u32, col: u16) -> ValueKind {
        self.get(row, col)
            .map(|c| c.value.kind())
            .unwrap_or(ValueKind::Empty)
    }

    pub fn is_populated(&self, row: u32, col: u16) -> bool {
        self.get(row, col).is_some_and(|c| !c.is_vacant())
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The bounding box of populated cells, or `None` for an empty sheet.
    pub fn used_range(&self) -> Option<RangeRef> {
        self.used
    }

    /// Iterate all populated cells in row-major order.
    pub fn iter(&self) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        self.cells
            .iter()
            .map(move |(&(r, c), cell)| (CellRef::new(self.id, r, c), cell))
    }

    /// Iterate the populated cells of one row, left to right.
    ///
    /// This is the hot path for region detection, and the ordered map makes it a
    /// contiguous range scan rather than a filter over the whole sheet.
    pub fn iter_row(&self, row: u32) -> impl Iterator<Item = (u16, &Cell)> + '_ {
        self.cells
            .range((row, 0u16)..=(row, u16::MAX))
            .map(|(&(_, c), cell)| (c, cell))
    }

    /// Iterate populated cells within a range, row-major.
    pub fn iter_range(&self, range: RangeRef) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        (range.sheet == self.id)
            .then_some(range)
            .into_iter()
            .flat_map(move |range| {
                (range.top..=range.bottom).flat_map(move |r| {
                    self.cells
                        .range((r, range.left)..=(r, range.right))
                        .map(move |(&(_, c), cell)| (CellRef::new(self.id, r, c), cell))
                })
            })
    }

    /// The merged range covering this cell, if any.
    pub fn merge_at(&self, row: u32, col: u16) -> Option<RangeRef> {
        self.merges
            .iter()
            .find(|m| m.contains(CellRef::new(self.id, row, col)))
            .copied()
    }

    /// Rebuild cached bounds after removing a cell on one of their edges.
    /// Interior removals cannot change the bounds and never pay this scan.
    fn recompute_used_range(&mut self) {
        let mut cells = self.cells.keys();
        let Some(&(first_row, first_col)) = cells.next() else {
            self.used = None;
            return;
        };
        let (mut bottom, mut left, mut right) = (first_row, first_col, first_col);
        for &(row, col) in cells {
            bottom = row;
            left = left.min(col);
            right = right.max(col);
        }
        self.used = Some(RangeRef::new(self.id, first_row, left, bottom, right));
    }
}

/// A whole workbook as loaded from disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workbook {
    /// Path the workbook was loaded from, for citations and re-ingest.
    pub path: String,
    pub format: Option<WorkbookFormat>,
    /// Content hash of the source file, used to skip unchanged re-ingests.
    pub content_hash: String,
    pub sheets: Vec<Sheet>,
    pub defined_names: Vec<DefinedName>,
    /// Paths of workbooks referenced by external links, in link-index order.
    pub external_links: Vec<String>,
}

impl Workbook {
    pub fn sheet(&self, id: SheetId) -> Option<&Sheet> {
        self.sheets.get(id.0 as usize)
    }

    pub fn sheet_mut(&mut self, id: SheetId) -> Option<&mut Sheet> {
        self.sheets.get_mut(id.0 as usize)
    }

    /// Look up a sheet by name, case-insensitively as Excel does.
    pub fn sheet_by_name(&self, name: &str) -> Option<&Sheet> {
        self.sheets
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))
    }

    pub fn sheet_id_by_name(&self, name: &str) -> Option<SheetId> {
        self.sheet_by_name(name).map(|s| s.id)
    }

    pub fn sheet_name(&self, id: SheetId) -> Option<&str> {
        self.sheet(id).map(|s| s.name.as_str())
    }

    /// Render a fully-qualified citation for a cell, falling back to the sheet
    /// index when the name is unknown so a citation is never silently dropped.
    pub fn cite(&self, cell: CellRef) -> String {
        match self.sheet_name(cell.sheet) {
            Some(name) => cell.to_a1_with_sheet(name),
            None => format!("{}!{}", cell.sheet, cell.to_a1()),
        }
    }

    pub fn cite_range(&self, range: RangeRef) -> String {
        match self.sheet_name(range.sheet) {
            Some(name) => range.to_a1_with_sheet(name),
            None => format!("{}!{}", range.sheet, range.to_a1()),
        }
    }

    pub fn total_cells(&self) -> usize {
        self.sheets.iter().map(|s| s.len()).sum()
    }

    /// Whether edits to this workbook could be written back to its own format.
    pub fn is_writable(&self) -> bool {
        self.format.is_some_and(|f| f.is_writable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{CellFormat, CellValue};

    fn sheet_with(cells: &[(u32, u16, &str)]) -> Sheet {
        let mut s = Sheet::new(SheetId(0), "Sheet1");
        for &(r, c, v) in cells {
            s.set(r, c, Cell::literal(CellValue::Text(v.to_string())));
        }
        s
    }

    #[test]
    fn xlsb_is_read_only() {
        assert!(!WorkbookFormat::Xlsb.is_writable());
        assert!(!WorkbookFormat::Xls.is_writable());
        assert!(!WorkbookFormat::Ods.is_writable());
        assert!(WorkbookFormat::Xlsx.is_writable());
    }

    #[test]
    fn format_detected_from_extension() {
        assert_eq!(
            WorkbookFormat::from_extension("XLSB"),
            Some(WorkbookFormat::Xlsb)
        );
        assert_eq!(WorkbookFormat::from_extension("csv"), None);
    }

    #[test]
    fn used_range_tracks_inserts() {
        let s = sheet_with(&[(2, 1, "a"), (5, 3, "b"), (0, 7, "c")]);
        let u = s.used_range().unwrap();
        assert_eq!(u.to_a1(), "B1:H6");
    }

    #[test]
    fn empty_sheet_has_no_used_range() {
        let s = Sheet::new(SheetId(0), "Sheet1");
        assert!(s.used_range().is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn vacant_unformatted_cells_are_not_stored() {
        let mut s = Sheet::new(SheetId(0), "Sheet1");
        s.set(0, 0, Cell::literal(CellValue::Empty));
        assert_eq!(s.len(), 0);

        // A blank cell that is *formatted* still carries structural signal.
        s.set(
            1,
            1,
            Cell {
                value: CellValue::Empty,
                formula: None,
                format: CellFormat {
                    has_fill: true,
                    ..Default::default()
                },
            },
        );
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn setting_a_vacant_cell_removes_an_existing_one() {
        let mut s = sheet_with(&[(0, 0, "x")]);
        assert_eq!(s.len(), 1);
        s.set(0, 0, Cell::literal(CellValue::Empty));
        assert_eq!(s.len(), 0);
        assert!(s.used_range().is_none());
    }

    #[test]
    fn removing_a_boundary_cell_shrinks_the_used_range() {
        let mut s = sheet_with(&[(0, 4, "top"), (2, 1, "left"), (3, 3, "bottom")]);
        assert_eq!(s.used_range().unwrap().to_a1(), "B1:E4");
        s.set(0, 4, Cell::literal(CellValue::Empty));
        assert_eq!(s.used_range().unwrap().to_a1(), "B3:D4");
    }

    #[test]
    fn iteration_is_row_major() {
        let s = sheet_with(&[(1, 1, "d"), (0, 1, "b"), (1, 0, "c"), (0, 0, "a")]);
        let got: Vec<&str> = s.iter().map(|(_, c)| c.value.as_text().unwrap()).collect();
        assert_eq!(got, ["a", "b", "c", "d"]);
    }

    #[test]
    fn row_scan_returns_only_that_row() {
        let s = sheet_with(&[(0, 0, "a"), (0, 5, "b"), (1, 0, "c")]);
        let got: Vec<u16> = s.iter_row(0).map(|(c, _)| c).collect();
        assert_eq!(got, [0, 5]);
        assert_eq!(s.iter_row(2).count(), 0);
    }

    #[test]
    fn range_scan_clips_to_the_box() {
        let s = sheet_with(&[(0, 0, "a"), (0, 5, "b"), (1, 1, "c"), (9, 9, "d")]);
        let r = RangeRef::parse_local("A1:C3", SheetId(0)).unwrap();
        let got: Vec<String> = s.iter_range(r).map(|(a, _)| a.to_a1()).collect();
        assert_eq!(got, ["A1", "B2"]);
    }

    #[test]
    fn cell_and_range_access_refuse_a_different_sheet() {
        let s = sheet_with(&[(0, 0, "a")]);
        assert!(s.get_ref(CellRef::new(SheetId(1), 0, 0)).is_none());
        let other = RangeRef::new(SheetId(1), 0, 0, 0, 0);
        assert_eq!(s.iter_range(other).count(), 0);
    }

    #[test]
    fn merges_are_looked_up_by_cell() {
        let mut s = Sheet::new(SheetId(0), "Sheet1");
        s.merges
            .push(RangeRef::parse_local("B2:D2", SheetId(0)).unwrap());
        assert_eq!(s.merge_at(1, 2).unwrap().to_a1(), "B2:D2");
        assert!(s.merge_at(0, 0).is_none());
    }

    #[test]
    fn sheet_lookup_is_case_insensitive() {
        let wb = Workbook {
            sheets: vec![Sheet::new(SheetId(0), "Q3 Sales")],
            ..Default::default()
        };
        assert!(wb.sheet_by_name("q3 sales").is_some());
        assert_eq!(wb.sheet_id_by_name("Q3 SALES"), Some(SheetId(0)));
        assert!(wb.sheet_by_name("Missing").is_none());
    }

    #[test]
    fn citations_quote_sheet_names_and_survive_unknown_sheets() {
        let wb = Workbook {
            sheets: vec![Sheet::new(SheetId(0), "Q3 Sales")],
            ..Default::default()
        };
        assert_eq!(wb.cite(CellRef::new(SheetId(0), 6, 1)), "'Q3 Sales'!B7");
        // A dangling sheet id must still yield something traceable.
        assert_eq!(wb.cite(CellRef::new(SheetId(9), 0, 0)), "#9!A1");
    }
}
