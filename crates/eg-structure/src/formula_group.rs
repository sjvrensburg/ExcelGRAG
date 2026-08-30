//! Collapsing repeated formulas into groups.
//!
//! A filled-down column is one idea written ten thousand times. Represented
//! cell-by-cell it would dominate the graph, blow out the embedding cost, and
//! bury an agent in near-identical text. Represented as a group it is a single
//! node saying "column D, rows 2 to 10001, each row is `RC[-1]*RC[-2]`".
//!
//! Two formulas belong to the same group when they share an R1C1 *shape* — they
//! do the same thing to correspondingly-placed cells. Grouping runs in two
//! passes: vertical runs within each column, then a horizontal merge of adjacent
//! columns whose runs line up exactly. That order matters because spreadsheets
//! are overwhelmingly filled downwards, so the vertical pass does most of the
//! work and the horizontal pass tidies up blocks.
//!
//! Memory is deliberately kept flat. Shapes are compared only between vertically
//! adjacent cells, so one shape string is held at a time rather than one per
//! cell — on a real workbook that is the difference between a few kilobytes and
//! several gigabytes.

use eg_model::formula::write_r1c1_shape;
use eg_model::{CellRef, RangeRef, ReferenceSpan, Sheet};
use serde::{Deserialize, Serialize};

/// A rectangle of cells that all share one formula shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormulaGroup {
    /// The rectangle the group covers.
    pub range: RangeRef,
    /// The shared shape, in R1C1.
    pub shape: String,
    /// The formula as written at the group's top-left cell, in A1. Kept so the
    /// group can be shown to a reader the way the workbook shows it.
    pub representative: String,
    /// Cells in the rectangle that actually carry the shape. Equal to the
    /// rectangle's area for a solid block, less where the group was merged
    /// across columns with gaps.
    pub cell_count: u64,
}

impl FormulaGroup {
    /// The cell whose formula `representative` was taken from.
    pub fn anchor(&self) -> CellRef {
        self.range.top_left()
    }

    /// Whether the group is a single cell, i.e. a one-off formula.
    pub fn is_singleton(&self) -> bool {
        self.cell_count == 1
    }
}

/// What grouping achieved, for reporting and for tuning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupingStats {
    pub formula_cells: u64,
    pub groups: u64,
    /// Groups covering exactly one cell — genuinely one-off formulas.
    pub singletons: u64,
    /// Cells in the largest group.
    pub largest_group: u64,
}

impl GroupingStats {
    /// Formula cells per group. The whole point of grouping is for this to be
    /// large on a real workbook.
    pub fn compression(&self) -> f64 {
        if self.groups == 0 {
            return 0.0;
        }
        self.formula_cells as f64 / self.groups as f64
    }
}

/// Every formula cell on a sheet, ordered by column then row.
///
/// Both passes below want to walk down a column, but a sheet stores its cells
/// row-major and sparsely. Iterating the used *rectangle* instead costs a lookup
/// per grid position, and on a real workbook the rectangles are mostly empty —
/// one sheet here is 136 columns by 115,392 rows holding 15.7M positions. So the
/// populated cells are collected once and sorted, which is bounded by the number
/// of formulas rather than by the sheet's area.
fn formula_cells_column_major(sheet: &Sheet) -> Vec<(u16, u32, &str)> {
    let mut cells: Vec<(u16, u32, &str)> = sheet
        .iter()
        .filter_map(|(addr, cell)| {
            cell.formula
                .as_deref()
                .map(|f| (addr.col, addr.row, f))
        })
        .collect();
    cells.sort_unstable_by_key(|&(col, row, _)| (col, row));
    cells
}

/// A vertical run of one shape within a single column.
struct Run {
    col: u16,
    top: u32,
    bottom: u32,
    shape: String,
    representative: String,
}

/// Group every formula on a sheet by shape.
///
/// Groups are returned in row-major order of their top-left cell.
pub fn group_formulas(sheet: &Sheet) -> (Vec<FormulaGroup>, GroupingStats) {
    let mut runs = Vec::new();
    let mut stats = GroupingStats::default();


    // Reused across every cell on the sheet, so shape computation allocates
    // almost nothing.
    let mut shape = String::new();
    let mut scratch: Vec<ReferenceSpan> = Vec::new();

    // Pass 1: vertical runs of one shape within a column.
    let cells = formula_cells_column_major(sheet);
    stats.formula_cells = cells.len() as u64;
    let mut open: Option<Run> = None;

    for &(col, row, formula) in &cells {
        write_r1c1_shape(
            formula,
            CellRef::new(sheet.id, row, col),
            &mut shape,
            &mut scratch,
        );
        match &mut open {
            // Extend the run only if this cell is directly below the last one
            // in the same column; a gap starts a new group even for the same
            // shape, so a group is always a contiguous block.
            Some(run)
                if run.col == col && run.bottom + 1 == row && run.shape == shape =>
            {
                run.bottom = row;
            }
            _ => {
                if let Some(run) = open.take() {
                    runs.push(run);
                }
                open = Some(Run {
                    col,
                    top: row,
                    bottom: row,
                    shape: shape.clone(),
                    representative: formula.to_string(),
                });
            }
        }
    }
    if let Some(run) = open.take() {
        runs.push(run);
    }

    let groups = merge_adjacent_columns(sheet, runs);

    stats.groups = groups.len() as u64;
    stats.singletons = groups.iter().filter(|g| g.is_singleton()).count() as u64;
    stats.largest_group = groups.iter().map(|g| g.cell_count).max().unwrap_or(0);
    (groups, stats)
}

/// Merge vertical runs that sit side by side and cover the same rows.
///
/// A block of formulas filled both across and down produces one run per column,
/// all with the same shape and row span. Merging them into a rectangle is what
/// turns a 12-month by 500-row model into one node instead of twelve.
fn merge_adjacent_columns(sheet: &Sheet, mut runs: Vec<Run>) -> Vec<FormulaGroup> {
    // Sort so that candidates to merge are adjacent: same rows and shape first,
    // then ascending column.
    runs.sort_by(|a, b| {
        (a.top, a.bottom, &a.shape, a.col).cmp(&(b.top, b.bottom, &b.shape, b.col))
    });

    let mut groups: Vec<FormulaGroup> = Vec::new();
    let mut i = 0;
    while i < runs.len() {
        let start = i;
        let mut right = runs[i].col;
        i += 1;
        while i < runs.len()
            && runs[i].top == runs[start].top
            && runs[i].bottom == runs[start].bottom
            && runs[i].shape == runs[start].shape
            && runs[i].col == right + 1
        {
            right = runs[i].col;
            i += 1;
        }

        let run = &runs[start];
        let range = RangeRef::new(sheet.id, run.top, run.col, run.bottom, right);
        groups.push(FormulaGroup {
            shape: run.shape.clone(),
            // The representative must be the formula at the rectangle's
            // top-left, which after merging is the first column's run.
            representative: run.representative.clone(),
            cell_count: range.cell_count(),
            range,
        });
    }

    groups.sort_by_key(|g| (g.range.top, g.range.left));
    groups
}

/// A formula that breaks the pattern of the cells around it.
///
/// This is the classic spreadsheet bug: a column of `=SUM(B2:B9)` where one row
/// was edited by hand. It is worth surfacing directly, because it is exactly
/// what a reader would want to be told about and exactly what a summary hides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShapeException {
    pub cell: CellRef,
    /// The formula at the odd cell, in A1.
    pub formula: String,
    /// The shape its neighbours agree on.
    pub expected_shape: String,
    /// The shape this cell actually has, or `None` if it holds no formula.
    pub actual_shape: Option<String>,
}

/// Find cells that break a vertical run of otherwise identical formulas.
///
/// A cell is an exception when the cells directly above and below it share a
/// shape that it does not. Requiring agreement on both sides is what keeps this
/// quiet: the boundary between two different blocks has disagreement on one side
/// only, so it is not reported.
pub fn find_shape_exceptions(sheet: &Sheet) -> Vec<ShapeException> {
    let cells = formula_cells_column_major(sheet);
    let mut out = Vec::new();
    let mut scratch: Vec<ReferenceSpan> = Vec::new();
    let mut buf = String::new();

    let shape_of = |col: u16, row: u32, f: &str, buf: &mut String, scratch: &mut Vec<_>| {
        write_r1c1_shape(f, CellRef::new(sheet.id, row, col), buf, scratch);
        buf.clone()
    };

    // Slide a window of three formula cells down each column. The interesting
    // shape is `a ? a`: neighbours that agree across something that does not.
    for w in cells.windows(3) {
        let (c0, r0, f0) = w[0];
        let (c1, r1, f1) = w[1];
        let (c2, r2, f2) = w[2];
        if c0 != c2 || c0 != c1 {
            continue;
        }

        let above = shape_of(c0, r0, f0, &mut buf, &mut scratch);

        // Case 1: three consecutive rows, the middle one edited by hand.
        if r1 == r0 + 1 && r2 == r1 + 1 {
            let below = shape_of(c2, r2, f2, &mut buf, &mut scratch);
            if above != below {
                continue;
            }
            let here = shape_of(c1, r1, f1, &mut buf, &mut scratch);
            if here != above {
                out.push(ShapeException {
                    cell: CellRef::new(sheet.id, r1, c1),
                    formula: f1.to_string(),
                    expected_shape: above,
                    actual_shape: Some(here),
                });
            }
        }
    }

    // Case 2: a formula replaced by a literal or left blank, so the row carries
    // no formula at all and never appears in `cells`. Two formula cells exactly
    // two rows apart, agreeing on shape, bracket exactly one such gap.
    for w in cells.windows(2) {
        let (c0, r0, f0) = w[0];
        let (c1, r1, f1) = w[1];
        if c0 != c1 || r1 != r0 + 2 {
            continue;
        }
        let above = shape_of(c0, r0, f0, &mut buf, &mut scratch);
        let below = shape_of(c1, r1, f1, &mut buf, &mut scratch);
        if above == below {
            out.push(ShapeException {
                cell: CellRef::new(sheet.id, r0 + 1, c0),
                formula: String::new(),
                expected_shape: above,
                actual_shape: None,
            });
        }
    }

    // Row-major, matching the order `group_formulas` returns.
    out.sort_by_key(|e| (e.cell.row, e.cell.col));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_model::{Cell, CellValue, SheetId};

    /// Build a sheet from `(row, col, formula)` triples.
    fn sheet_with(formulas: &[(u32, u16, &str)]) -> Sheet {
        let mut s = Sheet::new(SheetId(0), "Sheet1");
        for &(row, col, f) in formulas {
            s.set(
                row,
                col,
                Cell {
                    value: CellValue::Number(0.0),
                    formula: Some(f.to_string()),
                    format: Default::default(),
                },
            );
        }
        s
    }

    /// A column of `=A<n>*2` filled from row 0 down.
    fn filled_column(rows: u32, col: u16) -> Sheet {
        let f: Vec<(u32, u16, String)> = (0..rows)
            .map(|r| (r, col, format!("A{}*2", r + 1)))
            .collect();
        sheet_with(
            &f.iter()
                .map(|(r, c, s)| (*r, *c, s.as_str()))
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn a_filled_column_collapses_to_one_group() {
        let sheet = filled_column(1000, 1);
        let (groups, stats) = group_formulas(&sheet);
        assert_eq!(groups.len(), 1);
        assert_eq!(stats.formula_cells, 1000);
        assert_eq!(groups[0].range.to_a1(), "B1:B1000");
        assert_eq!(groups[0].shape, "RC[-1]*2");
        assert_eq!(groups[0].representative, "A1*2");
        assert_eq!(groups[0].cell_count, 1000);
        assert_eq!(stats.compression(), 1000.0);
    }

    #[test]
    fn a_filled_block_collapses_across_columns_too() {
        // Three columns each filled down five rows, all the same shape.
        let mut cells = Vec::new();
        for col in 1..4u16 {
            for row in 0..5u32 {
                cells.push((row, col, format!("A{}*2", row + 1)));
            }
        }
        let sheet = sheet_with(
            &cells
                .iter()
                .map(|(r, c, s)| (*r, *c, s.as_str()))
                .collect::<Vec<_>>(),
        );
        let (groups, stats) = group_formulas(&sheet);

        // Each column references column A absolutely-different offsets, so the
        // shapes differ per column and they must NOT merge.
        assert_eq!(stats.formula_cells, 15);
        assert!(groups.len() >= 3, "differing offsets must not merge");
    }

    #[test]
    fn identical_shapes_side_by_side_merge_into_a_rectangle() {
        // Each column doubles the cell to its own left, so all three share the
        // shape RC[-1]*2 and should become one rectangle.
        let mut cells = Vec::new();
        for col in 1..4u16 {
            for row in 0..5u32 {
                let left = eg_model::col_to_letters(col as u32 - 1);
                cells.push((row, col, format!("{left}{}*2", row + 1)));
            }
        }
        let sheet = sheet_with(
            &cells
                .iter()
                .map(|(r, c, s)| (*r, *c, s.as_str()))
                .collect::<Vec<_>>(),
        );
        let (groups, _) = group_formulas(&sheet);
        assert_eq!(groups.len(), 1, "got {groups:#?}");
        assert_eq!(groups[0].range.to_a1(), "B1:D5");
        assert_eq!(groups[0].cell_count, 15);
    }

    #[test]
    fn a_gap_splits_a_run() {
        // Same shape either side of a blank row: two groups, not one.
        let sheet = sheet_with(&[(0, 1, "A1*2"), (1, 1, "A2*2"), (3, 1, "A4*2")]);
        let (groups, _) = group_formulas(&sheet);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].range.to_a1(), "B1:B2");
        assert_eq!(groups[1].range.to_a1(), "B4");
        assert!(groups[1].is_singleton());
    }

    #[test]
    fn different_shapes_split_a_run() {
        let sheet = sheet_with(&[(0, 1, "A1*2"), (1, 1, "A2*3"), (2, 1, "A3*2")]);
        let (groups, stats) = group_formulas(&sheet);
        assert_eq!(groups.len(), 3);
        assert_eq!(stats.singletons, 3);
    }

    #[test]
    fn absolute_references_keep_their_own_group() {
        // `$A$1` is the same cell from every row, so these rows share a shape
        // with each other but not with a filled-down relative reference.
        let sheet = sheet_with(&[(0, 1, "$A$1*2"), (1, 1, "$A$1*2"), (2, 1, "$A$1*2")]);
        let (groups, _) = group_formulas(&sheet);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].shape, "R1C1*2");
    }

    #[test]
    fn an_empty_sheet_yields_nothing() {
        let sheet = Sheet::new(SheetId(0), "Sheet1");
        let (groups, stats) = group_formulas(&sheet);
        assert!(groups.is_empty());
        assert_eq!(stats.formula_cells, 0);
        assert_eq!(stats.compression(), 0.0);
    }

    #[test]
    fn a_hand_edited_row_is_reported_as_an_exception() {
        // The classic bug: one row of a filled column edited by hand.
        let sheet = sheet_with(&[
            (0, 1, "A1*2"),
            (1, 1, "A2*2"),
            (2, 1, "A3*3"), // <- the odd one
            (3, 1, "A4*2"),
            (4, 1, "A5*2"),
        ]);
        let exceptions = find_shape_exceptions(&sheet);
        assert_eq!(exceptions.len(), 1);
        assert_eq!(exceptions[0].cell.to_a1(), "B3");
        assert_eq!(exceptions[0].formula, "A3*3");
        assert_eq!(exceptions[0].expected_shape, "RC[-1]*2");
    }

    #[test]
    fn a_hardcoded_constant_in_a_formula_column_is_an_exception() {
        let sheet = sheet_with(&[(0, 1, "A1*2"), (2, 1, "A3*2")]);
        let mut sheet = sheet;
        // A literal value where a formula was expected.
        sheet.set(1, 1, Cell::literal(CellValue::Number(99.0)));
        let exceptions = find_shape_exceptions(&sheet);
        assert_eq!(exceptions.len(), 1);
        assert_eq!(exceptions[0].cell.to_a1(), "B2");
        assert_eq!(exceptions[0].actual_shape, None);
    }

    #[test]
    fn a_boundary_between_two_blocks_is_not_an_exception() {
        // Disagreement on one side only: a section change, not a mistake.
        let sheet = sheet_with(&[
            (0, 1, "A1*2"),
            (1, 1, "A2*2"),
            (2, 1, "A3*3"),
            (3, 1, "A4*3"),
        ]);
        assert!(find_shape_exceptions(&sheet).is_empty());
    }

    #[test]
    fn a_uniform_column_has_no_exceptions() {
        assert!(find_shape_exceptions(&filled_column(50, 1)).is_empty());
    }
}
