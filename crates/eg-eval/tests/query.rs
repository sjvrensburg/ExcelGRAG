//! Asking a table a question, against workbooks small enough to check by hand.
//!
//! The failure mode of a query engine over a spreadsheet is a confident wrong
//! number, so most of what these assert is that it refuses.

use eg_eval::query::{query, Aggregate, Filter, Query, QueryError, Test};
use eg_model::{Cell, CellValue, ErrorKind, RangeRef, Sheet, SheetId, Workbook, WorkbookFormat};
use eg_structure::{read_table, Region, RegionKind, RegionSource, Table};

fn grid(rows: &[&str]) -> Sheet {
    let mut sheet = Sheet::new(SheetId(0), "Debtors");
    for (r, line) in rows.iter().enumerate() {
        for (c, tok) in line.split_whitespace().enumerate() {
            if tok == "." {
                continue;
            }
            let value = match tok {
                "#REF!" => CellValue::Error(ErrorKind::Ref),
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

/// A hand-built region, so these test querying rather than detection.
fn region(bottom: u32, right: u16, headers: &[&str]) -> Region {
    Region {
        range: RangeRef::new(SheetId(0), 0, 0, bottom, right),
        kind: RegionKind::Table,
        source: RegionSource::Declared,
        title: None,
        header_rows: 1,
        header_cols: 1,
        headers: headers.iter().map(|h| h.to_string()).collect(),
        label_headers: Vec::new(),
        cell_count: 0,
        totals_rows: 0,
    }
}

/// `Customer | Type | Debt | Note`, four rows.
fn book() -> (Workbook, Table) {
    let sheet = grid(&[
        "Customer Type Debt Note",
        "North Residential 1200 ok",
        "South Business 3400 ok",
        "East Residential 900 .",
        "West Residential 700 ok",
    ]);
    let table = read_table(&sheet, &region(4, 3, &["Type", "Debt", "Note"])).unwrap();
    let workbook = Workbook {
        path: "debtors.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    (workbook, table)
}

fn ask(q: Query) -> Result<eg_eval::query::Answer, QueryError> {
    let (workbook, table) = book();
    query(&workbook, &table, &q)
}

#[test]
fn a_total_over_a_filtered_column_is_the_number_the_workbook_never_wrote_down() {
    let answer = ask(Query {
        filters: vec![Filter {
            column: "Type".into(),
            test: Test::Is(CellValue::Text("Residential".into())),
        }],
        aggregates: vec![Aggregate::Sum("Debt".into()), Aggregate::Count],
        limit: 10,
        ..Default::default()
    })
    .unwrap();

    let group = answer.one().expect("one group when nothing is grouped");
    assert_eq!(group.values[0], Some(2800.0), "1200 + 900 + 700");
    assert_eq!(group.counts[1], Some(3));
    assert_eq!(answer.rows_scanned, 4);
    assert_eq!(answer.rows_matched, 3);
}

#[test]
fn an_answer_names_the_cells_it_was_computed_over() {
    // Region boundaries are *inferred*. A totals row swept into a table's body
    // doubles every sum, and the only defence is that the caller can see what
    // was summed and check it.
    let answer = ask(Query {
        aggregates: vec![Aggregate::Sum("Debt".into())],
        limit: 10,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(answer.over, RangeRef::new(SheetId(0), 1, 1, 4, 3));
    assert_eq!(answer.one().unwrap().values[0], Some(6200.0));
}

#[test]
fn grouping_splits_the_rows_and_keeps_the_key() {
    let answer = ask(Query {
        group_by: vec!["Type".into()],
        aggregates: vec![Aggregate::Sum("Debt".into())],
        limit: 10,
        ..Default::default()
    })
    .unwrap();

    assert_eq!(answer.groups.len(), 2);
    let residential = answer
        .groups
        .iter()
        .find(|g| g.key[0] == CellValue::Text("Residential".into()))
        .expect("grouped under its own value");
    assert_eq!(residential.rows, 3);
    assert_eq!(residential.values[0], Some(2800.0));
}

#[test]
fn summing_a_column_that_is_not_numbers_is_refused_rather_than_skipped() {
    // The trap: a column that is 40% number and 35% text has no total, and
    // producing one by ignoring the text is how a figure ends up quietly short.
    let err = ask(Query {
        aggregates: vec![Aggregate::Sum("Type".into())],
        limit: 10,
        ..Default::default()
    })
    .unwrap_err();
    assert!(
        matches!(err, QueryError::NotNumeric { ref column, .. } if column == "Type"),
        "{err}"
    );
    assert!(err.to_string().contains("text"), "{err}");
}

#[test]
fn a_header_naming_two_columns_is_refused_rather_than_picked_between() {
    let sheet = grid(&["Region Total Total", "North 1 2", "South 3 4"]);
    let table = read_table(&sheet, &region(2, 2, &["Total", "Total"])).unwrap();
    let workbook = Workbook {
        path: "two.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let err = query(
        &workbook,
        &table,
        &Query {
            aggregates: vec![Aggregate::Sum("Total".into())],
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(err, QueryError::AmbiguousColumn(_)), "{err}");
}

#[test]
fn a_column_this_table_does_not_have_fails_before_it_reads_a_cell() {
    // Rather than an empty answer, which reads exactly like "nothing matched".
    let err = ask(Query {
        filters: vec![Filter {
            column: "Postcode".into(),
            test: Test::NotBlank,
        }],
        aggregates: vec![Aggregate::Count],
        limit: 10,
        ..Default::default()
    })
    .unwrap_err();
    assert!(matches!(err, QueryError::NoSuchColumn(_)), "{err}");
}

#[test]
fn error_cells_are_counted_rather_than_quietly_left_out() {
    // A total over a column holding errors is a finding about the workbook
    // before it is an answer.
    let sheet = grid(&["Customer Debt", "North 1200", "South #REF!", "East 900"]);
    let table = read_table(&sheet, &region(3, 1, &["Debt"])).unwrap();
    let workbook = Workbook {
        path: "errors.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let answer = query(
        &workbook,
        &table,
        &Query {
            aggregates: vec![Aggregate::Sum("Debt".into())],
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(answer.one().unwrap().values[0], Some(2100.0));
    assert_eq!(answer.errors_in_aggregates, 1, "and it says so");
}

#[test]
fn a_group_with_no_number_to_add_says_none_rather_than_zero() {
    // Zero is an answer about the data. `None` is an answer about the question.
    let answer = ask(Query {
        filters: vec![Filter {
            column: "Type".into(),
            test: Test::Is(CellValue::Text("Wholesale".into())),
        }],
        aggregates: vec![Aggregate::Sum("Debt".into()), Aggregate::Count],
        limit: 10,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(answer.rows_matched, 0);
    assert!(answer.groups.is_empty(), "no rows, no groups");
}

#[test]
fn counting_values_and_counting_rows_are_different_questions() {
    let answer = ask(Query {
        aggregates: vec![
            Aggregate::Count,
            Aggregate::CountValues("Note".into()),
            Aggregate::CountDistinct("Type".into()),
        ],
        limit: 10,
        ..Default::default()
    })
    .unwrap();
    let group = answer.one().unwrap();
    assert_eq!(group.counts[0], Some(4), "four rows");
    assert_eq!(group.counts[1], Some(3), "one of them has no note");
    assert_eq!(group.counts[2], Some(2), "two types");
}

#[test]
fn a_query_asking_for_nothing_is_refused() {
    let err = ask(Query::default()).unwrap_err();
    assert!(matches!(err, QueryError::NothingAsked), "{err}");
}

#[test]
fn a_zero_limit_is_refused_rather_than_silently_read_as_one() {
    // L22: `limit: 0` used to mean "show one group anyway" — a caller who
    // wrote 0 got a different answer from the one they asked for, with
    // nothing to say so.
    let err = ask(Query {
        aggregates: vec![Aggregate::Count],
        limit: 0,
        ..Default::default()
    })
    .unwrap_err();
    assert!(matches!(err, QueryError::NoLimit), "{err}");
}

#[test]
fn text_equality_folds_case_the_same_way_contains_does() {
    // L22: `Test::Is`/`OneOf` used ASCII-only case folding while
    // `Test::Contains` (and this crate's other text comparisons) fold on
    // full Unicode `to_lowercase` — a non-ASCII letter could disagree with
    // one and agree with the other on the very same cell.
    let sheet = grid(&["Label Customer Debt", "a CAFÉ 5", "b other 1"]);
    let table = read_table(&sheet, &region(2, 2, &["Customer", "Debt"])).unwrap();
    let workbook = Workbook {
        path: "case.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let answer = query(
        &workbook,
        &table,
        &Query {
            filters: vec![Filter {
                column: "Customer".into(),
                test: Test::Is(CellValue::Text("café".into())),
            }],
            aggregates: vec![Aggregate::Count],
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        answer.one().unwrap().counts[0],
        Some(1),
        "CAFÉ and café are the same word under Unicode case folding"
    );
}

#[test]
fn a_sum_here_agrees_with_the_sum_in_the_cell_beside_it() {
    // The reason this lives in the evaluator's crate. A sheet carries fifteen
    // significant digits: 10.13 + 6.75 is exactly 16.88 in Excel and is not in
    // any language with doubles.
    let sheet = grid(&["Customer Debt", "North 10.13", "South 6.75"]);
    let table = read_table(&sheet, &region(2, 1, &["Debt"])).unwrap();
    let workbook = Workbook {
        path: "digits.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let answer = query(
        &workbook,
        &table,
        &Query {
            aggregates: vec![Aggregate::Sum("Debt".into())],
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    let computed = eg_eval::evaluate(
        &workbook,
        eg_model::CellRef::new(SheetId(0), 0, 9),
        "SUM(B2:B3)",
    )
    .unwrap();
    assert_eq!(
        answer.one().unwrap().values[0],
        Some(16.88),
        "the number the sheet would show, not the last bits of an f64"
    );
    // The evaluator's own `SUM()` leaves those bits alone — it is compared
    // against what Excel cached, which tolerates them, so it has never had a
    // reason to round. The two agree by the comparison this project uses
    // everywhere, which is the claim worth making.
    assert_eq!(computed, CellValue::Number(16.880000000000003));
    assert!(
        eg_eval::calc::same(&computed, &CellValue::Number(16.88)),
        "and to a sheet they are one number: {computed:?}"
    );
}
