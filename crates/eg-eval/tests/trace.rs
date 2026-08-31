//! Cell-level provenance against workbooks small enough to check by hand.

use eg_eval::{cell, cells_in, dependents_of, precedents_of, Target};
use eg_model::{Cell, CellRef, CellValue, RangeRef, Sheet, SheetId, Workbook, WorkbookFormat};

fn grid(id: u16, name: &str, rows: &[&str]) -> Sheet {
    let mut sheet = Sheet::new(SheetId(id), name);
    for (r, line) in rows.iter().enumerate() {
        for (c, tok) in line.split_whitespace().enumerate() {
            if tok == "." {
                continue;
            }
            let cell = match tok.strip_prefix('=') {
                Some(f) => Cell {
                    value: CellValue::Number(0.0),
                    formula: Some(f.to_string()),
                    format: Default::default(),
                },
                None => match tok.parse::<f64>() {
                    Ok(n) => Cell::literal(CellValue::Number(n)),
                    Err(_) => Cell::literal(CellValue::Text(tok.to_string())),
                },
            };
            sheet.set(r as u32, c as u16, cell);
        }
    }
    sheet
}

fn book() -> Workbook {
    Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![
            grid(
                0,
                "Main",
                &[
                    "Row Net Gross Odd",
                    "a =B2*Rates!B2 =B2+C2 =GONE!A1",
                    "b =B3*Rates!B3 =B3+C3 =[1]Other!A1",
                    "c =B4*rates!B4 =B4+C4 .",
                ],
            ),
            grid(1, "Rates", &["K V", "x 1", "y 2", "z 3"]),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

fn at(sheet: u16, row: u32, col: u16) -> CellRef {
    CellRef::new(SheetId(sheet), row, col)
}

fn targets(refs: &[eg_eval::Reference]) -> Vec<String> {
    refs.iter()
        .map(|r| match &r.target {
            Target::Cells(range) => range.to_a1(),
            // Sheet ids spelled out: the ranges of a 3-D span differ only by
            // the sheet they are on, so `to_a1` alone would render them
            // identically and assert nothing.
            Target::Spanned(ranges) => ranges
                .iter()
                .map(|r| format!("#{}!{}", r.sheet.0, r.to_a1()))
                .collect::<Vec<_>>()
                .join("+"),
            Target::UnknownSheet(name) => format!("#REF:{name}"),
            Target::ExternalWorkbook(token) => format!("#EXT:{token}"),
        })
        .collect()
}

/// Three month sheets and a summary that adds one cell across all of them.
fn months() -> Workbook {
    Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![
            grid(0, "Jan", &[". .", ". 1"]),
            grid(1, "Feb", &[". .", ". 2"]),
            grid(2, "Mar", &[". .", ". 3"]),
            grid(3, "Summary", &["=SUM(Jan:Mar!B2)"]),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

#[test]
fn a_3d_reference_names_every_sheet_it_spans() {
    // `Jan:Mar!B2` reads three cells, and resolving it to `Jan!B2` alone —
    // which this layer did, while the graph beside it got it right — makes
    // every reader on the other sheets invisible to `dependents_of` and to
    // the what-if walk built on it.
    let wb = months();
    let refs = precedents_of(&wb, at(3, 0, 0));
    assert_eq!(targets(&refs), vec!["#0!B2+#1!B2+#2!B2"]);
    assert_eq!(
        refs[0].target.ranges().len(),
        3,
        "one reference, three ranges"
    );
}

#[test]
fn a_middle_sheet_of_a_span_has_the_summary_as_a_dependent() {
    // The direction that costs a full scan, and the one a what-if is built
    // on: Feb is neither end of `Jan:Mar`, so a resolver that keeps only the
    // sheet written first misses this entirely.
    let wb = months();
    let feb_b2 = RangeRef::new(SheetId(1), 1, 1, 1, 1);
    let (refs, report) = dependents_of(&wb, feb_b2, 10);
    assert_eq!(report.matches, 1);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].from, at(3, 0, 0), "Summary!A1 reads Feb!B2");
    assert_eq!(refs[0].text, "Jan:Mar!B2");

    // And it is reported once, not once per sheet of the span.
    let jan_to_mar = RangeRef::new(SheetId(0), 1, 1, 1, 1);
    let (refs, report) = dependents_of(&wb, jan_to_mar, 10);
    assert_eq!((report.matches, refs.len()), (1, 1));
}

#[test]
fn a_3d_reference_with_a_missing_end_is_a_ref_break() {
    // Half a span is not a span: `Jan:Gone!B2` names a sheet the workbook
    // does not have, and reading it as `Jan!B2` would invent a dependency
    // the formula does not have and hide a real `#REF!`.
    let mut wb = months();
    wb.sheet_mut(SheetId(3)).unwrap().set(
        0,
        0,
        eg_model::Cell {
            value: CellValue::Number(0.0),
            formula: Some("SUM(Jan:Gone!B2)".into()),
            format: Default::default(),
        },
    );
    assert_eq!(targets(&precedents_of(&wb, at(3, 0, 0))), vec!["#REF:Gone"]);
}

#[test]
fn an_unqualified_reference_resolves_to_the_sheet_the_formula_is_on() {
    let wb = book();
    // Main!C2 is `=B2+C2`, both on Main.
    let refs = precedents_of(&wb, at(0, 1, 2));
    assert_eq!(refs.len(), 2);
    for reference in &refs {
        match &reference.target {
            Target::Cells(range) => assert_eq!(range.sheet, SheetId(0)),
            other => panic!("resolved to {other:?}"),
        }
    }
    assert_eq!(targets(&refs), vec!["B2", "C2"]);
}

#[test]
fn a_qualified_reference_resolves_to_the_sheet_it_names() {
    let wb = book();
    // Main!B2 is `=B2*Rates!B2`: one local, one across.
    let refs = precedents_of(&wb, at(0, 1, 1));
    let sheets: Vec<SheetId> = refs
        .iter()
        .filter_map(|r| match &r.target {
            Target::Cells(range) => Some(range.sheet),
            _ => None,
        })
        .collect();
    assert_eq!(sheets, vec![SheetId(0), SheetId(1)]);
    assert!(refs.iter().any(|r| r.text == "Rates!B2"));
}

#[test]
fn a_sheet_name_is_matched_without_regard_to_case() {
    // Excel sheet names are case-insensitive, and a formula need not spell one
    // the way its tab does. Getting this wrong turns a working reference into
    // a phantom broken one.
    let wb = book();
    let refs = precedents_of(&wb, at(0, 3, 1)); // `=B4*rates!B4`
    assert!(
        refs.iter().any(|r| matches!(
            &r.target,
            Target::Cells(range) if range.sheet == SheetId(1)
        )),
        "resolved to {:?}",
        targets(&refs)
    );
}

#[test]
fn a_missing_sheet_and_another_workbook_are_told_apart() {
    let wb = book();
    let broken = precedents_of(&wb, at(0, 1, 3)); // `=GONE!A1`
    assert_eq!(targets(&broken), vec!["#REF:GONE"]);

    let external = precedents_of(&wb, at(0, 2, 3)); // `=[1]Other!A1`
    assert_eq!(targets(&external), vec!["#EXT:1"]);
}

#[test]
fn a_cell_with_no_formula_reads_nothing() {
    let wb = book();
    assert!(precedents_of(&wb, at(1, 1, 1)).is_empty());
    // And so does a cell that is not there at all.
    assert!(precedents_of(&wb, at(0, 900, 900)).is_empty());
    assert!(precedents_of(&wb, at(9, 0, 0)).is_empty());
}

#[test]
fn dependents_find_every_formula_that_names_a_cell() {
    let wb = book();
    // Rates!B2 is read by Main!B2 only.
    let (refs, report) = dependents_of(&wb, RangeRef::single(at(1, 1, 1)), 100);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].from, at(0, 1, 1));
    assert_eq!(report.matches, 1);
    assert!(!report.capped);
    assert!(report.formulas_scanned > 0);
}

#[test]
fn a_dependent_is_found_through_a_range_it_only_overlaps() {
    // Nothing writes `Rates!B2:B4` in this book, but a query for that range
    // must still find the three formulas naming cells inside it. A reader
    // asking "what depends on this column" means the column, not an exact
    // string match.
    let wb = book();
    let column = RangeRef::new(SheetId(1), 1, 1, 3, 1);
    let (refs, report) = dependents_of(&wb, column, 100);
    assert_eq!(report.matches, 3, "found {:?}", refs);
    assert_eq!(refs.len(), 3);
}

#[test]
fn the_cap_shortens_the_list_and_never_the_count() {
    let wb = book();
    let column = RangeRef::new(SheetId(1), 1, 1, 3, 1);
    let (refs, report) = dependents_of(&wb, column, 2);
    assert_eq!(refs.len(), 2);
    assert_eq!(report.matches, 3, "the count was capped too");
    assert!(report.capped);
}

#[test]
fn a_broken_reference_is_never_counted_as_a_dependent() {
    // `=GONE!A1` names a sheet the workbook does not have. It must not match a
    // query for any real range, however the row and column happen to line up.
    let wb = book();
    let a1_everywhere = RangeRef::new(SheetId(0), 0, 0, 0, 0);
    let (refs, _) = dependents_of(&wb, a1_everywhere, 100);
    assert!(
        refs.iter().all(|r| r.text != "GONE!A1"),
        "a #REF! break was reported as a dependency: {refs:?}"
    );
}

#[test]
fn cells_come_back_populated_only_and_in_reading_order() {
    let wb = book();
    let range = RangeRef::new(SheetId(0), 0, 0, 3, 3);
    let (cells, capped) = cells_in(&wb, range, 100);
    assert!(!capped);

    // Main!D4 is blank, so 15 of the 16 cells in the rectangle are there.
    assert_eq!(cells.len(), 15);
    let order: Vec<(u32, u16)> = cells.iter().map(|c| (c.cell.row, c.cell.col)).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted, "cells came back out of reading order");
}

#[test]
fn a_whole_column_range_is_clipped_to_what_the_sheet_uses() {
    // `Sheet1!A:A` now parses (C1), so this is reachable from a citation the
    // way `A1:D4` always was. Without clipping to the sheet's used range
    // first, this would probe every one of 1,048,576 rows through
    // `iter_range` — the exact perf regression the fix exists to avoid.
    let wb = book();
    // Column B (index 1) of Main, top to the sheet's last row — the shape
    // `A:A`/`Sheet1!B:B` resolves to, built by hand since the fixture has no
    // formula that produces one.
    let whole_column = RangeRef::new(SheetId(0), 0, 1, eg_model::MAX_ROW, 1);
    let (cells, capped) = cells_in(&wb, whole_column, 100);
    assert!(!capped);
    // Column B on Main has three populated cells (B2, B3, B4) plus its
    // header (B1) — none past row 4, which is all `used_range` covers.
    assert_eq!(cells.len(), 4);
}

#[test]
fn a_cell_fact_carries_its_own_citation_and_formula() {
    let wb = book();
    let fact = cell(&wb, at(0, 1, 1)).expect("Main!B2 is populated");
    assert_eq!(fact.a1, "Main!B2");
    assert_eq!(fact.formula.as_deref(), Some("B2*Rates!B2"));
    assert!(cell(&wb, at(0, 900, 900)).is_none());
}

#[test]
fn a_sheet_name_that_needs_quoting_survives_the_round_trip() {
    // The quoting bug that cost 1,663 real references upstream lived exactly
    // here: an unquoted `Q3 SALES!A1` reads as a sheet called SALES.
    // Written by hand: the `grid` helper splits on whitespace, and the whole
    // point of this reference is that it contains a space.
    let mut main = Sheet::new(SheetId(0), "Main");
    main.set(
        1,
        0,
        Cell {
            value: CellValue::Number(0.0),
            formula: Some("'Q3 Sales'!B2*2".to_string()),
            format: Default::default(),
        },
    );
    let wb = Workbook {
        path: "quoted.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "h".into(),
        sheets: vec![main, grid(1, "Q3 Sales", &["K V", "x 1"])],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let refs = precedents_of(&wb, at(0, 1, 0));
    assert_eq!(refs.len(), 1, "scanned {:?}", targets(&refs));
    assert_eq!(refs[0].text, "'Q3 Sales'!B2");
    assert!(
        matches!(&refs[0].target, Target::Cells(r) if r.sheet == SheetId(1)),
        "resolved to {:?}",
        targets(&refs)
    );
}
