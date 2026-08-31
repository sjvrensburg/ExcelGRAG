//! Reading the relations a workbook states in its formulas.

use eg_eval::{infer_schema, LookupKind};
use eg_model::{Cell, CellValue, Sheet, SheetId, Workbook, WorkbookFormat};

fn formula(text: &str) -> Cell {
    Cell {
        value: CellValue::Number(0.0),
        formula: Some(text.into()),
        format: Default::default(),
    }
}

/// `Work` looks its debt types up in `Rates`, one formula per row.
fn book(lookups: &[&str]) -> Workbook {
    let mut work = Sheet::new(SheetId(0), "Work");
    work.set(0, 0, Cell::literal(CellValue::Text("Customer".into())));
    work.set(0, 2, Cell::literal(CellValue::Text("Type".into())));
    for (i, text) in lookups.iter().enumerate() {
        let row = i as u32 + 1;
        work.set(row, 0, Cell::literal(CellValue::Text(format!("c{row}"))));
        work.set(row, 2, Cell::literal(CellValue::Text("Residential".into())));
        work.set(row, 3, formula(text));
    }
    let mut rates = Sheet::new(SheetId(1), "Rates");
    for (i, (key, rate)) in [("Residential", 8.0), ("Business", 11.0)]
        .iter()
        .enumerate()
    {
        rates.set(i as u32, 0, Cell::literal(CellValue::Text(key.to_string())));
        rates.set(i as u32, 1, Cell::literal(CellValue::Number(*rate)));
    }
    Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![work, rates],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

#[test]
fn a_filled_down_vlookup_is_a_foreign_key() {
    // The whole idea: a spreadsheet has no schema and states one anyway, once
    // per row, and nothing had ever read it.
    let wb = book(&[
        "VLOOKUP(C2,Rates!$A$1:$B$2,2,FALSE)",
        "VLOOKUP(C3,Rates!$A$1:$B$2,2,FALSE)",
        "VLOOKUP(C4,Rates!$A$1:$B$2,2,FALSE)",
    ]);
    let schema = infer_schema(&wb);
    assert_eq!(schema.lookups.len(), 1, "three rows, one relation");

    let lookup = &schema.lookups[0];
    assert_eq!(lookup.kind, LookupKind::Vlookup);
    assert_eq!(lookup.cells, 3, "carrying the rows behind it");
    assert!(!lookup.approximate);
    assert_eq!(lookup.column, Some(2));

    // The key is the *column*, not the row the representative happened to sit
    // on: that is the thing that joins.
    let key = lookup.key.expect("a plain reference names one");
    assert_eq!(key.left, 2, "column C");
    assert_eq!((key.top, key.bottom), (1, 3), "over the group's rows");
    assert_eq!(wb.cite_range(lookup.table), "Rates!A1:B2");
    assert_eq!(wb.cite_range(lookup.returns.unwrap()), "Rates!B1:B2");
}

#[test]
fn a_written_but_empty_fourth_argument_is_an_exact_key_not_a_dropped_formula() {
    // `VLOOKUP(...,)` — a trailing comma, empty fourth argument — coerces to
    // FALSE the same way `calc.rs`'s evaluator now does, not the omitted
    // argument's TRUE default. Before this fix the empty literal matched
    // neither the `Bool` nor `Number` arm below and fell to `Unrecognised`,
    // dropping a declared key from schema inference entirely.
    let wb = book(&[
        "VLOOKUP(C2,Rates!$A$1:$B$2,2,)",
        "VLOOKUP(C3,Rates!$A$1:$B$2,2,)",
        "VLOOKUP(C4,Rates!$A$1:$B$2,2,)",
    ]);
    let schema = infer_schema(&wb);
    assert_eq!(
        schema.lookups.len(),
        1,
        "must not be dropped as unrecognised"
    );
    assert!(
        !schema.lookups[0].approximate,
        "empty coerces to FALSE, exact"
    );
    assert_eq!(schema.keys().count(), 1, "and so counts as a declared key");
}

#[test]
fn an_approximate_lookup_is_a_banding_and_is_flagged_as_one() {
    // Without a FALSE, the formula asks for the last row not past its argument.
    // The first column is a set of thresholds, and joining on equality would be
    // wrong — so it is recorded and marked rather than treated as a key.
    let wb = book(&[
        "VLOOKUP(C2,Rates!$A$1:$B$2,2)",
        "VLOOKUP(C3,Rates!$A$1:$B$2,2)",
    ]);
    let schema = infer_schema(&wb);
    assert_eq!(schema.lookups.len(), 1);
    assert!(schema.lookups[0].approximate);
    assert_eq!(schema.keys().count(), 0, "and it is not counted as a key");
}

#[test]
fn index_match_is_the_same_relation_written_to_survive_an_insert() {
    let wb = book(&[
        "INDEX(Rates!$B$1:$B$2,MATCH(C2,Rates!$A$1:$A$2,0))",
        "INDEX(Rates!$B$1:$B$2,MATCH(C3,Rates!$A$1:$A$2,0))",
    ]);
    let schema = infer_schema(&wb);
    assert_eq!(
        schema.lookups.len(),
        1,
        "one relation, not a table and a key"
    );

    let lookup = &schema.lookups[0];
    assert_eq!(lookup.kind, LookupKind::IndexMatch);
    assert!(!lookup.approximate, "MATCH type 0 is exact");
    assert_eq!(wb.cite_range(lookup.table), "Rates!A1:A2", "the keys");
    assert_eq!(wb.cite_range(lookup.returns.unwrap()), "Rates!B1:B2");
}

#[test]
fn a_match_that_is_not_exact_is_a_banding_too() {
    let wb = book(&[
        "INDEX(Rates!$B$1:$B$2,MATCH(C2,Rates!$A$1:$A$2,1))",
        "INDEX(Rates!$B$1:$B$2,MATCH(C3,Rates!$A$1:$A$2,1))",
    ]);
    let schema = infer_schema(&wb);
    assert!(schema.lookups[0].approximate);
}

#[test]
fn a_computed_key_names_no_column_and_says_so() {
    // `VLOOKUP(A2&C2, …)` is a real relation whose key is not a column of
    // anything. Inventing one would be worse than leaving the hole visible.
    let wb = book(&[
        "VLOOKUP(A2&C2,Rates!$A$1:$B$2,2,FALSE)",
        "VLOOKUP(A3&C3,Rates!$A$1:$B$2,2,FALSE)",
    ]);
    let schema = infer_schema(&wb);
    assert_eq!(schema.lookups.len(), 1);
    assert_eq!(schema.lookups[0].key, None);
    assert_eq!(
        schema.lookups[0].table,
        eg_model::RangeRef::new(SheetId(1), 0, 0, 1, 1),
        "the table is still known"
    );
}

#[test]
fn a_lookup_into_a_sheet_that_is_gone_is_counted_and_dropped() {
    let wb = book(&["VLOOKUP(C2,Deleted!$A$1:$B$2,2,FALSE)"]);
    let schema = infer_schema(&wb);
    assert!(schema.is_empty());
    assert_eq!(schema.unresolvable, 1);
    assert_eq!(
        schema.unrecognised, 0,
        "it was read, it just points nowhere"
    );
}

#[test]
fn a_shape_this_cannot_read_is_left_unrecognised_rather_than_approximated() {
    // A column index that is itself a formula. A schema that guesses is worse
    // than one with holes in it, because a hole is visible.
    let wb = book(&["VLOOKUP(C2,Rates!$A$1:$B$2,MATCH(\"Rate\",Rates!$A$1:$B$1,0),FALSE)"]);
    let schema = infer_schema(&wb);
    assert_eq!(schema.unrecognised, 1);
    assert!(
        schema.lookups.iter().all(|l| l.kind != LookupKind::Vlookup),
        "no VLOOKUP relation was invented"
    );
}

#[test]
fn one_relation_written_in_two_blocks_is_one_relation() {
    // A column broken by a hand-edited row groups into two runs of the same
    // shape. That is one relation carrying both blocks' cells, not two.
    let mut wb = book(&[
        "VLOOKUP(C2,Rates!$A$1:$B$2,2,FALSE)",
        "VLOOKUP(C3,Rates!$A$1:$B$2,2,FALSE)",
    ]);
    let work = wb.sheet_mut(SheetId(0)).unwrap();
    work.set(3, 2, Cell::literal(CellValue::Text("Business".into())));
    work.set(3, 3, Cell::literal(CellValue::Number(99.0)));
    work.set(4, 2, Cell::literal(CellValue::Text("Residential".into())));
    work.set(4, 3, formula("VLOOKUP(C5,Rates!$A$1:$B$2,2,FALSE)"));
    work.set(5, 2, Cell::literal(CellValue::Text("Residential".into())));
    work.set(5, 3, formula("VLOOKUP(C6,Rates!$A$1:$B$2,2,FALSE)"));

    let schema = infer_schema(&wb);
    assert_eq!(schema.lookups.len(), 1, "{:?}", schema.lookups);
    assert_eq!(schema.lookups[0].cells, 4, "both blocks counted");
}

#[test]
fn a_workbook_that_looks_nothing_up_has_no_schema_and_no_complaint() {
    let mut sheet = Sheet::new(SheetId(0), "Plain");
    sheet.set(0, 0, Cell::literal(CellValue::Number(1.0)));
    sheet.set(1, 0, formula("A1*2"));
    let wb = Workbook {
        path: "plain.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let schema = infer_schema(&wb);
    assert!(schema.is_empty());
    assert_eq!(schema.with_lookups, 0);
    assert_eq!(schema.unrecognised, 0);
    assert!(schema.groups > 0, "it did look");
}
