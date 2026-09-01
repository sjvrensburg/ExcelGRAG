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

use eg_eval::{check, Outcome};
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

    // Every remaining disagreement is one defect, recorded rather than
    // tolerated: ODF marks an error cell with `calcext:value-type="error"` and
    // an empty `office:string-value`, and calamine reads the empty string, so
    // the `#DIV/0!` this fixture plants arrives as a blank. The evaluator is
    // right and the stored value is missing — which is exactly the shape a
    // reader defect takes, and why `eg check` finds them.
    let unexplained: Vec<&str> = disagreements
        .iter()
        .filter(|d| {
            !matches!(
                &d.outcome,
                Outcome::Differs {
                    computed: CellValue::Error(_),
                    stored: CellValue::Empty | CellValue::Text(_),
                }
            )
        })
        .map(|d| d.a1.as_str())
        .collect();
    assert!(
        unexplained.is_empty(),
        "ODS disagrees for a reason other than the error-cell gap: {unexplained:?}"
    );
    assert_eq!(
        disagreements.len() as u64,
        report.differed,
        "the limit was too low to see every disagreement"
    );
    assert!(
        report.differed > 0,
        "the ODS error-cell gap has closed — this workbook plants a #DIV/0! and \
         calamine now reads it, so delete the allowance and issue 10 with it"
    );
}
