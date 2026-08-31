//! M8/M9: a declared table's `headerRowCount`/`totalsRowCount` must be read
//! for what they say, not guessed at from the presence of column names or
//! dropped on the floor. `tableColumn` names exist even for a headerless
//! table — Excel auto-names them Column1, Column2, … — so only the table's
//! own declaration can tell a headerless table from a headed one, and only
//! calamine's fork (fixed alongside this) reports the totals row correctly
//! when there is no header row too. See docs/audit-2026-08-31.md M8, M9 and
//! docs/upstream-issues.md issue 8.
//!
//! XLSX table XML is plain OOXML, unlike XLSB, so these fixtures are authored
//! on the fly with `rust_xlsxwriter` rather than needing anything checked in.

use eg_ingest::load;
use rust_xlsxwriter::{Table, TableColumn, TableFunction, Workbook};

fn written(build: impl FnOnce(&mut Workbook)) -> tempfile::NamedTempFile {
    let mut workbook = Workbook::new();
    build(&mut workbook);
    let file = tempfile::NamedTempFile::with_suffix(".xlsx").expect("a temp file");
    workbook.save(file.path()).expect("saved");
    file
}

#[test]
fn a_headerless_table_does_not_annex_the_row_above_it() {
    let file = written(|workbook| {
        let worksheet = workbook.add_worksheet();
        // A real value sits directly above the table — exactly what a
        // headerless table's auto-generated column names must not annex.
        worksheet.write(0, 0, "Not a header").unwrap();
        worksheet.write_column(1, 0, [1.0, 2.0, 3.0]).unwrap();
        worksheet.write_column(1, 1, [10.0, 20.0, 30.0]).unwrap();
        let table = Table::new()
            .set_name("Headerless")
            .set_header_row(false)
            .set_columns(&[TableColumn::new(), TableColumn::new()]);
        worksheet.add_table(1, 0, 3, 1, &table).unwrap();
    });

    let loaded = load(file.path()).expect("loads");
    let sheet = &loaded.workbook.sheets[0];
    assert_eq!(sheet.tables.len(), 1);
    let table = &sheet.tables[0];
    assert!(
        !table.has_header_row,
        "headerRowCount was declared 0, so there is no header row"
    );
    // The table's range must start at its own first data row (index 1, the
    // second row), not row 0 — annexing "Not a header" as if it were one.
    assert_eq!(table.range.top, 1, "must not annex the row above the table");
}

#[test]
fn a_declared_totals_row_is_excluded_from_the_table_body() {
    let file = written(|workbook| {
        let worksheet = workbook.add_worksheet();
        worksheet.write_row(0, 0, ["Region", "Amount"]).unwrap();
        worksheet
            .write_column(1, 0, ["North", "South", "East"])
            .unwrap();
        worksheet.write_column(1, 1, [10.0, 20.0, 30.0]).unwrap();
        let columns = vec![
            TableColumn::new().set_total_label("Total"),
            TableColumn::new().set_total_function(TableFunction::Sum),
        ];
        let table = Table::new()
            .set_name("Sales")
            .set_total_row(true)
            .set_columns(&columns);
        // Rows 0..=4: header, three data rows, one totals row.
        worksheet.add_table(0, 0, 4, 1, &table).unwrap();
    });

    let loaded = load(file.path()).expect("loads");
    let sheet = &loaded.workbook.sheets[0];
    let table = &sheet.tables[0];
    assert!(table.has_header_row);
    assert!(table.has_totals_row, "totalsRowCount was declared 1");

    let region = eg_structure::detect_regions(sheet)
        .into_iter()
        .find(|r| r.source == eg_structure::RegionSource::Declared)
        .expect("the declared table is a region");
    let body = region.body().expect("a body");
    assert_eq!(
        body.rows(),
        3,
        "the totals row must not be counted as a data row: {body:?}"
    );

    // And a query summing the column gets the real total, not one inflated
    // by the totals-row cell that (per Excel convention) already holds it.
    let table_read = eg_structure::read_table(sheet, &region).expect("a table");
    let profile =
        eg_structure::profile_table(sheet, &table_read, &eg_structure::ProfileOptions::default());
    let amount = profile
        .into_iter()
        .find(|c| c.header == "Amount")
        .expect("the Amount column");
    assert_eq!(amount.populated, 3, "three data rows, not four");
}
