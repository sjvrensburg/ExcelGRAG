//! A region, read as the table it already is.
//!
//! Region detection recovers the rectangle, the header rows, the row-label
//! columns and one header string per body column. That is a table definition in
//! everything but name, and until now nothing could turn it back into rows —
//! the graph says *where* a table is, and a caller wanting what is in it had
//! only a rectangle of cells to walk.
//!
//! What this adds is the column: a header, the cells beneath it, and a type
//! read off those cells rather than off a format, because calamine exposes no
//! formatting for any of the five readable formats. A column is [`ColumnKind`]
//! by majority of what it holds, and a column that cannot decide says so rather
//! than picking.
//!
//! Rows are produced lazily and blanks are filled in. A sheet stores only its
//! populated cells, so the third column of a row whose third cell is empty is
//! *absent*, not blank — and a table that silently shortened its rows would
//! misalign every column after the gap.
//!
//! Nothing here reads a formula or evaluates anything. This is the data as the
//! workbook last saved it.

use eg_model::{CellRef, CellValue, RangeRef, Sheet, ValueKind};
use serde::{Deserialize, Serialize};

use crate::region::Region;

/// What a column holds, by majority of its populated cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnKind {
    Number,
    Text,
    Bool,
    /// Every cell is an error value.
    Error,
    /// Populated, and no kind holds a majority — a column of numbers with a
    /// "n/a" written into a third of it. Named rather than resolved, because
    /// summing it and reading it as text are both wrong.
    Mixed,
    /// No populated cell at all.
    Empty,
}

impl ColumnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ColumnKind::Number => "number",
            ColumnKind::Text => "text",
            ColumnKind::Bool => "bool",
            ColumnKind::Error => "error",
            ColumnKind::Mixed => "mixed",
            ColumnKind::Empty => "empty",
        }
    }

    /// Whether arithmetic over this column means anything.
    pub fn is_numeric(self) -> bool {
        self == ColumnKind::Number
    }

    /// The kind holding a majority of `counts`, in the order of [`ValueKind`]'s
    /// populated variants.
    ///
    /// A strict majority, not a plurality: a column that is 40% number, 35%
    /// text and 25% error has no type anyone should rely on, and calling it a
    /// number column because numbers came first is how a total ends up wrong.
    fn of(numbers: u64, text: u64, bools: u64, errors: u64) -> ColumnKind {
        let populated = numbers + text + bools + errors;
        if populated == 0 {
            return ColumnKind::Empty;
        }
        let half = populated / 2;
        match (numbers, text, bools, errors) {
            (n, _, _, _) if n > half => ColumnKind::Number,
            (_, t, _, _) if t > half => ColumnKind::Text,
            (_, _, b, _) if b > half => ColumnKind::Bool,
            (_, _, _, e) if e > half => ColumnKind::Error,
            _ => ColumnKind::Mixed,
        }
    }
}

/// One column of a table: a name, the cells under it, and what they are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableColumn {
    /// The header as written, with stacked header rows joined by `.`. Empty for
    /// a body column the region found no header for.
    pub header: String,
    /// The body cells of this column, excluding header and title rows.
    pub range: RangeRef,
    pub kind: ColumnKind,
    /// Populated cells. The rest of `range` is empty.
    pub populated: u64,
}

/// A region, read as a table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    /// The data rows, excluding title and header.
    pub body: RangeRef,
    /// The region's title, where it has one.
    pub title: Option<String>,
    /// Left to right, one per body column.
    pub columns: Vec<TableColumn>,
    /// The row-label columns to the left of the body, if the region has any.
    ///
    /// Kept separate because they are not data: they name the rows. Region
    /// detection excludes them from `headers`, which is why nothing in the
    /// graph stands for them — see the retrieval suite's recorded gap.
    pub labels: Option<RangeRef>,
}

impl Table {
    /// How many rows the body covers, blank ones included.
    pub fn rows(&self) -> u32 {
        self.body.bottom - self.body.top + 1
    }

    /// The column with this header, case-insensitively.
    ///
    /// `None` rather than a guess when two columns share a header, which real
    /// workbooks do: `Total` under both `Q3` and `Q4` is two different numbers
    /// and answering with either would be a coin toss.
    pub fn column(&self, header: &str) -> Option<&TableColumn> {
        let mut found = None;
        for column in &self.columns {
            if column.header.eq_ignore_ascii_case(header) {
                if found.is_some() {
                    return None;
                }
                found = Some(column);
            }
        }
        found
    }

    /// The rows of the body, in order, one value per column.
    ///
    /// Lazy, because a region of the reference workbook is 136 columns by
    /// 115,392 rows. Empty cells are filled in as [`CellValue::Empty`] so that
    /// row `n` position `i` is always column `i` — a sheet holds only its
    /// populated cells, and a row that skipped its gaps would misalign
    /// everything after them.
    ///
    /// One `iter_range` scan per row, not one point lookup per cell: a sheet
    /// is an ordered map keyed by (row, column), so a per-column probe costs a
    /// `BTreeMap` lookup each — the same access pattern that measured 3.5x
    /// slower in `read_table` before it was rewritten to a single row-major
    /// pass, which this reproduced by handing out a row iterator that never
    /// got the same treatment.
    pub fn read<'a>(&'a self, sheet: &'a Sheet) -> impl Iterator<Item = Vec<CellValue>> + 'a {
        let left = self.body.left;
        let width = self.columns.len();
        (self.body.top..=self.body.bottom).map(move |row| {
            let mut out = vec![CellValue::Empty; width];
            let row_range = RangeRef::new(self.body.sheet, row, left, row, self.body.right);
            for (at, cell) in sheet.iter_range(row_range) {
                if let Some(slot) = out.get_mut(usize::from(at.col - left)) {
                    *slot = cell.value.clone();
                }
            }
            out
        })
    }

    /// The label of one body row, where the region has a row-label column.
    pub fn label_of(&self, sheet: &Sheet, row: u32) -> Option<CellValue> {
        let labels = self.labels?;
        sheet
            .get_ref(CellRef::new(labels.sheet, row, labels.left))
            .map(|cell| cell.value.clone())
    }
}

/// Read a detected region as a table.
///
/// `None` when the region has no body at all — a title with nothing under it,
/// or a header row that is the whole of it.
pub fn read_table(sheet: &Sheet, region: &Region) -> Option<Table> {
    let body = region.body()?;
    let width = usize::from(body.right - body.left) + 1;

    // One row-major pass over the region, not one pass per column. A sheet is
    // an ordered map keyed by (row, column), so asking it for a single column
    // costs a probe per row — 115,392 of them on the reference workbook, times
    // 136 columns. Walking the region once and dispatching each cell to its
    // column's tally is the same answer for the cells the region actually has.
    let mut tally = vec![[0u64; 4]; width];
    for (at, cell) in sheet.iter_range(body) {
        let Some(slot) = tally.get_mut(usize::from(at.col - body.left)) else {
            continue;
        };
        match cell.value.kind() {
            ValueKind::Number => slot[0] += 1,
            ValueKind::Text => slot[1] += 1,
            ValueKind::Bool => slot[2] += 1,
            ValueKind::Error => slot[3] += 1,
            ValueKind::Empty => {}
        }
    }

    let columns = (body.left..=body.right)
        .enumerate()
        .map(|(i, col)| {
            let [numbers, text, bools, errors] = tally[i];
            TableColumn {
                // `headers` is one entry per body column, left to right, so the
                // offset from the body's own left edge indexes it. A column
                // past the end of that list is a real column the region found
                // no header for.
                header: region.headers.get(i).cloned().unwrap_or_default(),
                range: RangeRef::new(body.sheet, body.top, col, body.bottom, col),
                kind: ColumnKind::of(numbers, text, bools, errors),
                populated: numbers + text + bools + errors,
            }
        })
        .collect();

    let labels = (region.header_cols > 0).then(|| {
        RangeRef::new(
            region.range.sheet,
            body.top,
            region.range.left,
            body.bottom,
            region.range.left + region.header_cols - 1,
        )
    });

    Some(Table {
        body,
        title: region.title.clone(),
        columns,
        labels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_model::{Cell, SheetId};

    use crate::region::{detect_regions, Region};

    /// `.` is empty, a bare number is numeric, `TRUE`/`FALSE` are booleans,
    /// anything else is text.
    fn grid(rows: &[&str]) -> Sheet {
        let mut sheet = Sheet::new(SheetId(0), "Sheet1");
        for (r, line) in rows.iter().enumerate() {
            for (c, tok) in line.split_whitespace().enumerate() {
                if tok == "." {
                    continue;
                }
                let value = match tok {
                    "TRUE" => CellValue::Bool(true),
                    "FALSE" => CellValue::Bool(false),
                    _ => match tok.parse::<f64>() {
                        Ok(n) => CellValue::Number(n),
                        Err(_) => CellValue::Text(tok.to_string()),
                    },
                };
                sheet.set(r as u32, c as u16, Cell::literal(value));
            }
        }
        sheet
    }

    fn only_table(sheet: &Sheet) -> Table {
        let regions = detect_regions(sheet);
        assert_eq!(
            regions.len(),
            1,
            "fixture should be one region: {regions:?}"
        );
        read_table(sheet, &regions[0]).expect("the region has a body")
    }

    #[test]
    fn a_table_carries_its_headers_and_the_cells_under_them() {
        let sheet = grid(&["Customer Debt Rate", "North 1200 8", "South 3400 11"]);
        let table = only_table(&sheet);

        let headers: Vec<&str> = table.columns.iter().map(|c| c.header.as_str()).collect();
        assert_eq!(headers, vec!["Debt", "Rate"], "the row labels are not data");
        assert_eq!(table.rows(), 2);
        // Column B, rows 2 and 3 — the body, with the header row excluded.
        assert_eq!(table.columns[0].range.top, 1);
        assert_eq!(table.columns[0].range.bottom, 2);
        assert_eq!(table.columns[0].range.left, 1);
    }

    #[test]
    fn a_block_with_no_header_still_reads_as_columns() {
        // Region detection needs a value-kind contrast to call a row a header,
        // and a grid that is text throughout gives it none. That is a block,
        // and its columns are real columns with nothing to call them — which a
        // caller must be able to tell from a column whose header is blank.
        let sheet = grid(&["North Residential 1200", "South Business 3400"]);
        let regions = detect_regions(&sheet);
        let table = read_table(&sheet, &regions[0]).unwrap();
        assert!(table.columns.iter().all(|c| c.header.is_empty()));
        assert_eq!(table.rows(), 2, "no header row is consumed");
    }

    #[test]
    fn a_column_takes_the_kind_the_majority_of_its_cells_have() {
        let sheet = grid(&[
            "Customer Debt Flag Note",
            "North 1200 TRUE ok",
            "South 3400 TRUE ok",
            "East 900 FALSE .",
        ]);
        let table = only_table(&sheet);
        let kind = |header: &str| table.column(header).unwrap().kind;

        assert_eq!(kind("Debt"), ColumnKind::Number);
        assert_eq!(kind("Flag"), ColumnKind::Bool);
        assert_eq!(kind("Note"), ColumnKind::Text);
        assert_eq!(
            table.column("Note").unwrap().populated,
            2,
            "the gap is not a cell"
        );
    }

    #[test]
    fn a_column_with_no_majority_is_mixed_rather_than_guessed_at() {
        // Half numbers and half "n/a" is the ordinary shape of a hand-kept
        // column. Summing it and reading it as text are both wrong, so it says
        // so instead of picking whichever kind was counted first.
        //
        // The region is built by hand: that ragged column is exactly what stops
        // detection seeing a header row, and this is a test about reading a
        // table rather than about finding one.
        let sheet = grid(&[
            "Customer Rate",
            "North 1",
            "South n/a",
            "East 3",
            "West n/a",
        ]);
        let region = Region {
            range: RangeRef::new(SheetId(0), 0, 0, 4, 1),
            kind: crate::region::RegionKind::Table,
            source: crate::region::RegionSource::Declared,
            title: None,
            header_rows: 1,
            header_cols: 1,
            headers: vec!["Rate".to_string()],
            cell_count: 9,
            totals_rows: 0,
        };
        let table = read_table(&sheet, &region).unwrap();
        assert_eq!(table.column("Rate").unwrap().kind, ColumnKind::Mixed);
        assert_eq!(table.column("Rate").unwrap().populated, 4);
    }

    #[test]
    fn a_row_keeps_its_columns_aligned_across_a_gap() {
        // A sheet holds only its populated cells, so the middle column of the
        // second row is absent rather than blank. A reader that skipped it
        // would shift every column after the gap by one for that row only.
        let sheet = grid(&["Customer A B C", "North 1 2 3", "South 4 . 6"]);
        let table = only_table(&sheet);
        let rows: Vec<Vec<CellValue>> = table.read(&sheet).collect();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].len(), 3, "three columns, gap included");
        assert_eq!(rows[1][1], CellValue::Empty);
        assert_eq!(rows[1][2], CellValue::Number(6.0), "still the third column");
    }

    #[test]
    fn the_row_labels_are_kept_apart_from_the_data() {
        let sheet = grid(&["Customer Debt", "North 1200", "South 3400"]);
        let table = only_table(&sheet);
        let labels = table.labels.expect("column A labels the rows");
        assert_eq!(labels.left, 0);
        assert_eq!(
            table.label_of(&sheet, 1),
            Some(CellValue::Text("North".into()))
        );
        assert!(
            !table.columns.iter().any(|c| c.header == "Customer"),
            "and are not a column of it"
        );
    }

    #[test]
    fn two_columns_with_one_header_are_refused_rather_than_picked_between() {
        // Real workbooks do this: `Total` under both `Q3` and `Q4`. Answering
        // with either is a coin toss.
        let sheet = grid(&["Region Total Total", "North 1 2", "South 3 4"]);
        let table = only_table(&sheet);
        assert!(table.column("Total").is_none());
        assert_eq!(table.columns.len(), 2);
    }
}
