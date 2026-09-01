//! Changing a cell, against workbooks small enough to check by hand.

use eg_eval::whatif::{what_if, Blocked, Change, Stopped, WhatIfOptions};
use eg_model::{
    Cell, CellRef, CellValue, DefinedName, RangeRef, Sheet, SheetId, Workbook, WorkbookFormat,
};

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
    assert_eq!(tight.report.affected, 1, "the ceiling is never overshot");
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

#[test]
fn a_cell_reached_early_still_sees_an_input_that_moves_later() {
    // The level a cell is first reached at is its *shortest* distance from the
    // change, and a cell can read something further down a longer path. D1
    // reads A1 directly, so it is reached at level 1 — and it also reads C1,
    // which only moves at level 2. Judging D1 once, at level 1, would report a
    // number computed from the stored C1: wrong, and silently so.
    let mut wb = book();
    let sheet = wb.sheet_mut(SheetId(0)).unwrap();
    sheet.set(0, 3, formula("B1*10", 100.0)); // D1, level 2 behind B1
    sheet.set(0, 4, formula("A1+D1", 100.1)); // E1, level 1 and reads D1

    let impact = what_if(&wb, &[change(0, 0, 0.2)], &WhatIfOptions::default());
    assert_eq!(after(&impact, "Main!D1"), Some(CellValue::Number(200.0)));
    assert_eq!(
        after(&impact, "Main!E1"),
        Some(CellValue::Number(200.2)),
        "0.2 plus the recomputed D1 of 200, not the stored 100"
    );
    // And it is one affected cell, not two: reached twice, judged twice,
    // counted once.
    assert_eq!(
        impact.report.affected, 5,
        "B1, B2, C1, D1 and E1, each once"
    );
    assert_eq!(
        impact.moved.iter().filter(|m| m.a1 == "Main!E1").count(),
        1,
        "a cell judged again is corrected in place, not listed twice"
    );
}

#[test]
fn a_lookup_column_that_moves_mid_walk_is_not_answered_from_the_old_index() {
    // Past 512 rows a lookup is answered from a map built out of the column as
    // it read at the time, and the walk now holds one evaluator for its whole
    // run rather than an index per cell. So a key that moves *after* the map
    // was built has to drop it.
    //
    // `Main!B1` reads the changed cell directly, so it is judged at level 1 and
    // builds the map. `Rates!A9` is three hops away and only moves at level 3.
    // `Main!C1` reads that column and nothing nearer, so it is first judged at
    // level 4 — by which time the map it would otherwise be answered from is
    // two levels out of date.
    let mut main = Sheet::new(SheetId(0), "Main");
    main.set(0, 0, literal(0.0));
    main.set(0, 1, formula("VLOOKUP(7,Rates!A1:B600,2,FALSE)+A1", 70.0));
    main.set(0, 2, formula("VLOOKUP(999,Rates!A1:B600,2,FALSE)", 0.0));
    let mut rates = Sheet::new(SheetId(1), "Rates");
    for row in 0..600u32 {
        rates.set(row, 0, Cell::literal(CellValue::Number(f64::from(row))));
        rates.set(row, 1, literal(f64::from(row) * 10.0));
    }
    // Three hops from the change, ending in a key of the lookup column.
    rates.set(0, 2, formula("Main!A1", 0.0));
    rates.set(1, 2, formula("C1", 0.0));
    rates.set(8, 0, formula("C2+8", 8.0));
    let wb = Workbook {
        path: "lookup.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![main, rates],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };

    let impact = what_if(&wb, &[change(0, 0, 991.0)], &WhatIfOptions::default());
    assert_eq!(
        after(&impact, "Rates!A9"),
        Some(CellValue::Number(999.0)),
        "the key moves at level 3"
    );
    assert_eq!(
        after(&impact, "Main!C1"),
        Some(CellValue::Number(80.0)),
        "and the lookup for it finds row 9, not the map built before it moved"
    );
}

// ---- C2: defined names in the closure walk --------------------------------

/// A workbook with `Tax_Rate` defined as `Rates!$B$1` (a plain reference),
/// and `Main!D1 = Tax_Rate*A1` — a dependency on `Rates!B1` the closure walk
/// can only see by resolving the name, since `D1`'s own text never mentions
/// `Rates!B1` directly.
fn book_with_named_rate() -> Workbook {
    let mut main = book().sheets.remove(0);
    main.set(0, 3, formula("Tax_Rate*A1", 0.15)); // D1
    let mut rates = Sheet::new(SheetId(1), "Rates");
    rates.set(0, 1, literal(1.5)); // Rates!B1
    Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![main, rates],
        defined_names: vec![DefinedName {
            name: "Tax_Rate".into(),
            refers_to: "=Rates!$B$1".into(),
            scope: None,
        }],
        external_links: Vec::new(),
    }
}

#[test]
fn a_cell_reading_the_change_only_through_a_name_is_reported_moved() {
    let wb = book_with_named_rate();
    // D1 = Tax_Rate*A1 = Rates!B1 * A1. Change Rates!B1, which nothing in D1's
    // own text names — only `Tax_Rate` does.
    let impact = what_if(
        &wb,
        &[Change::new(
            CellRef::new(SheetId(1), 0, 1),
            CellValue::Number(2.0),
        )],
        &WhatIfOptions::default(),
    );
    assert_eq!(
        after(&impact, "Main!D1"),
        Some(CellValue::Number(0.2)),
        "D1 must be discovered as a reader through the name, not silently skipped"
    );
}

#[test]
fn a_cell_reading_a_blocked_cell_only_through_a_name_is_blocked_not_moved() {
    // D1 = Tax_Rate*2 reads Rates!B1 only through the name — nothing in D1's
    // own text mentions Rates!B1 or Main!A1. Rates!B1 itself is unmodellable
    // and, being a reader of Main!A1:A2, is discovered and blocked in level
    // 1; D1 must then be discovered (through the name) and blocked in level
    // 2, not evaluated against Rates!B1's stale stored value.
    let mut main = book().sheets.remove(0);
    main.set(0, 3, formula("Tax_Rate*2", 3.0)); // D1
    let mut rates = Sheet::new(SheetId(1), "Rates");
    rates.set(0, 1, formula("SUMPRODUCT(Main!A1:A2,Main!A1:A2)", 1.5)); // Rates!B1
    let wb = Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![main, rates],
        defined_names: vec![DefinedName {
            name: "Tax_Rate".into(),
            refers_to: "=Rates!$B$1".into(),
            scope: None,
        }],
        external_links: Vec::new(),
    };

    let impact = what_if(&wb, &[change(0, 0, 0.2)], &WhatIfOptions::default());
    let rates_b1 = impact
        .unanswered
        .iter()
        .find(|u| u.a1 == "Rates!B1")
        .expect("Rates!B1 reads the change directly and is itself unmodellable");
    assert!(matches!(rates_b1.reason, Blocked::Formula(_)));
    let d1 = impact
        .unanswered
        .iter()
        .find(|u| u.a1 == "Main!D1")
        .expect("D1 reads the blocked cell through the name and must be reported, not omitted");
    assert_eq!(
        d1.reason,
        Blocked::Upstream("Rates!B1".into()),
        "blocked because of what it reads, not guessed unchanged"
    );
    assert!(
        impact.moved.iter().all(|m| m.a1 != "Main!D1"),
        "must not also appear as Moved"
    );
}

#[test]
fn a_name_standing_for_a_formula_blocks_its_readers() {
    let mut wb = book();
    // `Tax_Rate` is defined as a formula, not a reference — this evaluator
    // refuses to follow it (see `calc::Eval::defined_name`), and a formula
    // naming it must be refused too, not silently treated as unaffected.
    wb.defined_names.push(DefinedName {
        name: "Tax_Rate".into(),
        refers_to: "=A1+1".into(),
        scope: None,
    });
    wb.sheet_mut(SheetId(0))
        .unwrap()
        .set(0, 3, formula("Tax_Rate*2", 2.2)); // D1

    let impact = what_if(&wb, &[change(0, 0, 0.5)], &WhatIfOptions::default());
    let d1 =
        impact.unanswered.iter().find(|u| u.a1 == "Main!D1").expect(
            "D1 might depend on the change through Tax_Rate's own A1+1, and must be reported",
        );
    assert!(matches!(d1.reason, Blocked::Formula(_)));
    assert!(impact.moved.iter().all(|m| m.a1 != "Main!D1"));
}

// ---- H1: a cycle spanning levels -------------------------------------------

#[test]
fn a_cross_level_cycle_is_reported_rather_than_ping_ponged() {
    // B1 reads A1 (the change) and C1; C1 reads only B1. Neither is a
    // same-level cycle: B1 is found at level 1 (it reads A1 directly), C1
    // only at level 2 (it reads B1) — `order_within`'s own cycle detection,
    // scoped to one level, never sees the two together. Before this fix they
    // ping-ponged between levels, each holding whatever its last visit
    // computed, until `Stopped::Levels`.
    let mut sheet = Sheet::new(SheetId(0), "Main");
    sheet.set(0, 0, literal(1.0)); // A1
    sheet.set(0, 1, formula("A1+C1", 2.0)); // B1
    sheet.set(0, 2, formula("B1", 2.0)); // C1
    let wb = Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };

    let impact = what_if(
        &wb,
        &[Change::new(
            CellRef::new(SheetId(0), 0, 0),
            CellValue::Number(5.0),
        )],
        &WhatIfOptions::default(),
    );

    assert_eq!(
        impact.report.moved, 0,
        "the whole affected set is the cycle — nothing settles into Moved"
    );
    assert_eq!(
        impact.report.stopped, None,
        "the cycle is caught, not silently exhausted against the level limit"
    );
    assert!(impact.moved.is_empty());
    let mut cycles: Vec<&str> = impact
        .unanswered
        .iter()
        .filter(|u| u.reason == Blocked::Cycle)
        .map(|u| u.a1.as_str())
        .collect();
    cycles.sort_unstable();
    assert_eq!(cycles, ["Main!B1", "Main!C1"]);
}

// ---- M11: a reverted revisit propagates too --------------------------------

#[test]
fn a_cell_that_reverts_still_makes_its_readers_revisit() {
    // D=-A, E=D, B=A+E, C=B+1. B is first reached with a stale E (level 1),
    // moves on a wrong value, and C inherits that wrong value at level 2.
    // Once E catches up (level 2) and B is revisited (level 3), B recomputes
    // to exactly its stored value — Unchanged, because -A and A cancel — but
    // C was never told: without propagating that revert, C stays stuck on
    // the stale value from level 2 forever.
    let mut sheet = Sheet::new(SheetId(0), "Main");
    sheet.set(0, 0, literal(1.0)); // A1
    sheet.set(0, 3, formula("-A1", -1.0)); // D1
    sheet.set(0, 4, formula("D1", -1.0)); // E1
    sheet.set(0, 1, formula("A1+E1", 0.0)); // B1
    sheet.set(0, 2, formula("B1+1", 1.0)); // C1
    let wb = Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };

    let impact = what_if(
        &wb,
        &[Change::new(
            CellRef::new(SheetId(0), 0, 0),
            CellValue::Number(5.0),
        )],
        &WhatIfOptions::default(),
    );

    assert!(
        impact.moved.iter().all(|m| m.a1 != "Main!C1"),
        "C1 must not be left listed as Moved on a value that no longer stands"
    );
    assert!(
        impact.moved.iter().all(|m| m.a1 != "Main!B1"),
        "B1 itself must also end reverted"
    );
    assert_eq!(after(&impact, "Main!D1"), Some(CellValue::Number(-5.0)));
    assert_eq!(after(&impact, "Main!E1"), Some(CellValue::Number(-5.0)));
}

// ---- a 3-D reference in the closure walk -----------------------------------

#[test]
fn every_sheet_of_a_3d_span_reaches_its_reader() {
    // `=SUM(Jan:Mar!B2)` reads a cell on three sheets. The cell layer used to
    // resolve that reference to `Jan!B2` alone — the sheet written first —
    // so substituting into Feb or Mar never put the summary on the frontier
    // and it came back *unaffected*: the one verdict this walk may not give a
    // cell it did not evaluate. The evaluator refuses a 3-D formula by name,
    // so the honest answer here is Blocked, not a number.
    let mut summary = Sheet::new(SheetId(3), "Summary");
    summary.set(0, 0, formula("SUM(Jan:Mar!B2)", 6.0)); // A1
    summary.set(0, 1, formula("A1*2", 12.0)); // B1, reads the blocked cell
    let month = |id: u16, name: &str, n: f64| {
        let mut sheet = Sheet::new(SheetId(id), name);
        sheet.set(1, 1, literal(n)); // B2
        sheet
    };
    let wb = Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![
            month(0, "Jan", 1.0),
            month(1, "Feb", 2.0),
            month(2, "Mar", 3.0),
            summary,
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };

    // Feb is the middle of the span: neither the sheet the reference was
    // written with first nor the one it ends on.
    let impact = what_if(
        &wb,
        &[Change::new(
            CellRef::new(SheetId(1), 1, 1),
            CellValue::Number(20.0),
        )],
        &WhatIfOptions::default(),
    );

    let summary_a1 = impact
        .unanswered
        .iter()
        .find(|u| u.a1 == "Summary!A1")
        .expect("Summary!A1 reads Feb!B2 through the span and must be reported");
    assert!(matches!(summary_a1.reason, Blocked::Formula(_)));
    assert!(
        impact.moved.iter().all(|m| m.a1 != "Summary!A1"),
        "must not also appear as Moved"
    );
    assert_eq!(
        impact
            .unanswered
            .iter()
            .find(|u| u.a1 == "Summary!B1")
            .map(|u| u.reason.clone()),
        Some(Blocked::Upstream("Summary!A1".into())),
        "and what reads the blocked cell is blocked in turn, not guessed"
    );

    // Each end of the span reaches it too, and the answer does not depend on
    // which end was written first.
    for sheet in [SheetId(0), SheetId(2)] {
        let impact = what_if(
            &wb,
            &[Change::new(
                CellRef::new(sheet, 1, 1),
                CellValue::Number(20.0),
            )],
            &WhatIfOptions::default(),
        );
        assert!(
            impact.unanswered.iter().any(|u| u.a1 == "Summary!A1"),
            "a change on {sheet} must reach the summary"
        );
    }
}
