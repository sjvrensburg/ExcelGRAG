//! Format-parity and round-trip tests.
//!
//! The parity tests are the most valuable in the suite: XLSB is the reason the
//! project is written in Rust, and a silent regression in XLSB handling would
//! otherwise only surface on a user's large confidential workbook, where it is
//! hardest to debug.
//!
//! The `.xlsb`/`.xlsx` pairs under `tests/fixtures/vendor` were authored by real
//! Excel, because nothing open-source can write XLSB.

use std::collections::BTreeSet;
use std::path::PathBuf;

use eg_ingest::{load, Capabilities};
use eg_model::{CellValue, WorkbookFormat};

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

fn formulas_agree(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let normalize = |text: &str| text.replace("FALSE()", "FALSE").replace("TRUE()", "TRUE");
            normalize(a) == normalize(b)
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
            if !formulas_agree(fa, fb) {
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

fn sheet_names(loaded: &eg_ingest::Loaded) -> BTreeSet<&str> {
    loaded
        .workbook
        .sheets
        .iter()
        .map(|sheet| sheet.name.as_str())
        .collect()
}

#[test]
fn all_formats_agree_on_values_and_formulas() {
    // issues.* exists in all three formats; the other two pair xlsx with xlsb.
    for base in ["issues", "any_sheets", "issue_419"] {
        let xlsx = load(vendor(&format!("{base}.xlsx"))).expect("load xlsx");
        let xlsb = load(vendor(&format!("{base}.xlsb"))).expect("load xlsb");
        assert_eq!(sheet_names(&xlsx), sheet_names(&xlsb), "{base} sheet set");
        assert_agree(&format!("{base}: xlsx vs xlsb"), &xlsx, &xlsb);

        let xls_path = vendor(&format!("{base}.xls"));
        if xls_path.exists() {
            let xls = load(&xls_path).expect("load xls");
            let mut expected = sheet_names(&xlsx);
            // The upstream issues.xls fixture predates the spc_chrs sheet in
            // its twins; pin that one deliberate fixture difference exactly.
            expected.remove("spc_chrs");
            assert_eq!(expected, sheet_names(&xls), "{base} xls sheet set");
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
fn both_formats_load_without_warnings() {
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
            let expected_warning = base == "any_sheets" && ext == "xlsb";
            if expected_warning {
                assert_eq!(
                    loaded.warnings.len(),
                    1,
                    "unexpected warnings: {:?}",
                    loaded.warnings
                );
                assert!(loaded.warnings[0].contains("Chart"));
            } else {
                assert!(
                    loaded.warnings.is_empty(),
                    "{base}.{ext}: unexpected warnings: {:?}",
                    loaded.warnings
                );
            }
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
fn xlsx_defined_names_keep_their_sheet_scope() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scoped-names.xlsx");
    let mut source = rust_xlsxwriter::Workbook::new();
    source.add_worksheet().set_name("First").unwrap();
    source.add_worksheet().set_name("Second").unwrap();
    source.define_name("Rate", "=First!$A$1").unwrap();
    source.define_name("Second!Rate", "=Second!$A$1").unwrap();
    source.save(&path).unwrap();

    let loaded = load(&path).unwrap();
    let global = loaded
        .workbook
        .defined_names
        .iter()
        .find(|name| name.name == "Rate" && name.scope.is_none())
        .expect("workbook-scoped name");
    assert_eq!(global.refers_to, "First!$A$1");
    let local = loaded
        .workbook
        .defined_names
        .iter()
        .find(|name| name.name == "Rate" && name.scope == Some(eg_model::SheetId(1)))
        .expect("sheet-scoped name");
    assert_eq!(local.refers_to, "Second!$A$1");
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
    let mut errors_preserved = 0usize;
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

            // LibreOffice's calcext marker preserves only that this is an
            // error, not its precise code. It must nevertheless remain an
            // error rather than silently becoming an empty string.
            if matches!(va, eg_model::CellValue::Error(_))
                && matches!(vb, eg_model::CellValue::Error(_))
            {
                errors_preserved += 1;
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
    assert!(
        errors_preserved > 0,
        "the fixture's ODS error cells were not preserved as errors"
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

#[test]
fn the_demo_workbook_reads_the_same_as_xlsx_and_as_xls() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/demo");
    let xlsx = load(dir.join("impairment.xlsx")).expect("load demo xlsx");
    let xls = load(dir.join("impairment.xls")).expect("load demo xls");
    assert_eq!(xlsx.workbook.sheets.len(), xls.workbook.sheets.len());
    assert_agree("demo: xlsx vs xls", &xlsx, &xls);
}
