//! Recomputation against workbooks small enough to check by hand.

use eg_eval::{check, evaluate, parse, recompute, Outcome, Unsupported};
use eg_model::{
    Cell, CellRef, CellValue, DefinedName, ErrorKind, RangeRef, Sheet, SheetId, Workbook,
    WorkbookFormat,
};

/// A sheet from a grid of tokens: `=…` is a formula, a number is a number,
/// `.` is an unpopulated cell, anything else is text. A formula cell's stored
/// value is given separately, because the whole point here is comparing the two.
fn grid(id: u16, name: &str, rows: &[&str]) -> Sheet {
    let mut sheet = Sheet::new(SheetId(id), name);
    for (r, line) in rows.iter().enumerate() {
        for (c, token) in line.split_whitespace().enumerate() {
            if token == "." {
                continue;
            }
            let cell = match token.parse::<f64>() {
                Ok(n) => Cell::literal(CellValue::Number(n)),
                Err(_) => Cell::literal(CellValue::Text(token.to_string())),
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
            grid(0, "Main", &["1 2 3", "4 5 6", "text . 7"]),
            grid(1, "Rates", &["a 10", "b 20", "c 30"]),
        ],
        // `RATES` is defined twice: once for the workbook, once scoped to
        // Main, which is how a real workbook ends up with two of them.
        defined_names: vec![
            DefinedName {
                name: "RATES".into(),
                refers_to: "=Rates!$B$1:$B$3".into(),
                scope: None,
            },
            DefinedName {
                name: "RATES".into(),
                refers_to: "=Main!$A$1:$C$1".into(),
                scope: Some(SheetId(0)),
            },
            DefinedName {
                name: "GROWTH".into(),
                refers_to: "=1.5".into(),
                scope: None,
            },
        ],
        external_links: Vec::new(),
    }
}

fn at(row: u32, col: u16) -> CellRef {
    CellRef::new(SheetId(0), row, col)
}

/// Evaluate a formula as if it sat in an unused cell of `Main`.
fn eval(formula: &str) -> CellValue {
    evaluate(&book(), at(20, 20), formula).expect("supported")
}

fn number(formula: &str) -> f64 {
    match eval(formula) {
        CellValue::Number(n) => n,
        other => panic!("{formula} gave {other:?}, wanted a number"),
    }
}

fn error(formula: &str) -> ErrorKind {
    match eval(formula) {
        CellValue::Error(e) => e,
        other => panic!("{formula} gave {other:?}, wanted an error"),
    }
}

fn refused(formula: &str) -> Unsupported {
    evaluate(&book(), at(20, 20), formula).expect_err("unsupported")
}

// ---- arithmetic ---------------------------------------------------------

#[test]
fn arithmetic_follows_excels_precedence() {
    assert_eq!(number("1+2*3"), 7.0);
    assert_eq!(number("(1+2)*3"), 9.0);
    assert_eq!(number("2^3^2"), 512.0, "^ is right-associative");
    assert_eq!(number("-2^2"), 4.0, "a sign binds tighter than ^");
    assert_eq!(number("2^-1"), 0.5);
    assert_eq!(number("50%"), 0.5);
    assert_eq!(number("-50%*2"), -1.0);
    assert_eq!(number("10-2-3"), 5.0, "- is left-associative");
}

#[test]
fn division_by_zero_is_an_answer_not_a_failure() {
    assert_eq!(error("1/0"), ErrorKind::Div0);
    assert_eq!(error("LN(0)"), ErrorKind::Num);
    assert_eq!(
        eval("IFERROR(1/0,\"caught\")"),
        CellValue::Text("caught".into())
    );
}

#[test]
fn errors_propagate_through_operators() {
    assert_eq!(error("1+#REF!"), ErrorKind::Ref);
    assert_eq!(error("#N/A&\"x\""), ErrorKind::NA);
    assert_eq!(error("\"abc\"+1"), ErrorKind::Value);
    // Text that looks like a number is coerced; text that does not is #VALUE!.
    assert_eq!(number("\"2\"+1"), 3.0);
}

#[test]
fn comparison_uses_excels_type_order() {
    assert_eq!(
        eval("1<\"a\""),
        CellValue::Bool(true),
        "numbers sort below text"
    );
    assert_eq!(
        eval("\"a\"=\"A\""),
        CellValue::Bool(true),
        "text ignores case"
    );
    assert_eq!(eval("EXACT(\"a\",\"A\")"), CellValue::Bool(false));
    assert_eq!(
        eval("\"x\"<TRUE"),
        CellValue::Bool(true),
        "text sorts below booleans"
    );
}

#[test]
fn concatenation_renders_numbers_as_a_sheet_would() {
    assert_eq!(eval("1&\"x\""), CellValue::Text("1x".into()));
    assert_eq!(eval("A1&\"\""), CellValue::Text("1".into()));
    // An unpopulated cell concatenates as nothing, not as zero.
    assert_eq!(eval("B3&\"y\""), CellValue::Text("y".into()));
}

#[test]
fn rounding_is_half_away_from_zero_on_the_shown_number() {
    assert_eq!(number("ROUND(2.5,0)"), 3.0);
    assert_eq!(number("ROUND(-2.5,0)"), -3.0);
    // The nearest double to 2.675 is a hair below it; Excel rounds the number
    // it shows, and a workbook full of ROUND depends on that.
    assert_eq!(number("ROUND(2.675,2)"), 2.68);
    assert_eq!(number("ROUND(1234.5,-2)"), 1200.0);
    assert_eq!(number("ROUNDUP(1.001,2)"), 1.01);
    assert_eq!(number("ROUNDDOWN(-1.009,2)"), -1.0);
    assert_eq!(
        number("MOD(-3,2)"),
        1.0,
        "MOD takes the sign of the divisor"
    );
}

// ---- references and aggregation -----------------------------------------

#[test]
fn aggregates_read_the_grid() {
    assert_eq!(number("SUM(A1:C2)"), 21.0);
    assert_eq!(number("SUM(A1:C3)"), 28.0, "text in a range is ignored");
    assert_eq!(number("AVERAGE(A1:C1)"), 2.0);
    assert_eq!(number("MIN(A1:C2)"), 1.0);
    assert_eq!(number("MAX(A1:C2)"), 6.0);
    assert_eq!(number("COUNT(A1:C3)"), 7.0);
    assert_eq!(
        number("COUNTA(A1:C3)"),
        8.0,
        "text counts, the blank does not"
    );
    assert_eq!(number("COUNTBLANK(A1:C3)"), 1.0);
    assert_eq!(number("SUM(Rates!B1:B3)"), 60.0);
    assert_eq!(number("SUM(rates!B1:B3)"), 60.0, "sheet names ignore case");
}

#[test]
fn text_and_booleans_count_only_when_written_into_the_formula() {
    // Excel's rule, and it is not obvious: SUM(TRUE) is 1, SUM of a range
    // holding TRUE is 0.
    assert_eq!(number("SUM(TRUE,\"2\")"), 3.0);
    assert_eq!(
        number("SUM(A3)"),
        0.0,
        "text in a referenced cell is ignored"
    );
    assert_eq!(error("SUM(A3&\"\")+1"), ErrorKind::Value);
}

#[test]
fn a_reference_to_a_missing_sheet_is_a_ref_error() {
    assert_eq!(error("Gone!A1"), ErrorKind::Ref);
    assert_eq!(error("SUM(Gone!A1:A9)"), ErrorKind::Ref);
}

#[test]
fn an_empty_cell_is_zero_in_arithmetic_and_blank_in_text() {
    assert_eq!(number("B3+1"), 1.0);
    assert_eq!(
        eval("B3"),
        CellValue::Number(0.0),
        "a bare blank reads as 0"
    );
    assert_eq!(eval("ISBLANK(B3)"), CellValue::Bool(true));
    assert_eq!(eval("LEN(B3)"), CellValue::Number(0.0));
}

#[test]
fn many_cells_where_one_value_was_needed_is_refused_not_guessed() {
    // Excel would intersect the range with the formula's own row, which
    // depends on where the formula sits rather than on what it says.
    assert!(matches!(refused("A1:C1+1"), Unsupported::RangeAsValue(_)));
}

// ---- logic, lookup ------------------------------------------------------

#[test]
fn the_branch_not_taken_is_not_evaluated() {
    // The dead branch calls something unmodelled, and the answer is still
    // defensible, so refusing the whole formula would be wrong.
    assert_eq!(number("IF(TRUE,1,SUMPRODUCT(A1:A2,B1:B2))"), 1.0);
    assert!(matches!(
        refused("IF(FALSE,1,SUMPRODUCT(A1:A2,B1:B2))"),
        Unsupported::Function(name) if name == "SUMPRODUCT"
    ));
}

#[test]
fn logical_functions_ignore_text_in_ranges() {
    assert_eq!(eval("AND(1,TRUE)"), CellValue::Bool(true));
    assert_eq!(eval("OR(FALSE,A1)"), CellValue::Bool(true));
    assert_eq!(eval("NOT(A1)"), CellValue::Bool(false));
    assert_eq!(
        eval("AND(A1:C3)"),
        CellValue::Bool(true),
        "the text cell is skipped"
    );
}

#[test]
fn lookups_walk_the_table() {
    assert_eq!(number("VLOOKUP(\"b\",Rates!A1:B3,2,FALSE)"), 20.0);
    assert_eq!(error("VLOOKUP(\"z\",Rates!A1:B3,2,FALSE)"), ErrorKind::NA);
    assert_eq!(error("VLOOKUP(\"b\",Rates!A1:B3,3,FALSE)"), ErrorKind::Ref);
    assert_eq!(number("MATCH(\"c\",Rates!A1:A3,0)"), 3.0);
    assert_eq!(number("INDEX(Rates!A1:B3,2,2)"), 20.0);
    // An approximate VLOOKUP takes the last row not past the key.
    assert_eq!(number("VLOOKUP(5,Main!A1:C2,3)"), 6.0);
}

#[test]
fn index_over_a_whole_column_is_refused() {
    assert!(matches!(
        refused("INDEX(Rates!A1:B3,0,2)"),
        Unsupported::Form(_)
    ));
}

// ---- what is deliberately not modelled ----------------------------------

#[test]
fn unmodelled_constructs_say_which_one_they_are() {
    assert!(matches!(
        refused("SUMPRODUCT(A1:A2,B1:B2)"),
        Unsupported::Function(_)
    ));
    assert!(matches!(refused("TODAY()"), Unsupported::Volatile(_)));
    assert!(matches!(refused("Tax_Rate*2"), Unsupported::Name(_)));
    // A name standing for a constant rather than a reference is refused:
    // following it would evaluate a second formula inside this cell.
    assert!(matches!(refused("GROWTH*2"), Unsupported::Name(_)));
    assert!(matches!(
        refused("[1]Other!A1"),
        Unsupported::ExternalWorkbook(_)
    ));
    assert!(matches!(
        refused("SUM('Main:Rates'!A1)"),
        Unsupported::ThreeDReference(_)
    ));
    assert!(matches!(refused("SUM({1,2})"), Unsupported::Unparsed(_)));
    assert!(matches!(
        refused("SUM(Table1[Amount])"),
        Unsupported::Unparsed(_)
    ));
    assert!(matches!(refused("SUM(A:A)"), Unsupported::Unparsed(_)));
}

#[test]
fn a_defined_name_resolves_the_way_excel_scopes_it() {
    // The sheet's own name wins over the workbook's, and a qualified name says
    // which scope it wants.
    assert_eq!(number("SUM(RATES)"), 6.0, "Main's RATES is A1:C1");
    assert_eq!(
        number("SUM(Rates!RATES)"),
        60.0,
        "no such sheet-scoped name, so the workbook's"
    );
    assert_eq!(number("SUM('Rates'!RATES)"), 60.0);
    assert_eq!(error("SUM(Gone!RATES)"), ErrorKind::Ref);
}

#[test]
fn a_name_is_cited_as_written_and_as_resolved() {
    let wb = book();
    let mut sheet = Sheet::new(SheetId(2), "Calc");
    sheet.set(
        0,
        0,
        Cell {
            value: CellValue::Number(60.0),
            formula: Some("SUM(RATES)".into()),
            format: Default::default(),
        },
    );
    let mut wb = wb;
    wb.sheets.push(sheet);
    let result = recompute(&wb, CellRef::new(SheetId(2), 0, 0)).unwrap();
    assert_eq!(result.outcome, Outcome::Agrees(CellValue::Number(60.0)));
    assert_eq!(result.inputs[0].text, "RATES");
    assert_eq!(result.inputs[0].a1, "Rates!B1:B3");
}

#[test]
fn a_reason_groups_by_what_would_have_to_be_built() {
    assert_eq!(refused("SUMPRODUCT(A1:A2,B1:B2)").key(), "SUMPRODUCT()");
    assert_eq!(refused("Tax_Rate*2").key(), "defined name");
    assert_eq!(
        refused("SUM({1,2})").key(),
        "unparsed: array literal",
        "the offset varies from formula to formula; the construct does not"
    );
}

#[test]
fn nesting_is_bounded_rather_than_recursed_into_the_stack() {
    let deep = format!("{}1{}", "SUM(".repeat(500), ")".repeat(500));
    assert!(matches!(
        evaluate(&book(), at(20, 20), &deep),
        Err(Unsupported::Unparsed(_)) | Err(Unsupported::TooDeep)
    ));
}

// ---- the verdict --------------------------------------------------------

/// A workbook whose formula cells carry a stored value, so agreement is a real
/// question. `D1` is stale on purpose.
fn checked_book() -> Workbook {
    let mut wb = book();
    let sheet = wb.sheet_mut(SheetId(0)).unwrap();
    sheet.set(
        0,
        3,
        Cell {
            value: CellValue::Number(999.0),
            formula: Some("A1+B1".into()),
            format: Default::default(),
        },
    );
    sheet.set(
        1,
        3,
        Cell {
            value: CellValue::Number(9.0),
            formula: Some("A2+B2".into()),
            format: Default::default(),
        },
    );
    sheet.set(
        2,
        3,
        Cell {
            value: CellValue::Number(1.0),
            formula: Some("SUMPRODUCT(A1:A2,B1:B2)".into()),
            format: Default::default(),
        },
    );
    wb
}

#[test]
fn a_stale_value_differs_and_a_live_one_agrees() {
    let wb = checked_book();
    let stale = recompute(&wb, at(0, 3)).expect("a formula cell");
    assert_eq!(stale.a1, "Main!D1");
    assert_eq!(
        stale.outcome,
        Outcome::Differs {
            computed: CellValue::Number(3.0),
            stored: CellValue::Number(999.0),
        }
    );
    let live = recompute(&wb, at(1, 3)).expect("a formula cell");
    assert_eq!(live.outcome, Outcome::Agrees(CellValue::Number(9.0)));
}

#[test]
fn a_verdict_carries_the_cells_it_stands_on() {
    let wb = checked_book();
    let result = recompute(&wb, at(0, 3)).unwrap();
    let inputs: Vec<(&str, &str)> = result
        .inputs
        .iter()
        .map(|i| (i.text.as_str(), i.a1.as_str()))
        .collect();
    assert_eq!(inputs, [("A1", "Main!A1"), ("B1", "Main!B1")]);
    assert_eq!(result.inputs[0].value, Some(CellValue::Number(1.0)));
    assert_eq!(result.inputs[0].cells, 1);
}

#[test]
fn a_literal_has_nothing_to_recompute() {
    let wb = checked_book();
    assert!(recompute(&wb, at(0, 0)).is_none());
    assert!(recompute(&wb, at(9, 9)).is_none(), "an absent cell");
}

#[test]
fn an_unmodelled_function_is_refused_before_it_reads_anything() {
    // The inputs are what the evaluation resolved, and it resolved nothing:
    // an unknown function is refused at the name. What the formula names is
    // `precedents_of`'s question, and it answers it without evaluating.
    let wb = checked_book();
    let result = recompute(&wb, at(2, 3)).unwrap();
    assert!(matches!(result.outcome, Outcome::Unsupported(_)));
    assert!(result.inputs.is_empty());
    assert_eq!(eg_eval::precedents_of(&wb, at(2, 3)).len(), 2);
}

#[test]
fn a_sweep_counts_everything_and_lists_the_disagreements() {
    let wb = checked_book();
    let (differed, report) = check(&wb, None, 10);
    assert_eq!(report.formulas, 3);
    assert_eq!(report.agreed, 1);
    assert_eq!(report.differed, 1);
    assert_eq!(report.unsupported, 1);
    assert_eq!(report.reasons, [("SUMPRODUCT()".to_string(), 1)]);
    assert!(!report.capped);
    assert_eq!(differed.len(), 1);
    assert_eq!(differed[0].a1, "Main!D1");
}

#[test]
fn a_sweep_caps_its_list_but_not_its_counts() {
    let wb = checked_book();
    let (differed, report) = check(&wb, None, 0);
    assert!(differed.is_empty());
    assert_eq!(report.differed, 1);
    assert!(report.capped);
}

#[test]
fn a_sweep_can_be_confined_to_a_range() {
    let wb = checked_book();
    let scope = RangeRef::parse_local("D2:D3", SheetId(0)).unwrap();
    let (_, report) = check(&wb, Some(scope), 10);
    assert_eq!(report.formulas, 2);
    assert_eq!(report.agreed, 1);
    assert_eq!(report.differed, 0);
}

// ---- the parser ---------------------------------------------------------

#[test]
fn a_reference_is_the_one_the_scanner_found() {
    // `1E5` is a number, not a reference to E5, and the parser must agree with
    // the scanner the graph is built from.
    assert_eq!(number("1E5"), 100000.0);
    assert_eq!(eval("\"A1\""), CellValue::Text("A1".into()));
    assert!(parse("SUM('Q3 Sales'!A1:B2)").is_ok());
}

#[test]
fn an_omitted_argument_is_not_a_missing_one() {
    assert_eq!(number("IF(FALSE,1,)"), 0.0);
    assert_eq!(eval("IF(TRUE,1,)"), CellValue::Number(1.0));
}

#[test]
fn comparison_is_made_on_the_number_a_sheet_shows() {
    // False in binary floating point, true in every spreadsheet ever written.
    // A formula that tests it — an ageing bucket picking its label — takes the
    // other branch if the raw doubles are compared.
    assert_eq!(number("10.13+6.75"), 16.880000000000003);
    assert_eq!(eval("(10.13+6.75)=16.88"), CellValue::Bool(true));
    assert_eq!(eval("0.1+0.2=0.3"), CellValue::Bool(true));
    assert_eq!(eval("1.0000000000000002=1"), CellValue::Bool(true));
    assert_eq!(eval("1.00001=1"), CellValue::Bool(false));
}

#[test]
fn a_unary_plus_passes_its_operand_through() {
    // Lotus-style `=+…` formulas are everywhere in real workbooks, and the
    // leading plus must not turn text into a #VALUE!.
    assert_eq!(eval("+\"3 Points\""), CellValue::Text("3 Points".into()));
    assert_eq!(number("+1"), 1.0);
    assert_eq!(eval("+A3&\"!\""), CellValue::Text("text!".into()));
}

#[test]
fn a_long_lookup_column_answers_the_same_as_a_short_one() {
    // Past a threshold a lookup stops walking its column and indexes it, and
    // the two paths have to agree — including on case, on which duplicate wins,
    // and on a number stored as the double just above the one written.
    let mut sheet = Sheet::new(SheetId(0), "Big");
    for row in 0..2000u32 {
        sheet.set(row, 0, Cell::literal(CellValue::Text(format!("k{row}"))));
        sheet.set(row, 1, Cell::literal(CellValue::Number(row as f64)));
    }
    // A duplicate key far down the column: the first row must still win.
    sheet.set(1999, 0, Cell::literal(CellValue::Text("k7".into())));
    sheet.set(1500, 2, Cell::literal(CellValue::Number(10.13 + 6.75)));
    let wb = Workbook {
        path: "big.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let at = CellRef::new(SheetId(0), 3000, 9);
    let value = |formula: &str| evaluate(&wb, at, formula).expect("supported");

    assert_eq!(
        value("VLOOKUP(\"k7\",A1:B2000,2,FALSE)"),
        CellValue::Number(7.0)
    );
    assert_eq!(
        value("VLOOKUP(\"K7\",A1:B2000,2,FALSE)"),
        CellValue::Number(7.0)
    );
    assert_eq!(
        value("VLOOKUP(\"k7\",A1:B400,2,FALSE)"),
        CellValue::Number(7.0)
    );
    assert_eq!(
        value("MATCH(\"k1234\",A1:A2000,0)"),
        CellValue::Number(1235.0)
    );
    assert_eq!(
        value("VLOOKUP(\"nope\",A1:B2000,2,FALSE)"),
        CellValue::Error(ErrorKind::NA)
    );
    assert_eq!(value("MATCH(16.88,C1:C2000,0)"), CellValue::Number(1501.0));
    // The approximate form is not indexed, since it asks an ordering question.
    assert_eq!(value("VLOOKUP(1234,B1:B2000,1)"), CellValue::Number(1234.0));
}

#[test]
fn operands_that_cancel_at_fifteen_digits_subtract_to_zero() {
    // Two numbers a sheet shows identically are the same number, so their
    // difference is zero rather than the 1.49e-8 between the doubles. Excel
    // does this, and a column of differences reads as empty because of it.
    assert_eq!(number("49276148.73000001-49276148.73"), 0.0);
    assert_eq!(number("49276148.73000001+(0-49276148.73)"), 0.0);
    // A difference a sheet can show survives untouched.
    assert_eq!(number("49276148.7301-49276148.73"), 0.00010000169277191162);
    assert_eq!(number("1-0.9"), 0.09999999999999998);
    assert_eq!(number("0.1+0.2"), 0.30000000000000004);
}
