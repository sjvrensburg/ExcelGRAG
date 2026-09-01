//! Format-parity and round-trip tests.
//!
//! The parity tests are the most valuable in the suite: XLSB is the reason the
//! project is written in Rust, and a silent regression in XLSB handling would
//! otherwise only surface on a user's large confidential workbook, where it is
//! hardest to debug.
//!
//! The `.xlsb`/`.xlsx` pairs under `tests/fixtures/vendor` were authored by real
//! Excel, because nothing open-source can write XLSB.

use std::collections::BTreeMap;
use std::path::PathBuf;

use eg_ingest::{load, Capabilities};
use eg_model::{scan_references, CellValue, WorkbookFormat};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

fn vendor(name: &str) -> PathBuf {
    fixture("vendor").join(name)
}

/// Values compare equal if they agree; numeric comparison tolerates the
/// round-trip noise between XLSB's binary doubles and XLSX's decimal text.
fn values_agree(a: &CellValue, b: &CellValue) -> bool {
    match (a, b) {
        (CellValue::Number(x), CellValue::Number(y)) => {
            (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
        }
        _ => a == b,
    }
}

/// Compare two loads of the same logical workbook, sheet by sheet.
///
/// Sheets are matched by name, not position: calamine reports XLSB sheets in a
/// different order than XLSX for the same workbook, so comparing positionally
/// would report spurious mismatches. Only sheets present in both are compared,
/// because the vendored fixtures do not all carry the same sheet set.
fn assert_agree(label: &str, a: &eg_ingest::Loaded, b: &eg_ingest::Loaded) {
    let mut compared = 0usize;
    let mut mismatches = Vec::new();

    for sheet_a in &a.workbook.sheets {
        let Some(sheet_b) = b.workbook.sheet_by_name(&sheet_a.name) else {
            continue;
        };
        compared += 1;

        let mut coords: Vec<(u32, u16)> = sheet_a
            .iter()
            .chain(sheet_b.iter())
            .map(|(r, _)| (r.row, r.col))
            .collect();
        coords.sort_unstable();
        coords.dedup();

        for (row, col) in coords {
            let a1 = eg_model::CellRef::new(sheet_a.id, row, col).to_a1();

            let va = sheet_a.value(row, col);
            let vb = sheet_b.value(row, col);
            if !values_agree(&va, &vb) {
                mismatches.push(format!("{}!{a1} value: {va:?} vs {vb:?}", sheet_a.name));
            }

            // Formula parity is the load-bearing assertion. The binary formats
            // store formulas as token streams, and both losing them and
            // mis-decoding an operator are silent failures that leave the
            // cached values looking perfectly correct.
            let fa = sheet_a.get(row, col).and_then(|c| c.formula.as_deref());
            let fb = sheet_b.get(row, col).and_then(|c| c.formula.as_deref());
            if fa != fb {
                mismatches.push(format!("{}!{a1} formula: {fa:?} vs {fb:?}", sheet_a.name));
            }
        }
    }

    assert!(compared > 0, "{label}: no sheets in common to compare");
    assert!(
        mismatches.is_empty(),
        "{label}: {} mismatches across {compared} sheets:\n  {}",
        mismatches.len(),
        mismatches.join("\n  ")
    );
}

#[test]
fn all_formats_agree_on_values_and_formulas() {
    // issues.* exists in all three formats; the other two pair xlsx with xlsb.
    for base in ["issues", "any_sheets", "issue_419"] {
        let xlsx = load(vendor(&format!("{base}.xlsx"))).expect("load xlsx");
        let xlsb = load(vendor(&format!("{base}.xlsb"))).expect("load xlsb");
        assert_agree(&format!("{base}: xlsx vs xlsb"), &xlsx, &xlsb);

        let xls_path = vendor(&format!("{base}.xls"));
        if xls_path.exists() {
            let xls = load(&xls_path).expect("load xls");
            assert_agree(&format!("{base}: xlsx vs xls"), &xlsx, &xls);
        }
    }
}

#[test]
fn binary_formats_decode_comparison_operators_correctly() {
    // The BIFF token tables had PtgGe and PtgGt transposed, inverting every
    // comparison read from .xlsb and .xls. `datatypes!A4` is the guard: the
    // XLSX twin stores the authoritative text as XML, so it cannot drift.
    for ext in ["xlsx", "xlsb", "xls"] {
        let loaded = load(vendor(&format!("issues.{ext}"))).expect("load");
        let sheet = loaded
            .workbook
            .sheet_by_name("datatypes")
            .expect("datatypes");
        let cell = sheet.get(3, 0).expect("datatypes!A4");
        assert_eq!(
            cell.formula.as_deref(),
            Some("A1>A2"),
            "{ext}: A4 must decode as a strict greater-than"
        );
    }
}

#[test]
fn xlsb_actually_yields_formulas() {
    // Guards against a regression where XLSB silently returns no formulas at
    // all: every assertion above would still pass if both sides had none.
    let xlsb = load(vendor("issues.xlsb")).unwrap();
    let formulas: Vec<String> = xlsb
        .workbook
        .sheets
        .iter()
        .flat_map(|s| s.iter())
        .filter_map(|(_, c)| c.formula.clone())
        .collect();
    assert!(
        formulas.len() >= 3,
        "expected formulas decoded from XLSB, got {formulas:?}"
    );
}

#[test]
fn both_formats_load_without_warnings_that_matter() {
    for base in ["issues", "any_sheets"] {
        for ext in ["xlsx", "xlsb"] {
            let loaded = load(vendor(&format!("{base}.{ext}"))).expect("load");
            assert!(
                !loaded.workbook.sheets.is_empty(),
                "{base}.{ext}: no sheets loaded"
            );
            assert!(
                loaded.workbook.total_cells() > 0,
                "{base}.{ext}: no cells loaded"
            );
        }
    }
}

#[test]
fn content_hash_is_stable_and_distinguishes_files() {
    let a = load(vendor("issues.xlsx")).unwrap();
    let b = load(vendor("issues.xlsx")).unwrap();
    let c = load(vendor("issues.xlsb")).unwrap();
    assert_eq!(a.workbook.content_hash, b.workbook.content_hash);
    assert_ne!(a.workbook.content_hash, c.workbook.content_hash);
    assert!(!a.workbook.content_hash.is_empty());
}

#[test]
fn capabilities_are_reported_per_format() {
    let xlsx = load(vendor("issues.xlsx")).unwrap();
    assert_eq!(
        xlsx.capabilities,
        Capabilities::for_format(WorkbookFormat::Xlsx)
    );
    assert!(xlsx.workbook.is_writable());

    let xlsb = load(vendor("issues.xlsb")).unwrap();
    assert!(!xlsb.workbook.is_writable());
    assert!(!xlsb.capabilities.merged_cells);
}

#[test]
fn sheet_ids_match_workbook_order() {
    let loaded = load(vendor("any_sheets.xlsx")).unwrap();
    for (i, sheet) in loaded.workbook.sheets.iter().enumerate() {
        assert_eq!(sheet.id.0 as usize, i, "sheet id must be its tab index");
        assert_eq!(loaded.workbook.sheet(sheet.id).unwrap().name, sheet.name);
    }
}

#[test]
fn citations_resolve_against_loaded_sheet_names() {
    let loaded = load(vendor("issues.xlsx")).unwrap();
    let sheet = &loaded.workbook.sheets[0];
    let (addr, _) = sheet.iter().next().expect("at least one populated cell");
    let citation = loaded.workbook.cite(addr);
    // A citation must round-trip back to the sheet it names.
    let parsed = eg_model::parse_a1(&citation).expect("citation must be parseable");
    assert_eq!(parsed.sheet_name.as_deref(), Some(sheet.name.as_str()));
}

#[test]
fn the_demo_workbook_reads_the_same_as_xlsx_and_as_ods() {
    // The vendored fixtures above are calamine's, and they pair xlsx with the
    // binary formats. This is ours: one logical workbook written by
    // `eg-fixtures` and converted by LibreOffice, so the two files are the same
    // spreadsheet by construction rather than by someone remembering to save it
    // twice. Regenerate both with:
    //
    //   cargo run --release -p eg-fixtures -- --rows 2000 --out tests/fixtures/demo
    //
    // Formula text is compared, and this is the only test that can check the
    // ODF translation against something other than its own author's opinion.
    // calamine hands back the ODF source verbatim for ODS — `of:=VLOOKUP(
    // [.D2];[Rates.$A$4:.$B$7];2;FALSE())` — and `eg_ingest::odf` turns it into
    // the A1 the rest of the workspace speaks. What it should turn it into is
    // exactly what LibreOffice wrote into the xlsx from the same source, cell
    // for cell, which is what this compares.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/demo");
    let xlsx = load(dir.join("impairment.xlsx")).expect("load demo xlsx");
    let ods = load(dir.join("impairment.ods")).expect("load demo ods");

    let mut mismatches = Vec::new();
    let mut errors_lost = 0usize;
    let mut compared = 0usize;

    for sheet_a in &xlsx.workbook.sheets {
        let sheet_b = ods
            .workbook
            .sheet_by_name(&sheet_a.name)
            .expect("the same workbook, so the same sheets");
        compared += 1;

        let mut coords: Vec<(u32, u16)> = sheet_a
            .iter()
            .chain(sheet_b.iter())
            .map(|(r, _)| (r.row, r.col))
            .collect();
        coords.sort_unstable();
        coords.dedup();

        for (row, col) in coords {
            let a1 = eg_model::CellRef::new(sheet_a.id, row, col).to_a1();
            let va = sheet_a.value(row, col);
            let vb = sheet_b.value(row, col);

            // A known gap, recorded rather than tolerated: ODF marks an error
            // cell with `calcext:value-type="error"` and leaves
            // `office:string-value` empty, and calamine reads the empty string.
            // The `#DIV/0!` this fixture plants therefore arrives as a blank
            // cell — the same silent loss as issue 6, in the ODS reader. See
            // `docs/upstream-issues.md`.
            if matches!(va, eg_model::CellValue::Error(_))
                && matches!(vb, eg_model::CellValue::Empty)
            {
                errors_lost += 1;
                continue;
            }
            if !values_agree(&va, &vb) {
                mismatches.push(format!("{}!{a1} value: {va:?} vs {vb:?}", sheet_a.name));
            }

            let fa = sheet_a.get(row, col).and_then(|c| c.formula.as_deref());
            let fb = sheet_b.get(row, col).and_then(|c| c.formula.as_deref());
            if fa != fb {
                mismatches.push(format!("{}!{a1} formula: {fa:?} vs {fb:?}", sheet_a.name));
            }
        }
    }

    assert_eq!(compared, 7, "every sheet of the demo workbook");
    assert!(
        mismatches.is_empty(),
        "demo: xlsx vs ods: {} mismatches:\n  {}",
        mismatches.len(),
        mismatches.join("\n  ")
    );
    // Asserted, not merely allowed: the day calamine reads ODF error cells,
    // this fails and the note above gets deleted rather than quietly outliving
    // the defect it describes.
    assert!(
        errors_lost > 0,
        "the ODS error-cell gap has closed — delete this allowance and the \
         upstream note with it"
    );

    // A defined name's target is an address, not formula text, and arrives in
    // ODF syntax too — a gap that hides rather than fails, because a name that
    // resolves to nothing looks like a workbook without one.
    let names = |w: &eg_model::Workbook| {
        let mut v: Vec<(String, String)> = w
            .defined_names
            .iter()
            .map(|n| (n.name.clone(), n.refers_to.clone()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(names(&xlsx.workbook), names(&ods.workbook));
}

/// The same demo workbook as `.xls`, where the reader is known to be wrong.
///
/// This is here to make an upstream bug report reproducible by someone who does
/// not have the workbook it was found on. calamine reads a BIFF cross-sheet
/// reference through the wrong table — the `EXTERNSHEET`/`XTI` index used as if
/// it were a tab index — and the qualifiers come back *swapped*: what is on
/// `Rates` is attributed to `Debtors` and the other way round. It is recorded
/// as issue 9 in `docs/upstream-issues.md`, and reproduced against stock
/// calamine 0.36.1, so it is not the fork's doing.
///
/// The defect is invisible to every other check in this file. The values are
/// right, the formula count is right, the formulas *parse*, and each one names
/// a real range on a real sheet — just not the one that was written. Only
/// having the same spreadsheet in a second format shows it.
#[test]
fn the_demo_workbook_as_xls_names_the_wrong_sheets_and_nothing_else() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/demo");
    let xlsx = load(dir.join("impairment.xlsx")).expect("load demo xlsx");
    let xls = load(dir.join("impairment.xls")).expect("load demo xls");

    // Which sheet a reference was attributed to, counted over every formula
    // whose text differs: `("Rates", "Debtors")` means the xlsx said `Rates`
    // where the xls said `Debtors`.
    let mut substitutions: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut unexplained: Vec<String> = Vec::new();
    let mut values = Vec::new();
    let (mut agreeing, mut differing) = (0usize, 0usize);

    for sheet_a in &xlsx.workbook.sheets {
        let sheet_b = xls
            .workbook
            .sheet_by_name(&sheet_a.name)
            .expect("the same workbook, so the same sheets");

        let mut coords: Vec<(u32, u16)> = sheet_a
            .iter()
            .chain(sheet_b.iter())
            .map(|(r, _)| (r.row, r.col))
            .collect();
        coords.sort_unstable();
        coords.dedup();

        for (row, col) in coords {
            let a1 = eg_model::CellRef::new(sheet_a.id, row, col).to_a1();
            let (va, vb) = (sheet_a.value(row, col), sheet_b.value(row, col));
            if !values_agree(&va, &vb) {
                values.push(format!("{}!{a1}: {va:?} vs {vb:?}", sheet_a.name));
            }

            let fa = sheet_a.get(row, col).and_then(|c| c.formula.as_deref());
            let fb = sheet_b.get(row, col).and_then(|c| c.formula.as_deref());
            let (Some(fa), Some(fb)) = (fa, fb) else {
                if fa.is_some() != fb.is_some() {
                    unexplained.push(format!("{}!{a1}: {fa:?} vs {fb:?}", sheet_a.name));
                }
                continue;
            };
            if fa == fb {
                agreeing += 1;
                continue;
            }
            differing += 1;

            // A qualifier defect and only a qualifier defect: the two formulas
            // must refer to the same things in the same order and at the same
            // local addresses, differing only in the sheet each is attributed
            // to. Anything else is a different bug and must not be absorbed
            // into this one.
            let (refs_a, refs_b) = (scan_references(fa), scan_references(fb));
            let mut qualifier_only = refs_a.len() == refs_b.len();
            for (x, y) in refs_a.iter().zip(refs_b.iter()) {
                let local_matches = fa[x.local.clone()] == fb[y.local.clone()];
                match (local_matches, &x.parsed.sheet_name, &y.parsed.sheet_name) {
                    (true, Some(from), Some(to)) if from != to => {
                        *substitutions.entry((from.clone(), to.clone())).or_default() += 1;
                    }
                    (true, from, to) if from == to => {}
                    _ => qualifier_only = false,
                }
            }
            if !qualifier_only {
                unexplained.push(format!("{}!{a1}: {fa:?} vs {fb:?}", sheet_a.name));
            }
        }
    }

    // Values are untouched. The defect is in how a formula's *references* are
    // decoded, not in what the file cached, which is exactly why a sweep like
    // `eg check` cannot see it either.
    assert!(values.is_empty(), "xls values differ: {values:?}");

    // The one difference that is not a swapped qualifier: a 3-D reference,
    // whose sheet span decodes to a single sheet and a column past Excel's
    // last (`XFD`). Recorded here rather than tolerated — a second unexplained
    // difference, or this one going away, fails the test.
    assert_eq!(
        unexplained.len(),
        1,
        "xls differs in ways issue 9 does not explain: {unexplained:?}"
    );
    assert!(
        unexplained[0].contains("Jan:Mar!B2") && unexplained[0].contains("BTRN"),
        "the 3-D reference decodes differently than recorded: {}",
        unexplained[0]
    );

    // And the shape of the defect itself: a *swap*, not a shift or a default to
    // the formula's own sheet. Both directions occurring is what points at an
    // index into the wrong table rather than an off-by-one.
    let pairs: Vec<(&str, &str)> = substitutions
        .keys()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    assert_eq!(
        pairs,
        vec![("Debtors", "Rates"), ("Rates", "Debtors")],
        "the substitutions are not the recorded swap: {substitutions:?}"
    );

    // Asserted so the test fails when calamine is fixed, rather than passing
    // vacuously and outliving the defect it documents.
    assert!(
        differing > 0 && agreeing > 0,
        "{agreeing} formulas agreed and {differing} differed — if none differ, \
         issue 9 is fixed: delete this test and the upstream note with it"
    );
}
