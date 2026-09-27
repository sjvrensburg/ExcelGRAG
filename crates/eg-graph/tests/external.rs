//! References into other workbooks, read from a real `.xls` that links to two.

use std::path::PathBuf;

use eg_graph::{build, check, NodeKind};

#[test]
fn an_xls_reference_into_another_workbook_is_not_lifted_onto_one_of_ours() {
    // OOM_alloc.xls links to two other workbooks, and its XTI table points
    // into both. Each XTI names a book and a tab *of that book*; read as a tab
    // of this workbook, tab 1 of a linked book became our `Data` and tab 2
    // our `EIM New Deals` — real sheets, so every one of these references was
    // lifted onto a dependency that does not exist, and nothing looked wrong.
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/vendor/OOM_alloc.xls");
    let loaded = eg_ingest::load(&path).expect("load OOM_alloc.xls");

    let sheet = loaded
        .workbook
        .sheet_by_name("WE 2-15 EOL Data")
        .expect("sheet");
    assert_eq!(
        sheet.get(5, 1).and_then(|c| c.formula.as_deref()),
        Some(
            "'[2]Thrusday 02-15-01'!S9+'[2]Thrusday 02-15-01'!S10\
             +-'[2]Thursday 02-08-01'!S9-'[2]Thursday 02-08-01'!S10"
        ),
        "B6"
    );

    let built = build(&loaded.workbook);
    assert_eq!(check(&built), vec![]);
    // One node per linked workbook, and no reference left dangling.
    assert_eq!(built.report.nodes_of(NodeKind::ExternalWorkbook), 2);
    assert!(built.report.references_external > 0);
    assert_eq!(built.report.references_dangling, 0);
}
