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

fn demo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/demo/impairment.xlsx")
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
