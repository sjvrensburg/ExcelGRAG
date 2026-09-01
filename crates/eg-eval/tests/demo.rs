//! The evaluator against a workbook a different engine calculated.
//!
//! Every other test in this crate states both the formula and the answer, which
//! means the answer is only ever as right as whoever wrote the test. This one
//! is not like that. `crates/eg-fixtures` writes the demo workbook as flat ODS
//! with formulas and *no values*, and LibreOffice computes them on conversion —
//! so the numbers here were produced by an implementation that shares no code,
//! no author and no assumptions with this one.
//!
//! That makes this the cheap, committed version of `eg check` against a real
//! workbook: thousands of formulas — nested `IF`, both `VLOOKUP` forms, `PV`,
//! `ROUND`, division by zero — each compared against a second opinion. A
//! disagreement here is a real defect in one of the two, and finding out which
//! is what `sheet-oracle` and the parity tests are for.
//!
//! Regenerate with:
//!
//! ```sh
//! cargo run --release -p eg-fixtures -- --rows 2000 --out tests/fixtures/demo
//! ```

use std::path::PathBuf;

use eg_eval::check;
use eg_model::CellValue;

fn demo() -> PathBuf {
    fixture("xlsx")
}

fn fixture(extension: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/demo")
        .join(format!("impairment.{extension}"))
}

#[test]
fn every_formula_agrees_with_the_engine_that_computed_it() {
    let loaded = eg_ingest::load(demo()).expect("the demo fixture is committed");
    let (disagreements, report) = check(&loaded.workbook, None, 20);

    assert!(
        report.formulas > 10_000,
        "the fixture should be big enough to be worth sweeping, got {}",
        report.formulas
    );
    assert_eq!(
        disagreements.len(),
        0,
        "first few: {:?}",
        disagreements
            .iter()
            .take(3)
            .map(|d| loaded.workbook.cite(d.cell))
            .collect::<Vec<_>>()
    );
    assert_eq!(report.differed, 0);
    assert_eq!(report.agreed + report.unsupported, report.formulas);
}

#[test]
fn the_only_refusals_are_the_two_the_fixture_plants() {
    // Recorded exactly, the way the answers suite records its known gaps: a new
    // refusal fails this, and so does one of these starting to work without the
    // record being updated. Both are deliberate — the workbook contains one 3-D
    // reference and one call to a function this evaluator does not model — and
    // both must come back refused *by name* rather than guessed at, which is
    // the difference between "no answer" and a wrong one.
    let loaded = eg_ingest::load(demo()).expect("the demo fixture is committed");
    let (_, report) = check(&loaded.workbook, None, 0);

    let mut reasons: Vec<(String, u64)> = report.reasons.clone();
    reasons.sort();
    assert_eq!(
        reasons,
        vec![
            ("3-D reference".to_string(), 1),
            ("SUMPRODUCT()".to_string(), 1),
        ],
        "the fixture's refusals have changed"
    );
    assert_eq!(report.unsupported, 2);
}

#[test]
fn the_same_workbook_as_ods_recomputes_to_the_same_answers() {
    // The same spreadsheet, written by the same generator and converted by the
    // same LibreOffice run, so any difference here is the *reader's*, not the
    // workbook's. It is worth sweeping separately because ODS is the one format
    // whose formulas do not arrive in A1: calamine hands back OpenFormula, and
    // `eg_ingest::odf` translates it. Before that translation this sweep
    // refused all 14,010 formulas — a total failure that looked, from the
    // outside, like a workbook nothing could be said about.
    let ods = eg_ingest::load(fixture("ods")).expect("the demo fixture is committed");
    let xlsx = eg_ingest::load(demo()).expect("the demo fixture is committed");
    // A limit high enough to collect every disagreement, since the assertion
    // below is about all of them and not a sample.
    let (disagreements, report) = check(&ods.workbook, None, 1000);
    let (_, from_xlsx) = check(&xlsx.workbook, None, 0);

    assert_eq!(report.formulas, from_xlsx.formulas);
    let mut reasons = report.reasons.clone();
    let mut expected = from_xlsx.reasons.clone();
    reasons.sort();
    expected.sort();
    assert_eq!(
        reasons, expected,
        "ODS refuses different formulas from xlsx"
    );

    // ODS error cells carry LibreOffice's `calcext:value-type="error"`; the
    // vendored reader reads their rendered error code from the element body.
    // They must now agree exactly rather than being tolerated as blanks.
    assert_eq!(
        disagreements.len() as u64,
        report.differed,
        "the limit was too low to see every disagreement"
    );
    assert_eq!(report.differed, 0, "ODS must agree cell-for-cell with XLSX");
}

#[test]
fn the_value_no_index_can_hold_is_still_findable_in_the_cells() {
    // The other half of the gap `tests/fixtures/demo/answers.json` records for
    // "1612". `Impairment` has too many distinct values for a profile to keep,
    // so only its bounds reach the index and no search will ever return this
    // number — which is a fact about indexing and not about the workbook. A
    // scan of the cells says where it actually is, and this asserts the two
    // stories are about the same cell.
    let loaded = eg_ingest::load(demo()).expect("the demo fixture is committed");
    let (cells, report) = eg_eval::cells_holding(&loaded.workbook, &CellValue::Number(1612.0), 40);

    let cited: Vec<&str> = cells.iter().map(|c| c.a1.as_str()).collect();
    assert_eq!(cited, ["Debtors!L632"]);
    assert_eq!(report.matches, 1);
    // The cell is a formula: what it *holds* is what was asked for, which is
    // the cell someone quoting the figure means.
    assert!(cells[0].formula.is_some());
    // Exhaustive, which is what makes a nil answer worth anything.
    assert!(
        report.cells_scanned > 10_000,
        "scanned {}",
        report.cells_scanned
    );
}
