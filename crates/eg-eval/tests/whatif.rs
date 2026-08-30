//! Changing a cell, against workbooks small enough to check by hand.

use eg_eval::whatif::{what_if, Blocked, Change, Stopped, WhatIfOptions};
use eg_model::{Cell, CellRef, CellValue, RangeRef, Sheet, SheetId, Workbook, WorkbookFormat};

/// A cell holding a formula and the value Excel last calculated for it.
fn formula(text: &str, stored: f64) -> Cell {
    Cell {
        value: CellValue::Number(stored),
        formula: Some(text.into()),
        format: Default::default(),
    }
}

fn literal(n: f64) -> Cell {
    Cell::literal(CellValue::Number(n))
}

/// ```text
///        A            B                  C
///  1     rate 0.1     =A1*100  (10)      =B1+B2  (30)
///  2     base 200     =A2*0.1  (20)
/// ```
/// `C1` is two levels below `A1`; `B2` is one.
fn book() -> Workbook {
    let mut sheet = Sheet::new(SheetId(0), "Main");
    sheet.set(0, 0, literal(0.1));
    sheet.set(1, 0, literal(200.0));
    sheet.set(0, 1, formula("A1*100", 10.0));
    sheet.set(1, 1, formula("A2*A1", 20.0));
    sheet.set(0, 2, formula("B1+B2", 30.0));
    Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

fn at(row: u32, col: u16) -> CellRef {
    CellRef::new(SheetId(0), row, col)
}

fn change(row: u32, col: u16, to: f64) -> Change {
    Change::new(at(row, col), CellValue::Number(to))
}

/// The new value of a cell, by its citation.
fn after(impact: &eg_eval::whatif::Impact, a1: &str) -> Option<CellValue> {
    impact
        .moved
        .iter()
        .find(|m| m.a1 == a1)
        .map(|m| m.after.clone())
}

#[test]
fn a_change_travels_as_far_as_the_chain_goes() {
    let impact = what_if(&book(), &[change(0, 0, 0.2)], &WhatIfOptions::default());

    assert_eq!(impact.report.affected, 3, "B1, B2 and C1 all read A1");
    assert_eq!(impact.report.moved, 3);
    assert_eq!(impact.report.blocked, 0);
    assert_eq!(after(&impact, "Main!B1"), Some(CellValue::Number(20.0)));
    assert_eq!(after(&impact, "Main!B2"), Some(CellValue::Number(40.0)));
    // C1 is computed from the *new* B1 and B2, not the stored ones — which is
    // the whole difference between this and recomputing one cell.
    assert_eq!(after(&impact, "Main!C1"), Some(CellValue::Number(60.0)));

    let levels: Vec<usize> = ["Main!B1", "Main!B2", "Main!C1"]
        .iter()
        .map(|a1| impact.moved.iter().find(|m| &m.a1 == a1).unwrap().level)
        .collect();
    assert_eq!(levels, [1, 1, 2]);
}

#[test]
fn what_was_displaced_is_reported_and_not_recomputed() {
    // Substituting into a formula cell is typing over it, as in Excel: the
    // value stands and the formula it replaced is named rather than run.
    let impact = what_if(&book(), &[change(0, 1, 999.0)], &WhatIfOptions::default());
    assert_eq!(impact.changes[0].a1, "Main!B1");
    assert_eq!(impact.changes[0].before, CellValue::Number(10.0));
    assert_eq!(
        impact.changes[0].replaced_formula.as_deref(),
        Some("A1*100")
    );
    assert_eq!(impact.report.affected, 1, "only C1 reads B1");
    assert_eq!(after(&impact, "Main!C1"), Some(CellValue::Number(1019.0)));
}

#[test]
fn a_cell_that_does_not_move_stops_the_walk() {
    let mut wb = book();
    // B2 multiplies by A2, so setting A2 to 0 pins it at 0 whatever A1 does.
    wb.sheet_mut(SheetId(0)).unwrap().set(1, 0, literal(0.0));
    wb.sheet_mut(SheetId(0))
        .unwrap()
        .set(1, 1, formula("A2*A1", 0.0));

    let impact = what_if(&wb, &[change(0, 0, 0.2)], &WhatIfOptions::default());
    assert_eq!(impact.report.unchanged, 1, "B2 recomputes to the same 0");
    // C1 still moves, because B1 did — but it is reached through B1 alone.
    assert_eq!(after(&impact, "Main!C1"), Some(CellValue::Number(20.0)));
}

#[test]
fn changing_a_cell_to_what_it_holds_moves_nothing() {
    let impact = what_if(&book(), &[change(0, 0, 0.1)], &WhatIfOptions::default());
    assert_eq!(impact.report.affected, 0);
    assert_eq!(impact.report.scans, 0, "nothing to look for, so no scan");
    assert_eq!(
        impact.changes.len(),
        1,
        "the substitution is still reported"
    );
}

#[test]
fn a_formula_this_cannot_model_blocks_itself_and_what_reads_it() {
    let mut wb = book();
    // B2 becomes unmodellable, and C1 reads it.
    wb.sheet_mut(SheetId(0))
        .unwrap()
        .set(1, 1, formula("SUMPRODUCT(A1:A2,A1:A2)", 20.0));

    let impact = what_if(&wb, &[change(0, 0, 0.2)], &WhatIfOptions::default());
    assert_eq!(impact.report.blocked, 2, "B2, and C1 because it reads B2");

    let b2 = impact
        .unanswered
        .iter()
        .find(|u| u.a1 == "Main!B2")
        .unwrap();
    assert!(matches!(b2.reason, Blocked::Formula(_)));
    let c1 = impact
        .unanswered
        .iter()
        .find(|u| u.a1 == "Main!C1")
        .unwrap();
    assert_eq!(
        c1.reason,
        Blocked::Upstream("Main!B2".into()),
        "a cell reading an unanswered one is unanswered, not unchanged"
    );
    // The honest failure mode: C1 must not be reported as having kept its
    // stored value, which is what leaving it out of `moved` alone would imply.
    assert!(after(&impact, "Main!C1").is_none());
}

#[test]
fn a_cycle_is_reported_rather_than_iterated() {
    let mut wb = book();
    let sheet = wb.sheet_mut(SheetId(0)).unwrap();
    // D1 and E1 read each other, and both read A1.
    sheet.set(0, 3, formula("A1+E1", 1.0));
    sheet.set(0, 4, formula("A1+D1", 1.0));

    let impact = what_if(&wb, &[change(0, 0, 0.2)], &WhatIfOptions::default());
    let cycles: Vec<&str> = impact
        .unanswered
        .iter()
        .filter(|u| u.reason == Blocked::Cycle)
        .map(|u| u.a1.as_str())
        .collect();
    assert_eq!(cycles, ["Main!D1", "Main!E1"]);
}

#[test]
fn one_level_reads_another_cell_of_the_same_level_in_the_right_order() {
    let mut wb = book();
    let sheet = wb.sheet_mut(SheetId(0)).unwrap();
    // D1 reads A1 directly, so it lands in level 1 beside B1 — and it also
    // reads B1, so it must be computed after it however the scan ordered them.
    sheet.set(0, 3, formula("A1+B1", 10.1));

    let impact = what_if(&wb, &[change(0, 0, 0.2)], &WhatIfOptions::default());
    assert_eq!(
        after(&impact, "Main!D1"),
        Some(CellValue::Number(20.2)),
        "0.2 plus the recomputed B1 of 20, not the stored 10"
    );
}

#[test]
fn a_substitution_is_visible_to_everything_that_reads_the_cell() {
    let mut wb = book();
    let mut rates = Sheet::new(SheetId(1), "Rates");
    for (row, (key, rate)) in [("a", 1.0), ("b", 2.0), ("c", 3.0)].iter().enumerate() {
        rates.set(
            row as u32,
            0,
            Cell::literal(CellValue::Text(key.to_string())),
        );
        rates.set(row as u32, 1, literal(*rate));
    }
    wb.sheets.push(rates);
    let sheet = wb.sheet_mut(SheetId(0)).unwrap();
    sheet.set(3, 0, formula("VLOOKUP(\"b\",Rates!A1:B3,2,FALSE)", 2.0));
    sheet.set(4, 0, formula("SUM(Rates!B1:B3)", 6.0));

    let impact = what_if(
        &wb,
        &[Change::new(
            CellRef::new(SheetId(1), 1, 1),
            CellValue::Number(20.0),
        )],
        &WhatIfOptions::default(),
    );
    assert_eq!(
        after(&impact, "Main!A4"),
        Some(CellValue::Number(20.0)),
        "the lookup reads the substituted rate, not the stored one"
    );
    assert_eq!(after(&impact, "Main!A5"), Some(CellValue::Number(24.0)));
}

#[test]
fn a_value_put_into_an_empty_cell_is_read_by_a_range_over_it() {
    let mut wb = book();
    // B3 is empty, and D1 sums the column it sits in.
    wb.sheet_mut(SheetId(0))
        .unwrap()
        .set(0, 3, formula("SUM(B1:B3)", 30.0));

    let impact = what_if(&wb, &[change(2, 1, 5.0)], &WhatIfOptions::default());
    assert_eq!(
        after(&impact, "Main!D1"),
        Some(CellValue::Number(35.0)),
        "a substitution into a cell the sheet never populated still counts"
    );
}

#[test]
fn the_walk_can_be_stopped_short_and_says_so() {
    let impact = what_if(
        &book(),
        &[change(0, 0, 0.2)],
        &WhatIfOptions {
            max_levels: 1,
            ..Default::default()
        },
    );
    assert_eq!(impact.report.affected, 2, "B1 and B2, and not C1 past them");
    assert_eq!(impact.report.levels, 1);
    assert_eq!(
        impact.report.stopped,
        Some(Stopped::Levels),
        "C1 is missing because the walk stopped, not because it did not move"
    );
    assert!(after(&impact, "Main!C1").is_none());

    // And a walk that finishes says nothing stopped it.
    let whole = what_if(&book(), &[change(0, 0, 0.2)], &WhatIfOptions::default());
    assert_eq!(whole.report.stopped, None);

    let tight = what_if(
        &book(),
        &[change(0, 0, 0.2)],
        &WhatIfOptions {
            max_cells: 1,
            ..Default::default()
        },
    );
    assert_eq!(tight.report.stopped, Some(Stopped::Cells));
}

#[test]
fn a_cell_that_already_disagreed_is_flagged_rather_than_blamed_on_the_change() {
    let mut wb = book();
    // B1 stores 999 where its formula says 10: stale before anything changed.
    wb.sheet_mut(SheetId(0))
        .unwrap()
        .set(0, 1, formula("A1*100", 999.0));

    let impact = what_if(&wb, &[change(0, 0, 0.2)], &WhatIfOptions::default());
    let b1 = impact.moved.iter().find(|m| m.a1 == "Main!B1").unwrap();
    assert!(b1.was_stale, "999 was never what this formula computed");
    assert_eq!(b1.before, CellValue::Number(999.0));
    assert_eq!(b1.after, CellValue::Number(20.0));

    let b2 = impact.moved.iter().find(|m| m.a1 == "Main!B2").unwrap();
    assert!(!b2.was_stale);
}

#[test]
fn a_change_to_another_sheet_is_followed_across() {
    let mut wb = book();
    let mut other = Sheet::new(SheetId(1), "Other");
    other.set(0, 0, literal(7.0));
    wb.sheets.push(other);
    wb.sheet_mut(SheetId(0))
        .unwrap()
        .set(0, 3, formula("Other!A1*2", 14.0));

    let impact = what_if(
        &wb,
        &[Change::new(
            CellRef::new(SheetId(1), 0, 0),
            CellValue::Number(8.0),
        )],
        &WhatIfOptions::default(),
    );
    assert_eq!(after(&impact, "Main!D1"), Some(CellValue::Number(16.0)));
}

#[test]
fn nothing_asked_is_nothing_reported() {
    let impact = what_if(&book(), &[], &WhatIfOptions::default());
    assert_eq!(impact.report, Default::default());
    assert!(impact.changes.is_empty());
}

#[test]
fn the_workbook_is_not_touched() {
    let wb = book();
    let before = wb.clone();
    let _ = what_if(&wb, &[change(0, 0, 0.2)], &WhatIfOptions::default());
    assert_eq!(
        wb.sheet(SheetId(0)).unwrap().value(0, 0),
        before.sheet(SheetId(0)).unwrap().value(0, 0)
    );
    // And the range it sits in still reads as it did.
    let range = RangeRef::new(SheetId(0), 0, 0, 1, 2);
    let (cells, _) = eg_eval::cells_in(&wb, range, 100);
    assert_eq!(cells.len(), 5);
}
