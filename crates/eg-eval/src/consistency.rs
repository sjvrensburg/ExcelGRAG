//! Looking through a workbook's formulas for the ones that stopped matching.
//!
//! The faults a spreadsheet actually carries are ordinary ones: a value typed
//! over one cell of a filled-down column, a figure typed into some rows'
//! formulas where the rest read a parameter cell, a formula edited by hand and
//! never filled again. [`recompute`](crate::recompute) answers whether *one*
//! formula still agrees with its stored value; nothing before this answered
//! whether a column's formulas still agree with *each other*.
//!
//! The unit is a **column of a table**. Its formulas are normalised to an R1C1
//! shape — `B5*0.05` on row 5 and `B6*0.05` on row 6 are the same shape — and
//! the shape most of the column has is what it is "filled with". Everything
//! else is reported against that:
//!
//! - cells whose formula has another shape,
//! - cells that hold a typed value where the column has formulas,
//! - formulas whose stored value is an error.
//!
//! What this says is *differs*, not *is wrong*. A running balance seeds its
//! first row with a literal, a section's subtotal row is not its detail rows;
//! those are reported because a reader would want to look, and the report names
//! the cells so that looking is one `read_cells` away.
//!
//! Three things are deliberately not flagged:
//!
//! - **The heading row.** The table's own header rows are excluded by the
//!   region, never counted as typed text in every formula column.
//! - **A totals row.** The last row of a table, holding an aggregate where the
//!   column above holds something else, or a text label, is what a totals row
//!   is. Counted, and said so, but not listed as a defect.
//! - **A column with no common fill.** A financial model fills across, not
//!   down; its columns hold a different formula on every row, and listing each
//!   one as an exception would bury the column that really broke. Such a column
//!   is reported as having no common fill, and still has its errors counted.
//!
//! Not covered: a formula that is wrong *in agreement with its neighbours*
//! (every row's `SUM` stops a row short), and a pattern that runs across a row
//! rather than down a column.
//!
//! One row-major pass per table, because a sheet is an ordered map keyed by
//! (row, column) and asking for a single column costs a probe per row. A column
//! is kept as runs of consecutive rows sharing a shape, so a filled-down column
//! of hundreds of thousands of rows is one run and memory follows how often the
//! column *changes*, not how long it is.

use eg_model::formula::write_r1c1_shape;
use eg_model::{CellRef, CellValue, RangeRef, Sheet, SheetId, Workbook};
use eg_structure::{detect_regions, Region};
use rustc_hash::FxHashMap;

/// How much of a workbook to look through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every sheet.
    Workbook,
    /// One sheet.
    Sheet(SheetId),
    /// The columns of the tables a range overlaps, clipped to the range.
    Range(RangeRef),
}

/// What to keep of a column's findings.
#[derive(Debug, Clone, Copy)]
pub struct AuditOptions {
    /// Most cells to name per kind of finding, per column. The counts are
    /// exact whatever this is.
    pub examples: usize,
}

impl Default for AuditOptions {
    fn default() -> Self {
        AuditOptions { examples: 10 }
    }
}

/// What the column is filled with, and how far.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    /// The first cell that has it, and its formula in A1, as an example of the
    /// pattern. The rest are this one, moved.
    pub at: CellRef,
    pub formula: String,
    /// How many cells have it.
    pub cells: u64,
    /// First to last row that has it. Wider than `cells` when something else
    /// sits inside.
    pub span: RangeRef,
}

/// A formula that is not the column's.
#[derive(Debug, Clone, PartialEq)]
pub struct Deviant {
    pub cell: CellRef,
    pub formula: String,
}

/// A typed value where the column has formulas.
#[derive(Debug, Clone, PartialEq)]
pub struct Typed {
    pub cell: CellRef,
    pub value: CellValue,
}

/// What one column of formulas turned out to hold.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnAudit {
    /// The column's cells in the table's body, within the scope.
    pub column: RangeRef,
    pub header: Option<String>,
    /// Formula cells in the column.
    pub formulas: u64,
    /// The formula most of the column has, or `None` when no formula covers
    /// more than half of them.
    pub fill: Option<Fill>,
    /// Distinct shapes among the formulas.
    pub shapes: u64,
    /// Formulas with another shape than the fill. Always 0 without one.
    pub deviant_count: u64,
    pub deviants: Vec<Deviant>,
    pub typed_count: u64,
    pub typed: Vec<Typed>,
    /// Formulas whose stored value is an error.
    pub errors: u64,
    pub error_cells: Vec<CellRef>,
    /// Cells of the table's last row left out as a totals row.
    pub totals_row: u64,
}

impl ColumnAudit {
    /// Whether anything in the column wants a look.
    pub fn is_broken(&self) -> bool {
        self.deviant_count > 0 || self.typed_count > 0 || self.errors > 0
    }
}

/// The audit of a scope.
#[derive(Debug, Clone, PartialEq)]
pub struct Audit {
    /// Tables whose columns were examined.
    pub tables: u64,
    /// Every column that holds formulas, clean ones included, in workbook order
    /// — sheet, then position on it.
    pub columns: Vec<ColumnAudit>,
}

impl Audit {
    pub fn broken(&self) -> impl Iterator<Item = &ColumnAudit> {
        self.columns.iter().filter(|c| c.is_broken())
    }

    pub fn formulas(&self) -> u64 {
        self.columns.iter().map(|c| c.formulas).sum()
    }
}

/// Look through a scope of a workbook. See the module documentation.
pub fn audit(workbook: &Workbook, scope: Scope, opts: &AuditOptions) -> Audit {
    let mut out = Audit {
        tables: 0,
        columns: Vec::new(),
    };
    for sheet in &workbook.sheets {
        let clip = match scope {
            Scope::Workbook => None,
            Scope::Sheet(id) if id == sheet.id => None,
            // Clipped to what the sheet uses: a whole-column range costs a
            // probe per row of its height, a million of them, to find the
            // handful of cells that are there.
            Scope::Range(range) if range.sheet == sheet.id => {
                match sheet
                    .used_range()
                    .and_then(|used| range.intersection(&used))
                {
                    Some(range) => Some(range),
                    None => continue,
                }
            }
            _ => continue,
        };
        audit_sheet(sheet, clip, opts, &mut out);
    }
    out
}

/// A table body, as the pass sees it.
struct Body<'a> {
    /// What to read.
    rect: RangeRef,
    /// The table's last row, whether or not `rect` was clipped short of it.
    last_row: u32,
    region: Option<&'a Region>,
}

fn audit_sheet(sheet: &Sheet, clip: Option<RangeRef>, opts: &AuditOptions, out: &mut Audit) {
    let regions = detect_regions(sheet);
    let mut bodies: Vec<Body> = Vec::new();
    for region in &regions {
        // The table's own columns, label columns included: a key can be a
        // formula too. Header rows and a declared totals row are not body.
        let top = region.range.top + region.title_rows() + region.header_rows;
        let bottom = region.range.bottom.saturating_sub(region.totals_rows);
        if top > bottom {
            continue;
        }
        let whole = RangeRef::new(sheet.id, top, region.range.left, bottom, region.range.right);
        let rect = match clip {
            Some(clip) => match whole.intersection(&clip) {
                Some(rect) => rect,
                None => continue,
            },
            None => whole,
        };
        bodies.push(Body {
            rect,
            last_row: bottom,
            region: Some(region),
        });
    }
    // A range that overlaps no table is still a range somebody asked about.
    if bodies.is_empty() {
        if let Some(clip) = clip {
            bodies.push(Body {
                rect: clip,
                last_row: clip.bottom,
                region: None,
            });
        }
    }
    for body in &bodies {
        let before = out.columns.len();
        audit_body(sheet, body, opts, &mut out.columns);
        if out.columns.len() > before {
            out.tables += 1;
        }
    }
}

/// What one column accumulates during the pass.
#[derive(Default)]
struct Column {
    ids: FxHashMap<String, u32>,
    /// Per shape: how many cells, and the first row that has it.
    shapes: Vec<(u64, u32)>,
    /// Consecutive rows sharing a shape: first row, last row, shape.
    runs: Vec<(u32, u32, u32)>,
    typed_count: u64,
    typed_rows: Vec<u32>,
    errors: u64,
    error_rows: Vec<u32>,
    totals_row: u64,
}

fn audit_body(sheet: &Sheet, body: &Body, opts: &AuditOptions, out: &mut Vec<ColumnAudit>) {
    let rect = body.rect;
    let width = (rect.right - rect.left) as usize + 1;
    let mut columns: Vec<Column> = (0..width).map(|_| Column::default()).collect();
    let mut shape = String::new();
    let mut scratch = Vec::new();

    for (at, cell) in sheet.iter_range(rect) {
        let column = &mut columns[(at.col - rect.left) as usize];
        match &cell.formula {
            Some(formula) => {
                write_r1c1_shape(formula, at, &mut shape, &mut scratch);
                let id = match column.ids.get(shape.as_str()) {
                    Some(&id) => id,
                    None => {
                        let id = column.shapes.len() as u32;
                        column.ids.insert(shape.clone(), id);
                        column.shapes.push((0, at.row));
                        id
                    }
                };
                column.shapes[id as usize].0 += 1;
                match column.runs.last_mut() {
                    Some(run) if run.2 == id && run.1 + 1 == at.row => run.1 = at.row,
                    _ => column.runs.push((at.row, at.row, id)),
                }
                if matches!(cell.value, CellValue::Error(_)) {
                    column.errors += 1;
                    if column.error_rows.len() < opts.examples {
                        column.error_rows.push(at.row);
                    }
                }
            }
            None if cell.is_vacant() => {}
            None => {
                // A label closing the table — `Total` — is not a value typed
                // over a formula.
                if at.row == body.last_row && matches!(cell.value, CellValue::Text(_)) {
                    column.totals_row += 1;
                    continue;
                }
                column.typed_count += 1;
                if column.typed_rows.len() < opts.examples {
                    column.typed_rows.push(at.row);
                }
            }
        }
    }

    for (offset, column) in columns.into_iter().enumerate() {
        let col = rect.left + offset as u16;
        let formulas: u64 = column.shapes.iter().map(|s| s.0).sum();
        // A column with a formula or two among typed values is a data column
        // with a total in it, not a column of formulas with something typed in.
        if formulas < 2 || formulas <= column.typed_count {
            continue;
        }
        out.push(audit_column(sheet, body, col, formulas, column, opts));
    }
}

fn audit_column(
    sheet: &Sheet,
    body: &Body,
    col: u16,
    formulas: u64,
    column: Column,
    opts: &AuditOptions,
) -> ColumnAudit {
    let rect = body.rect;
    let at = |row: u32| CellRef::new(sheet.id, row, col);
    let formula_at = |row: u32| {
        sheet
            .get(row, col)
            .and_then(|c| c.formula.clone())
            .unwrap_or_default()
    };

    // The shape that more than half of the formulas have. Not merely the most
    // common one: among four different formulas the most common is a plurality
    // of one, and calling the other three deviations from it would be saying
    // something about the column that is not true.
    let dominant = column
        .shapes
        .iter()
        .enumerate()
        .max_by_key(|(i, (count, first))| (*count, std::cmp::Reverse((*first, *i))))
        .filter(|(_, (count, _))| count * 2 > formulas)
        .map(|(i, _)| i as u32);

    let mut totals_row = column.totals_row;
    let mut deviant_count = 0u64;
    let mut deviants = Vec::new();
    let fill = dominant.map(|id| {
        let (cells, first) = column.shapes[id as usize];
        let last = column
            .runs
            .iter()
            .rev()
            .find(|r| r.2 == id)
            .map(|r| r.1)
            .unwrap_or(first);
        Fill {
            at: at(first),
            formula: formula_at(first),
            cells,
            span: RangeRef::new(sheet.id, first, col, last, col),
        }
    });

    if let Some(dominant) = dominant {
        for &(start, end, id) in &column.runs {
            if id == dominant {
                continue;
            }
            // The last row of the table holding an aggregate is a totals row.
            let mut end = end;
            if end == body.last_row && is_aggregate(&formula_at(end)) {
                totals_row += 1;
                if end == start {
                    continue;
                }
                end -= 1;
            }
            deviant_count += u64::from(end - start) + 1;
            let mut row = start;
            while row <= end && deviants.len() < opts.examples {
                deviants.push(Deviant {
                    cell: at(row),
                    formula: formula_at(row),
                });
                row += 1;
            }
        }
    }

    let typed = column
        .typed_rows
        .iter()
        .map(|&row| Typed {
            cell: at(row),
            value: sheet.value(row, col),
        })
        .collect();

    let header = body.region.and_then(|region| {
        let offset = (col - region.range.left) as usize;
        let name = if offset < region.header_cols as usize {
            region.label_headers.get(offset)
        } else {
            region.headers.get(offset - region.header_cols as usize)
        };
        name.filter(|h| !h.is_empty()).cloned()
    });

    ColumnAudit {
        column: RangeRef::new(sheet.id, rect.top, col, rect.bottom, col),
        header,
        formulas,
        fill,
        shapes: column.shapes.len() as u64,
        deviant_count,
        deviants,
        typed_count: column.typed_count,
        typed,
        errors: column.errors,
        error_cells: column.error_rows.iter().map(|&row| at(row)).collect(),
        totals_row,
    }
}

/// Whether a formula is, at its top level, one of the functions a totals row is
/// made of.
fn is_aggregate(formula: &str) -> bool {
    let upper = formula.trim_start().to_ascii_uppercase();
    [
        "SUM(",
        "SUBTOTAL(",
        "AGGREGATE(",
        "AVERAGE(",
        "COUNT(",
        "COUNTA(",
        "MIN(",
        "MAX(",
    ]
    .iter()
    .any(|f| upper.starts_with(f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_model::{Cell, WorkbookFormat};

    fn formula(f: &str, value: CellValue) -> Cell {
        Cell {
            value,
            formula: Some(f.to_string()),
            format: Default::default(),
        }
    }

    /// A table `Item | Qty | Gross | Tax | Net` from row 4, rows 5..=14:
    /// `Tax = Gross*0.2`, `Net = Gross-Tax`, which is what a payroll sheet is.
    fn payroll() -> Sheet {
        let mut sheet = Sheet::new(SheetId(0), "January");
        for (c, h) in ["Item", "Qty", "Gross", "Tax", "Net"].iter().enumerate() {
            sheet.set(3, c as u16, Cell::literal(CellValue::Text((*h).into())));
        }
        for r in 4..14u32 {
            let n = r + 1;
            sheet.set(r, 0, Cell::literal(CellValue::Text(format!("item {r}"))));
            sheet.set(r, 1, Cell::literal(CellValue::Number(f64::from(r))));
            sheet.set(
                r,
                2,
                Cell::literal(CellValue::Number(1000.0 * f64::from(r))),
            );
            sheet.set(
                r,
                3,
                formula(
                    &format!("C{n}*0.2"),
                    CellValue::Number(200.0 * f64::from(r)),
                ),
            );
            sheet.set(
                r,
                4,
                formula(
                    &format!("C{n}-D{n}"),
                    CellValue::Number(800.0 * f64::from(r)),
                ),
            );
        }
        sheet
    }

    fn workbook(sheets: Vec<Sheet>) -> Workbook {
        Workbook {
            path: "book.xlsx".into(),
            format: Some(WorkbookFormat::Xlsx),
            content_hash: "hash".into(),
            sheets,
            defined_names: Vec::new(),
            external_links: Vec::new(),
        }
    }

    fn run(sheet: Sheet) -> Audit {
        audit(
            &workbook(vec![sheet]),
            Scope::Workbook,
            &AuditOptions::default(),
        )
    }

    #[test]
    fn a_clean_workbook_has_nothing_to_report() {
        let found = run(payroll());
        assert_eq!(found.tables, 1);
        assert_eq!(found.columns.len(), 2, "Tax and Net hold formulas");
        assert_eq!(found.broken().count(), 0);
        let tax = &found.columns[0];
        assert_eq!(tax.header.as_deref(), Some("Tax"));
        assert_eq!(tax.formulas, 10);
        let fill = tax.fill.as_ref().expect("a fill");
        assert_eq!(fill.formula, "C5*0.2");
        assert_eq!(fill.cells, 10);
        assert_eq!(fill.span.to_a1(), "D5:D14");
    }

    #[test]
    fn the_heading_row_is_not_a_typed_value() {
        // The fault another tool had: `Tax` over a column of formulas counted
        // as a typed value in every one of them.
        let found = run(payroll());
        assert!(found.columns.iter().all(|c| c.typed_count == 0));
    }

    #[test]
    fn a_value_typed_over_a_formula_is_found_with_the_value() {
        let mut sheet = payroll();
        sheet.set(8, 3, Cell::literal(CellValue::Number(1234.0)));
        let found = run(sheet);
        let tax = &found.columns[0];
        assert_eq!(tax.typed_count, 1);
        assert_eq!(tax.typed[0].cell.to_a1(), "D9");
        assert_eq!(tax.typed[0].value, CellValue::Number(1234.0));
        assert_eq!(tax.formulas, 9);
        assert_eq!(tax.deviant_count, 0);
        assert!(tax.is_broken());
        assert_eq!(found.broken().count(), 1, "Net is untouched");
    }

    #[test]
    fn a_figure_typed_into_one_formula_is_a_deviant() {
        let mut sheet = payroll();
        sheet.set(
            6,
            3,
            formula("C7*0.35", CellValue::Number(0.0)), // a rate typed in
        );
        let found = run(sheet);
        let tax = &found.columns[0];
        assert_eq!(tax.deviant_count, 1);
        assert_eq!(tax.deviants[0].cell.to_a1(), "D7");
        assert_eq!(tax.deviants[0].formula, "C7*0.35");
        assert_eq!(tax.fill.as_ref().unwrap().cells, 9);
        assert_eq!(tax.fill.as_ref().unwrap().formula, "C5*0.2");
    }

    #[test]
    fn the_fill_may_be_broken_at_either_end() {
        // `find_shape_exceptions` needs a neighbour on both sides; the first
        // and last row of a column are where hand edits usually are.
        let mut sheet = payroll();
        sheet.set(4, 3, formula("C5*0.9", CellValue::Number(0.0)));
        sheet.set(12, 3, formula("C13*0.9", CellValue::Number(0.0)));
        let found = run(sheet);
        let tax = &found.columns[0];
        let rows: Vec<String> = tax.deviants.iter().map(|d| d.cell.to_a1()).collect();
        assert_eq!(rows, ["D5", "D13"]);
        assert_eq!(tax.fill.as_ref().unwrap().at.to_a1(), "D6");
    }

    #[test]
    fn a_formula_whose_value_is_an_error_is_counted() {
        let mut sheet = payroll();
        sheet.set(
            7,
            4,
            formula("C8-D8", CellValue::Error(eg_model::ErrorKind::Div0)),
        );
        let found = run(sheet);
        let net = &found.columns[1];
        assert_eq!(net.errors, 1);
        assert_eq!(net.error_cells[0].to_a1(), "E8");
        assert_eq!(net.deviant_count, 0, "same formula, bad value");
        assert!(net.is_broken());
    }

    #[test]
    fn a_totals_row_is_not_a_defect() {
        let mut sheet = payroll();
        sheet.set(14, 0, Cell::literal(CellValue::Text("Total".into())));
        sheet.set(14, 3, formula("SUM(D5:D14)", CellValue::Number(0.0)));
        sheet.set(14, 4, formula("SUM(E5:E14)", CellValue::Number(0.0)));
        let found = run(sheet);
        assert_eq!(found.broken().count(), 0, "{:#?}", found.columns);
        assert_eq!(found.columns[0].totals_row, 1);
    }

    #[test]
    fn a_column_with_no_common_fill_is_not_all_exceptions() {
        // A model filled across: a different formula on every row.
        let mut sheet = Sheet::new(SheetId(0), "Model");
        for (c, h) in ["Line", "Base", "Next"].iter().enumerate() {
            sheet.set(0, c as u16, Cell::literal(CellValue::Text((*h).into())));
        }
        for r in 1..=6u32 {
            sheet.set(r, 0, Cell::literal(CellValue::Text(format!("line {r}"))));
            sheet.set(r, 1, Cell::literal(CellValue::Number(f64::from(r))));
        }
        sheet.set(1, 2, formula("B2*2", CellValue::Number(0.0)));
        sheet.set(2, 2, formula("B3+B2", CellValue::Number(0.0)));
        sheet.set(3, 2, formula("SUM(B2:B4)", CellValue::Number(0.0)));
        sheet.set(4, 2, formula("B5/3", CellValue::Number(0.0)));
        let found = run(sheet);
        let next = &found.columns[0];
        assert_eq!(next.fill, None);
        assert_eq!(next.shapes, 4);
        assert_eq!(next.deviant_count, 0);
        assert!(!next.is_broken());
    }

    #[test]
    fn a_data_column_with_a_formula_in_it_is_not_a_formula_column() {
        let mut sheet = payroll();
        // Gross holds typed numbers; one formula among them is not a column
        // of formulas with nine typed values in it.
        sheet.set(13, 2, formula("C5+C6", CellValue::Number(0.0)));
        let found = run(sheet);
        assert!(found
            .columns
            .iter()
            .all(|c| c.header.as_deref() != Some("Gross")));
    }

    #[test]
    fn a_range_looks_only_at_the_columns_it_covers() {
        let mut sheet = payroll();
        sheet.set(8, 3, Cell::literal(CellValue::Number(1.0)));
        sheet.set(8, 4, Cell::literal(CellValue::Number(2.0)));
        let wb = workbook(vec![sheet]);
        let opts = AuditOptions::default();

        let net_only = RangeRef::new(SheetId(0), 4, 4, 13, 4);
        let found = audit(&wb, Scope::Range(net_only), &opts);
        assert_eq!(found.columns.len(), 1);
        assert_eq!(found.columns[0].header.as_deref(), Some("Net"));
        assert_eq!(found.columns[0].typed_count, 1);

        let none = RangeRef::new(SheetId(0), 40, 0, 50, 3);
        let found = audit(&wb, Scope::Range(none), &opts);
        assert!(found.columns.is_empty());
    }

    #[test]
    fn a_sheet_scope_leaves_the_others_alone() {
        let mut broken = payroll();
        broken.set(8, 3, Cell::literal(CellValue::Number(1.0)));
        let mut other = payroll();
        other.id = SheetId(1);
        other.name = "February".into();
        // Re-key the cells onto the new sheet id.
        let other = {
            let mut moved = Sheet::new(SheetId(1), "February");
            for (at, cell) in other.iter() {
                moved.set(at.row, at.col, cell.clone());
            }
            moved
        };
        let wb = workbook(vec![broken, other]);
        let opts = AuditOptions::default();

        assert_eq!(audit(&wb, Scope::Workbook, &opts).broken().count(), 1);
        assert_eq!(
            audit(&wb, Scope::Sheet(SheetId(1)), &opts).broken().count(),
            0
        );
        assert_eq!(
            audit(&wb, Scope::Sheet(SheetId(0)), &opts).broken().count(),
            1
        );
    }

    #[test]
    fn counts_are_exact_when_examples_are_capped() {
        let mut sheet = payroll();
        for r in 4..8u32 {
            sheet.set(r, 3, Cell::literal(CellValue::Number(f64::from(r))));
        }
        let wb = workbook(vec![sheet]);
        let found = audit(&wb, Scope::Workbook, &AuditOptions { examples: 3 });
        let tax = &found.columns[0];
        assert_eq!(tax.typed_count, 4);
        assert_eq!(tax.typed.len(), 3, "named up to the cap");
        assert_eq!(tax.formulas, 6);
    }
}
