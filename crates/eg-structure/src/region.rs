//! Finding the tables and blocks a sheet is actually made of.
//!
//! A sheet is a grid, but people build it as tables, side notes, and stacked
//! blocks with blank rows between them. Recovering that layout is what lets a
//! retrieved answer say "the Revenue column of the Q3 Sales table" instead of
//! "some cells near D5", and it is the single biggest lever on retrieval quality.
//!
//! # Working without styling
//!
//! The obvious signals — bold headers, fills, borders — are not available:
//! calamine exposes no cell styling for *any* format. Depending on them would
//! also have meant XLSB, the format this project exists for, degrading to
//! guesswork. So detection uses only what every format supplies:
//!
//! - **Blank rows and columns**, which is how people visually separate blocks.
//! - **Value kinds**, since a text row above numeric columns is a header.
//! - **Declared Excel tables**, which are ground truth where a format has them.
//!
//! # How it splits
//!
//! Recursive alternating projection: split the used area at blank rows, split
//! each band at blank columns, and repeat until neither axis splits any further.
//! This is cheap, needs no styling, and matches how sheets are actually laid
//! out. It deliberately splits a table containing a blank row into two regions —
//! for retrieval that is the better error, since each half is still coherent and
//! correctly cited, whereas merging unrelated blocks produces a region whose
//! header does not describe its rows.

use eg_model::{CellValue, RangeRef, Sheet, SheetId, ValueKind};
use serde::{Deserialize, Serialize};

/// How a region was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegionSource {
    /// The workbook declares an Excel table here. Not a guess.
    Declared,
    /// Inferred from layout.
    Detected,
}

/// What the region appears to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegionKind {
    /// A header row or column plus a body of data underneath it.
    Table,
    /// A rectangle of data with no header we could identify.
    Block,
    /// A thin strip of text: a title, a note, a caption.
    Note,
}

/// A detected region of a sheet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub range: RangeRef,
    pub kind: RegionKind,
    pub source: RegionSource,
    /// A lone text cell heading the region, such as a section title written
    /// above a block. Far better to retrieve on than a bare A1 range.
    pub title: Option<String>,
    /// Leading rows of `range` that are header, not data. Counted *after* any
    /// title row.
    pub header_rows: u32,
    /// Leading columns of `range` that label rows rather than hold data.
    pub header_cols: u16,
    /// One entry per body column, left to right. Multi-row headers are joined
    /// with `.`, giving paths like `Q3.Revenue`.
    pub headers: Vec<String>,
    /// Populated cells inside `range`.
    pub cell_count: u64,
}

impl Region {
    /// Rows of `range` consumed by the title.
    ///
    /// A detected title was read from the region's own first row, so it costs a
    /// row. A declared table's title is the table's *name*, which lives in the
    /// workbook rather than in a cell and costs nothing — counting it would push
    /// [`Region::body`] past the table's first data row.
    pub fn title_rows(&self) -> u32 {
        match self.source {
            RegionSource::Declared => 0,
            RegionSource::Detected => u32::from(self.title.is_some()),
        }
    }

    /// The data rows, excluding any title and header.
    pub fn body(&self) -> Option<RangeRef> {
        let top = self.range.top + self.title_rows() + self.header_rows;
        let left = self.range.left + self.header_cols;
        if top > self.range.bottom || left > self.range.right {
            return None;
        }
        Some(RangeRef::new(
            self.range.sheet,
            top,
            left,
            self.range.bottom,
            self.range.right,
        ))
    }

    /// Fraction of the rectangle that is actually populated.
    pub fn density(&self) -> f64 {
        let area = self.range.cell_count();
        if area == 0 {
            0.0
        } else {
            self.cell_count as f64 / area as f64
        }
    }

    pub fn has_header(&self) -> bool {
        self.header_rows > 0 || self.header_cols > 0
    }
}

/// Tuning for [`detect_regions`].
#[derive(Debug, Clone)]
pub struct RegionOptions {
    /// Blank rows needed to separate two blocks. One is the usual visual cue.
    pub row_gap: u32,
    /// Blank columns needed to separate two blocks.
    pub col_gap: u16,
    /// Most header rows to consider stacking.
    pub max_header_rows: u32,
    /// Fraction of columns that must look like a header for a row to be one.
    pub header_agreement: f64,
    /// Regions smaller than this are still emitted, but never called tables.
    pub min_table_rows: u32,
    /// Depth cap on the alternating split, as a guard against pathological input.
    pub max_depth: u8,
}

impl Default for RegionOptions {
    fn default() -> Self {
        Self {
            row_gap: 1,
            col_gap: 1,
            max_header_rows: 3,
            header_agreement: 0.5,
            min_table_rows: 2,
            max_depth: 12,
        }
    }
}

/// Detect the regions of a sheet.
///
/// Regions are returned in row-major order and never overlap.
pub fn detect_regions(sheet: &Sheet) -> Vec<Region> {
    detect_regions_with(sheet, &RegionOptions::default())
}

/// Detect the regions of a sheet with explicit options.
pub fn detect_regions_with(sheet: &Sheet, opts: &RegionOptions) -> Vec<Region> {
    let mut regions = Vec::new();

    // Declared tables are ground truth; where the workbook says a table is, we
    // do not guess. Their areas are then excluded from the heuristic pass.
    for table in &sheet.tables {
        let header_rows = u32::from(table.has_header_row);
        regions.push(Region {
            range: table.range,
            kind: RegionKind::Table,
            source: RegionSource::Declared,
            title: Some(table.name.clone()),
            header_rows,
            header_cols: 0,
            headers: table.columns.clone(),
            cell_count: sheet.iter_range(table.range).count() as u64,
        });
    }

    // The declared-table filter is a linear scan per cell, so it is skipped
    // entirely on the overwhelmingly common sheet that declares no tables.
    let mut cells: Vec<(u32, u16)> = if regions.is_empty() {
        sheet.iter().map(|(addr, _)| (addr.row, addr.col)).collect()
    } else {
        sheet
            .iter()
            .map(|(addr, _)| (addr.row, addr.col))
            .filter(|&(row, col)| {
                !regions
                    .iter()
                    .any(|r| r.range.contains(eg_model::CellRef::new(sheet.id, row, col)))
            })
            .collect()
    };

    let table_ranges: Vec<RangeRef> = sheet.tables.iter().map(|t| t.range).collect();
    let mut blocks = Vec::new();
    partition(
        &mut cells,
        &table_ranges,
        Axis::Row,
        0,
        0,
        opts,
        &mut blocks,
    );

    for (bounds, count) in blocks {
        let range = RangeRef::new(sheet.id, bounds.0, bounds.1, bounds.2, bounds.3);
        regions.push(describe(sheet, range, count, opts));
    }

    regions.sort_by_key(|r| (r.range.top, r.range.left));
    regions
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Row,
    Col,
}

impl Axis {
    fn other(self) -> Axis {
        match self {
            Axis::Row => Axis::Col,
            Axis::Col => Axis::Row,
        }
    }
}

/// Bounding box `(top, left, bottom, right)` plus the populated cell count.
type Block = ((u32, u16, u32, u16), u64);

/// Recursively split a set of cells at blank rows and columns.
///
/// `stalled` counts consecutive axes that produced no split; at two, both axes
/// have been tried against the current set and it is as small as this method can
/// make it.
fn partition(
    cells: &mut [(u32, u16)],
    tables: &[RangeRef],
    axis: Axis,
    stalled: u8,
    depth: u8,
    opts: &RegionOptions,
    out: &mut Vec<Block>,
) {
    if cells.is_empty() {
        return;
    }
    if stalled >= 2 || depth >= opts.max_depth {
        push_clear_of_tables(cells, tables, Axis::Row, 0, out);
        return;
    }

    // Order along the axis being split, then cut wherever the gap to the next
    // occupied line is wide enough to read as a separator.
    let cuts: Vec<usize> = match axis {
        Axis::Row => {
            cells.sort_unstable_by_key(|&(row, col)| (row, col));
            gap_cuts(
                cells.iter().map(|&(row, _)| row as u64),
                opts.row_gap as u64,
            )
        }
        Axis::Col => {
            cells.sort_unstable_by_key(|&(row, col)| (col, row));
            gap_cuts(
                cells.iter().map(|&(_, col)| col as u64),
                opts.col_gap as u64,
            )
        }
    };

    if cuts.is_empty() {
        partition(
            cells,
            tables,
            axis.other(),
            stalled + 1,
            depth + 1,
            opts,
            out,
        );
        return;
    }

    let mut rest = cells;
    let mut consumed = 0;
    for cut in cuts {
        let (part, tail) = rest.split_at_mut(cut - consumed);
        partition(part, tables, axis.other(), 0, depth + 1, opts, out);
        consumed = cut;
        rest = tail;
    }
    partition(rest, tables, axis.other(), 0, depth + 1, opts, out);
}

/// Push `cells` as blocks whose bounding boxes clear every declared table.
///
/// A detected block can *bound* a declared table without containing any of its
/// cells: the table's cells were filtered out before partitioning, but the cells
/// around it still span a rectangle that encloses it. Emitting that rectangle
/// would put the same cell in two regions and break the documented "regions
/// never overlap" invariant, so the block is cut at the table's own edges.
///
/// A cut always makes progress. If the row cut leaves the set whole then every
/// cell lies inside the table's rows; if the column cut then leaves it whole
/// too, every cell lies inside the table itself — impossible, since those cells
/// were removed. `stalled` guards the invariant rather than relying on it.
fn push_clear_of_tables(
    cells: &mut [(u32, u16)],
    tables: &[RangeRef],
    axis: Axis,
    stalled: u8,
    out: &mut Vec<Block>,
) {
    if cells.is_empty() {
        return;
    }
    let block = bounds(cells);
    let (top, left, bottom, right) = block.0;
    let clash = tables
        .iter()
        .find(|t| t.top <= bottom && t.bottom >= top && t.left <= right && t.right >= left);
    let (Some(table), true) = (clash, stalled < 2) else {
        out.push(block);
        return;
    };

    // Cut into the lines before the table, the lines it spans, and the lines
    // after it. Only the middle part can still overlap, but all three are
    // re-checked: a part may clash with a *different* table.
    let (first, second) = match axis {
        Axis::Row => {
            cells.sort_unstable_by_key(|&(row, col)| (row, col));
            (
                cells.partition_point(|&(row, _)| row < table.top),
                cells.partition_point(|&(row, _)| row <= table.bottom),
            )
        }
        Axis::Col => {
            cells.sort_unstable_by_key(|&(row, col)| (col, row));
            (
                cells.partition_point(|&(_, col)| col < table.left),
                cells.partition_point(|&(_, col)| col <= table.right),
            )
        }
    };

    let split = first > 0 || second < cells.len();
    let next_stalled = if split { 0 } else { stalled + 1 };
    let (head, tail) = cells.split_at_mut(second);
    let (head, middle) = head.split_at_mut(first);
    for part in [head, middle, tail] {
        push_clear_of_tables(part, tables, axis.other(), next_stalled, out);
    }
}

/// Indices where a sorted coordinate sequence jumps by more than `gap` blanks.
fn gap_cuts(values: impl Iterator<Item = u64>, gap: u64) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut prev: Option<u64> = None;
    for (i, v) in values.enumerate() {
        if let Some(p) = prev {
            // `v - p - 1` blank lines lie between the two occupied ones.
            if v - p > gap {
                cuts.push(i);
            }
        }
        prev = Some(v);
    }
    cuts
}

fn bounds(cells: &[(u32, u16)]) -> Block {
    let mut top = u32::MAX;
    let mut bottom = 0;
    let mut left = u16::MAX;
    let mut right = 0;
    for &(row, col) in cells {
        top = top.min(row);
        bottom = bottom.max(row);
        left = left.min(col);
        right = right.max(col);
    }
    ((top, left, bottom, right), cells.len() as u64)
}

/// Classify a block and work out its headers.
fn describe(sheet: &Sheet, range: RangeRef, cell_count: u64, opts: &RegionOptions) -> Region {
    // A title is read first, so header detection looks below it rather than at
    // it. Otherwise a block captioned "Impairment summary" hides its real
    // header row one line down.
    let title = detect_title(sheet, range);
    let after_title = RangeRef::new(
        range.sheet,
        range.top + u32::from(title.is_some()),
        range.left,
        range.bottom,
        range.right,
    );
    let header_rows = count_header_rows(sheet, after_title, opts);
    let header_cols = u16::from(has_row_labels(sheet, after_title, header_rows));

    let rows = range.rows();
    let cols = range.cols();

    let kind = if header_rows > 0 && rows > header_rows && rows >= opts.min_table_rows {
        RegionKind::Table
    } else if (rows <= 1 || cols == 1) && is_mostly_text(sheet, range) {
        // A single line of text either way: a title, a caption, a note.
        RegionKind::Note
    } else {
        RegionKind::Block
    };

    let headers = if header_rows > 0 {
        column_headers(sheet, after_title, header_rows, header_cols)
    } else {
        Vec::new()
    };

    Region {
        range,
        kind,
        source: RegionSource::Detected,
        title,
        header_rows,
        header_cols,
        headers,
        cell_count,
    }
}

/// A lone text cell on the region's first row, read as a section title.
///
/// Requiring the row to hold exactly one populated cell is what separates a
/// title from a header: a header labels every column, a title labels the block.
/// That test only discriminates when the block is more than one column wide —
/// in a single column a header row *is* one populated cell, so a title is not
/// claimed there and the text stays the column's header.
fn detect_title(sheet: &Sheet, range: RangeRef) -> Option<String> {
    if range.rows() < 2 || range.cols() < 2 {
        return None;
    }
    let mut found: Option<String> = None;
    for col in range.left..=range.right {
        match sheet.value(range.top, col) {
            CellValue::Empty => continue,
            CellValue::Text(t) if found.is_none() && !t.trim().is_empty() => {
                found = Some(t);
            }
            // A second populated cell means this row labels columns, not the
            // block; anything non-text means it is data.
            _ => return None,
        }
    }
    found
}

/// How many leading rows look like a header rather than data.
///
/// A row is a header when, across the columns that hold data below it, it is
/// text where the body is not. That contrast is the only signal available
/// without styling, and it is the one people actually rely on when reading.
fn count_header_rows(sheet: &Sheet, range: RangeRef, opts: &RegionOptions) -> u32 {
    let max = opts.max_header_rows.min(range.rows().saturating_sub(1));
    let mut header_rows = 0;

    while header_rows < max {
        let candidate = range.top + header_rows;
        let body_top = candidate + 1;
        if body_top > range.bottom {
            break;
        }

        let (mut looks_header, mut considered) = (0u32, 0u32);
        for col in range.left..=range.right {
            let body = dominant_kind(sheet, body_top, range.bottom, col);
            let Some(body) = body else { continue };
            considered += 1;
            let here = sheet.kind(candidate, col);
            // Text over non-text is a header. Text over text is not evidence
            // either way, so it neither counts for nor against.
            if here == ValueKind::Text && body != ValueKind::Text && body != ValueKind::Empty {
                looks_header += 1;
            }
        }

        if considered == 0 {
            break;
        }
        if f64::from(looks_header) / f64::from(considered) < opts.header_agreement {
            break;
        }
        header_rows += 1;
    }

    header_rows
}

/// The most common non-empty value kind in a column slice.
fn dominant_kind(sheet: &Sheet, top: u32, bottom: u32, col: u16) -> Option<ValueKind> {
    let mut counts = [0u32; 5];
    // Cap the sample: a column can be a hundred thousand rows deep and the
    // dominant kind is obvious long before the end.
    const SAMPLE: u32 = 64;
    let mut seen = 0;
    for row in top..=bottom {
        let kind = sheet.kind(row, col);
        if kind == ValueKind::Empty {
            continue;
        }
        counts[kind as usize] += 1;
        seen += 1;
        if seen >= SAMPLE {
            break;
        }
    }
    if seen == 0 {
        return None;
    }
    let (best, _) = counts
        .iter()
        .enumerate()
        .max_by_key(|&(_, n)| *n)
        .expect("five buckets");
    Some(match best {
        1 => ValueKind::Number,
        2 => ValueKind::Text,
        3 => ValueKind::Bool,
        4 => ValueKind::Error,
        _ => ValueKind::Empty,
    })
}

/// Whether the leading column labels rows rather than holding data.
fn has_row_labels(sheet: &Sheet, range: RangeRef, header_rows: u32) -> bool {
    if range.cols() < 2 {
        return false;
    }
    let body_top = range.top + header_rows;
    if body_top > range.bottom {
        return false;
    }
    let first = dominant_kind(sheet, body_top, range.bottom, range.left);
    if first != Some(ValueKind::Text) {
        return false;
    }
    // Only a label column if the rest of the block is mostly not text.
    let mut non_text = 0;
    let mut considered = 0;
    for col in (range.left + 1)..=range.right {
        if let Some(kind) = dominant_kind(sheet, body_top, range.bottom, col) {
            considered += 1;
            if kind != ValueKind::Text {
                non_text += 1;
            }
        }
    }
    considered > 0 && f64::from(non_text) / f64::from(considered) >= 0.5
}

/// Header text per body column, joining stacked header rows with `.`.
fn column_headers(
    sheet: &Sheet,
    range: RangeRef,
    header_rows: u32,
    header_cols: u16,
) -> Vec<String> {
    let mut out = Vec::new();
    for col in (range.left + header_cols)..=range.right {
        let mut parts: Vec<String> = Vec::new();
        for r in 0..header_rows {
            let row = range.top + r;
            let text = match sheet.value(row, col) {
                CellValue::Empty => String::new(),
                other => other.to_display(),
            };
            // A merged or repeated header leaves blanks under the label; skip
            // them rather than emitting `Q3..Revenue`.
            if !text.is_empty() && !parts.iter().any(|p| p == &text) {
                parts.push(text);
            }
        }
        out.push(parts.join("."));
    }
    out
}

/// Whether a range is mostly textual, used to spot titles and notes.
fn is_mostly_text(sheet: &Sheet, range: RangeRef) -> bool {
    let (mut text, mut total) = (0u32, 0u32);
    for (_, cell) in sheet.iter_range(range) {
        if cell.value.kind() == ValueKind::Empty {
            continue;
        }
        total += 1;
        if cell.value.kind() == ValueKind::Text {
            text += 1;
        }
    }
    total > 0 && f64::from(text) / f64::from(total) > 0.5
}

/// Regions for every sheet in a workbook, keyed by sheet id.
pub fn detect_workbook_regions(workbook: &eg_model::Workbook) -> Vec<(SheetId, Vec<Region>)> {
    workbook
        .sheets
        .iter()
        .map(|s| (s.id, detect_regions(s)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_model::{Cell, CellValue, ExcelTable};

    /// Build a sheet from a compact text grid.
    ///
    /// `.` is an empty cell, a bare number is numeric, anything else is text.
    fn grid(rows: &[&str]) -> Sheet {
        let mut sheet = Sheet::new(SheetId(0), "Sheet1");
        for (r, line) in rows.iter().enumerate() {
            for (c, tok) in line.split_whitespace().enumerate() {
                if tok == "." {
                    continue;
                }
                let value = match tok.parse::<f64>() {
                    Ok(n) => CellValue::Number(n),
                    Err(_) => CellValue::Text(tok.to_string()),
                };
                sheet.set(r as u32, c as u16, Cell::literal(value));
            }
        }
        sheet
    }

    #[test]
    fn a_single_table_is_one_region_with_its_header() {
        let sheet = grid(&[
            "Region Q1 Q2",
            "North  10 20",
            "South  30 40",
            "East   50 60",
        ]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 1, "{regions:#?}");
        let r = &regions[0];
        assert_eq!(r.range.to_a1(), "A1:C4");
        assert_eq!(r.kind, RegionKind::Table);
        assert_eq!(r.header_rows, 1);
        assert_eq!(r.header_cols, 1, "the Region column labels rows");
        assert_eq!(r.headers, ["Q1", "Q2"]);
        assert_eq!(r.body().unwrap().to_a1(), "B2:C4");
    }

    #[test]
    fn a_blank_row_separates_two_blocks() {
        let sheet = grid(&[
            "Region Q1",
            "North  10",
            ".      .",
            "Region Q2",
            "South  30",
        ]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 2, "{regions:#?}");
        assert_eq!(regions[0].range.to_a1(), "A1:B2");
        assert_eq!(regions[1].range.to_a1(), "A4:B5");
    }

    #[test]
    fn a_blank_column_separates_side_by_side_tables() {
        let sheet = grid(&["Region Q1 . Region Q2", "North  10 . South  30"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 2, "{regions:#?}");
        assert_eq!(regions[0].range.to_a1(), "A1:B2");
        assert_eq!(regions[1].range.to_a1(), "D1:E2");
    }

    #[test]
    fn blocks_stacked_and_side_by_side_all_separate() {
        // Alternating row/column splitting has to recurse to get all four.
        let sheet = grid(&["a 1 . b 2", ". . . . .", "c 3 . d 4"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 4, "{regions:#?}");
        let mut spans: Vec<String> = regions.iter().map(|r| r.range.to_a1()).collect();
        spans.sort();
        assert_eq!(spans, ["A1:B1", "A3:B3", "D1:E1", "D3:E3"]);
    }

    #[test]
    fn a_numeric_block_with_no_header_is_a_block() {
        let sheet = grid(&["1 2 3", "4 5 6"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].kind, RegionKind::Block);
        assert_eq!(regions[0].header_rows, 0);
        assert!(regions[0].headers.is_empty());
    }

    #[test]
    fn a_lone_text_strip_is_a_note() {
        let sheet = grid(&["Prepared by the finance team"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].kind, RegionKind::Note);
    }

    #[test]
    fn a_text_only_table_is_not_mistaken_for_a_header() {
        // Text over text is not evidence of a header, so this stays a block.
        let sheet = grid(&["alpha beta", "gamma delta", "epsilon zeta"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions[0].header_rows, 0, "{:#?}", regions[0]);
    }

    #[test]
    fn stacked_headers_join_into_paths() {
        let sheet = grid(&[
            "Q3    Q3      Q4",
            "Gross Net     Net",
            "1     2       3",
            "4     5       6",
        ]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 1);
        let r = &regions[0];
        assert_eq!(r.header_rows, 2, "{r:#?}");
        assert_eq!(r.headers, ["Q3.Gross", "Q3.Net", "Q4.Net"]);
    }

    #[test]
    fn detected_blocks_never_span_a_declared_table() {
        // The table's own cells are filtered out before partitioning, but the
        // cells around it still bound a rectangle that encloses it. Without the
        // cut at the table's edges, `A1:C2` and the table at `B2` would both
        // claim B2, and any answer citing it would have two header contexts.
        let mut sheet = grid(&["x y z", "p . q"]);
        sheet.tables.push(ExcelTable {
            name: "Inner".into(),
            range: RangeRef::parse_local("B2:B2", SheetId(0)).unwrap(),
            columns: vec!["Inner".into()],
            has_header_row: false,
            has_totals_row: false,
        });
        let regions = detect_regions(&sheet);
        for (i, a) in regions.iter().enumerate() {
            for b in &regions[i + 1..] {
                assert!(
                    !a.range.intersects(&b.range),
                    "{} overlaps {}",
                    a.range.to_a1(),
                    b.range.to_a1()
                );
            }
        }
    }

    #[test]
    fn declared_tables_are_used_verbatim() {
        let mut sheet = grid(&["Region Q1", "North 10", "South 20"]);
        sheet.tables.push(ExcelTable {
            name: "Sales".into(),
            range: RangeRef::parse_local("A1:B3", SheetId(0)).unwrap(),
            columns: vec!["Region".into(), "Q1".into()],
            has_header_row: true,
            has_totals_row: false,
        });
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 1, "declared table must not be re-detected");
        assert_eq!(regions[0].source, RegionSource::Declared);
        assert_eq!(regions[0].headers, ["Region", "Q1"]);
        // The table's name is not a row, so the body starts directly under the
        // header row rather than one row further down.
        assert_eq!(regions[0].body().unwrap().to_a1(), "A2:B3");
    }

    #[test]
    fn a_lone_text_cell_above_a_block_becomes_its_title() {
        let sheet = grid(&["Impairment_summary . .", "1 2 3", "4 5 6"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 1, "{regions:#?}");
        assert_eq!(regions[0].title.as_deref(), Some("Impairment_summary"));
        assert_eq!(regions[0].header_rows, 0);
        // The title row is not data.
        assert_eq!(regions[0].body().unwrap().to_a1(), "A2:C3");
    }

    #[test]
    fn a_title_does_not_hide_the_header_below_it() {
        let sheet = grid(&[
            "Sales_by_region . .",
            "Region Q1 Q2",
            "North  10 20",
            "South  30 40",
        ]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 1, "{regions:#?}");
        let r = &regions[0];
        assert_eq!(r.title.as_deref(), Some("Sales_by_region"));
        assert_eq!(r.header_rows, 1, "{r:#?}");
        assert_eq!(r.headers, ["Q1", "Q2"]);
        assert_eq!(r.body().unwrap().to_a1(), "B3:C4");
    }

    #[test]
    fn a_header_row_is_not_mistaken_for_a_title() {
        // More than one populated cell means it labels columns, not the block.
        let sheet = grid(&["Region Q1 Q2", "North 10 20", "South 30 40"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions[0].title, None);
        assert_eq!(regions[0].header_rows, 1);
    }

    #[test]
    fn a_single_column_keeps_its_header_rather_than_gaining_a_title() {
        // One populated cell in the top row is a header here, not a title.
        let sheet = grid(&["Amount", "100", "200"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions[0].title, None, "{:#?}", regions[0]);
        assert_eq!(regions[0].header_rows, 1);
        assert_eq!(regions[0].headers, ["Amount"]);
        assert_eq!(regions[0].kind, RegionKind::Table);
    }

    #[test]
    fn a_numeric_first_row_is_not_a_title() {
        let sheet = grid(&["7 . .", "1 2 3", "4 5 6"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions[0].title, None);
    }

    #[test]
    fn a_single_row_region_has_no_title() {
        // With nothing under it there is no block for it to be the title of.
        let sheet = grid(&["Just a note"]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions[0].title, None);
        assert_eq!(regions[0].kind, RegionKind::Note);
    }

    #[test]
    fn an_empty_sheet_has_no_regions() {
        assert!(detect_regions(&Sheet::new(SheetId(0), "Sheet1")).is_empty());
    }

    #[test]
    fn regions_never_overlap() {
        let sheet = grid(&["a 1 . b 2", ". . . . .", "c 3 . d 4"]);
        let regions = detect_regions(&sheet);
        for (i, a) in regions.iter().enumerate() {
            for b in &regions[i + 1..] {
                assert!(!a.range.intersects(&b.range), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn density_reports_how_solid_a_region_is() {
        let sheet = grid(&["1 2", "3 ."]);
        let regions = detect_regions(&sheet);
        assert_eq!(regions.len(), 1);
        assert!((regions[0].density() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn a_wide_gap_is_needed_when_configured() {
        // With row_gap 2, a single blank row no longer separates.
        let sheet = grid(&["a 1", ". .", "b 2"]);
        let opts = RegionOptions {
            row_gap: 2,
            ..Default::default()
        };
        assert_eq!(detect_regions_with(&sheet, &opts).len(), 1);
        assert_eq!(detect_regions(&sheet).len(), 2);
    }
}
