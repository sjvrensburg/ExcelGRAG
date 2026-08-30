//! Format-parity and round-trip tests.
//!
//! The parity tests are the most valuable in the suite: XLSB is the reason the
//! project is written in Rust, and a silent regression in XLSB handling would
//! otherwise only surface on a user's large confidential workbook, where it is
//! hardest to debug.
//!
//! The `.xlsb`/`.xlsx` pairs under `tests/fixtures/vendor` were authored by real
//! Excel, because nothing open-source can write XLSB.

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

#[test]
fn xlsb_and_xlsx_agree_on_values_and_formulas() {
    for base in ["issues", "any_sheets", "issue_419"] {
        let xlsx = load(vendor(&format!("{base}.xlsx"))).expect("load xlsx");
        let xlsb = load(vendor(&format!("{base}.xlsb"))).expect("load xlsb");

        let names_a: Vec<&str> = xlsx.workbook.sheets.iter().map(|s| s.name.as_str()).collect();
        let mut sorted_a = names_a.clone();
        let mut sorted_b: Vec<&str> = xlsb.workbook.sheets.iter().map(|s| s.name.as_str()).collect();
        sorted_a.sort_unstable();
        sorted_b.sort_unstable();
        assert_eq!(sorted_a, sorted_b, "{base}: sheet sets differ");

        // Sheets are matched by name, not position: calamine reports XLSB sheets
        // in a different order than XLSX for the same workbook, so a positional
        // comparison would report spurious mismatches.
        for sheet_a in &xlsx.workbook.sheets {
            let sheet_b = xlsb
                .workbook
                .sheet_by_name(&sheet_a.name)
                .expect("matched by name above");

            let mut coords: Vec<(u32, u16)> = sheet_a
                .iter()
                .chain(sheet_b.iter())
                .map(|(r, _)| (r.row, r.col))
                .collect();
            coords.sort_unstable();
            coords.dedup();

            let mut mismatches = Vec::new();
            for (row, col) in coords {
                let a1 = eg_model::CellRef::new(sheet_a.id, row, col).to_a1();

                let va = sheet_a.value(row, col);
                let vb = sheet_b.value(row, col);
                if !values_agree(&va, &vb) {
                    mismatches.push(format!(
                        "{}!{a1} value: xlsx={va:?} xlsb={vb:?}",
                        sheet_a.name
                    ));
                }

                // Formula parity is the load-bearing assertion: XLSB stores
                // formulas as binary RPN tokens, and losing them would silently
                // gut the dependency graph on exactly the large files that
                // motivated choosing XLSB support in the first place.
                let fa = sheet_a.get(row, col).and_then(|c| c.formula.as_deref());
                let fb = sheet_b.get(row, col).and_then(|c| c.formula.as_deref());
                if fa != fb {
                    mismatches.push(format!(
                        "{}!{a1} formula: xlsx={fa:?} xlsb={fb:?}",
                        sheet_a.name
                    ));
                }
            }
            assert!(
                mismatches.is_empty(),
                "{base}: {} mismatches:\n  {}",
                mismatches.len(),
                mismatches.join("\n  ")
            );
        }
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
