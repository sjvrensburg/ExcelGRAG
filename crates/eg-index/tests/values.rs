//! Whether a column can be found by what it holds.
//!
//! Everything else in this crate indexes what a workbook *is*. This is the one
//! part that indexes its data, and it arrives through a profile — which is
//! also what bounds it: a profile keeps a column's distinct values only where
//! there are few enough of them, so what can be found this way is exactly what
//! `profiles/` was willing to store, and no more. Both halves of that are
//! asserted here, because the half that is missing is the one a caller has to
//! be told about.

use eg_graph::{build, NodeKind};
use eg_index::{docs_for_with, SearchOptions, TextIndex};
use eg_model::{Cell, CellValue, Sheet, SheetId, Workbook, WorkbookFormat};
use eg_structure::{detect_regions, profile_table, read_table, ProfileOptions, Profiles};

/// Wider than `ProfileOptions::max_distinct`, so a column of one value per row
/// is a column whose distinct list the profiler abandons.
const ROWS: usize = 70;

/// A ledger with one column of each kind a profile treats differently: a
/// category repeated down the sheet, a number per row, and an identifier per
/// row.
///
/// Half the columns hold numbers under a text header, because that contrast is
/// what region detection reads a header row off — a table of text under text
/// has no header at all, and then no column nodes to hang values on.
fn ledger() -> Workbook {
    let mut sheet = Sheet::new(SheetId(0), "Ledger");
    for (c, header) in ["Account", "Debt Type", "Balance", "Days", "Provision"]
        .iter()
        .enumerate()
    {
        sheet.set(
            0,
            c as u16,
            Cell::literal(CellValue::Text((*header).to_string())),
        );
    }
    const TYPES: [&str; 3] = ["Retail", "Business", "Wholesale"];
    for i in 0..ROWS {
        let row = i as u32 + 1;
        sheet.set(
            row,
            0,
            Cell::literal(CellValue::Text(format!("ACC{:04}", i + 1))),
        );
        sheet.set(
            row,
            1,
            Cell::literal(CellValue::Text(TYPES[i % TYPES.len()].to_string())),
        );
        sheet.set(row, 2, Cell::literal(CellValue::Number(1000.0 + i as f64)));
        sheet.set(row, 3, Cell::literal(CellValue::Number((i % 90) as f64)));
        sheet.set(row, 4, Cell::literal(CellValue::Number(0.25)));
    }

    // A second sheet whose column is headed the same and holds something else,
    // so that a profile reaching the wrong column would be visible rather than
    // merely possible.
    let mut other = Sheet::new(SheetId(1), "Archive");
    for (c, header) in ["Account", "Debt Type", "Balance", "Days", "Provision"]
        .iter()
        .enumerate()
    {
        other.set(
            0,
            c as u16,
            Cell::literal(CellValue::Text((*header).to_string())),
        );
    }
    for i in 0..ROWS {
        let row = i as u32 + 1;
        other.set(
            row,
            0,
            Cell::literal(CellValue::Text(format!("OLD{:04}", i + 1))),
        );
        other.set(row, 1, Cell::literal(CellValue::Text("Government".into())));
        other.set(row, 2, Cell::literal(CellValue::Number(5000.0 + i as f64)));
        other.set(row, 3, Cell::literal(CellValue::Number((i % 90) as f64)));
        other.set(row, 4, Cell::literal(CellValue::Number(0.4)));
    }

    // A rates table whose *headers* are the ledger's category values, so that
    // "wholesale" names one column and is held by another. Which of the two
    // comes first is what `VALUES_BOOST` decides.
    let mut rates = Sheet::new(SheetId(2), "Rates");
    for (c, header) in ["Retail", "Business", "Wholesale"].iter().enumerate() {
        rates.set(
            0,
            c as u16,
            Cell::literal(CellValue::Text((*header).to_string())),
        );
        for row in 1..4u32 {
            rates.set(
                row,
                c as u16,
                Cell::literal(CellValue::Number(0.1 * (c + 1) as f64 * row as f64)),
            );
        }
    }

    Workbook {
        path: "ledger.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-ledger".into(),
        sheets: vec![sheet, other, rates],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

/// The profiles `eg index` would store for a workbook.
fn profiles(workbook: &Workbook, values: bool) -> Profiles {
    let opts = ProfileOptions {
        values,
        ..Default::default()
    };
    let mut columns = Vec::new();
    for sheet in &workbook.sheets {
        for region in detect_regions(sheet) {
            if let Some(table) = read_table(sheet, &region) {
                columns.extend(profile_table(sheet, &table, &opts));
            }
        }
    }
    Profiles {
        columns,
        values: opts.values,
    }
}

fn dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "eg-index-values-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    base
}

fn indexed(tag: &str, workbook: &Workbook, values: bool) -> (std::path::PathBuf, TextIndex) {
    let root = dir(tag);
    let mut index = TextIndex::open(&root).unwrap();
    let built = build(workbook);
    index
        .index_graph_with(
            &built.graph,
            &workbook.content_hash,
            &workbook.path,
            Some(&profiles(workbook, values)),
        )
        .unwrap();
    (root, index)
}

fn columns(hits: &[eg_index::Hit]) -> Vec<(&str, &str)> {
    hits.iter()
        .filter(|h| h.kind == NodeKind::Column)
        .map(|h| (h.sheet.as_deref().unwrap_or(""), h.label.as_str()))
        .collect()
}

#[test]
fn a_column_carries_the_values_its_profile_kept() {
    let workbook = ledger();
    let built = build(&workbook);
    let docs = docs_for_with(&built.graph, Some(&profiles(&workbook, true)));

    let debt_type = docs
        .iter()
        .find(|d| d.label == "Debt Type" && d.sheet.as_deref() == Some("Ledger"))
        .expect("the Debt Type column is indexed");
    let mut values = debt_type.values.clone();
    values.sort();
    assert_eq!(values, ["Business", "Retail", "Wholesale"]);
    assert!(debt_type.categorical, "a few values, each repeated");

    // Same header, other sheet, other values: the profile is matched by range,
    // so the two never see each other's.
    let archive = docs
        .iter()
        .find(|d| d.label == "Debt Type" && d.sheet.as_deref() == Some("Archive"))
        .expect("the Archive column is indexed");
    assert_eq!(archive.values, ["Government"]);
}

#[test]
fn a_column_with_too_many_values_to_keep_carries_its_bounds_and_nothing_else() {
    // The trade the whole feature rests on. A profile abandons the distinct
    // list above `max_distinct`, so what is left of a large numeric column is
    // its ends — and those are cells, which a search for either can be
    // followed to. A sum is not, and is deliberately absent.
    let workbook = ledger();
    let built = build(&workbook);
    let docs = docs_for_with(&built.graph, Some(&profiles(&workbook, true)));

    let balance = docs
        .iter()
        .find(|d| d.label == "Balance")
        .expect("the Balance column is indexed");
    assert_eq!(balance.values, ["1000", "1069"]);
    assert!(!balance.categorical, "one value per row is not a category");

    // An identifier column is neither: nothing to keep and no bounds to take.
    let days = docs
        .iter()
        .find(|d| d.label == "Days" && d.sheet.as_deref() == Some("Ledger"))
        .expect("the Days column is indexed");
    assert_eq!(days.values, ["0", "69"]);
    assert!(!days.categorical);
}

#[test]
fn a_row_label_column_carries_no_values_and_this_is_where_that_is_written_down() {
    // A region's leading row-label columns sit outside the body `read_table`
    // reads, so `profile_table` never sees them and there is nothing to index
    // for them — `eg search ACC0007` misses, and the reason is here rather
    // than in a puzzled bug report. Matching by *range* is what makes that a
    // clean absence: the Account column gets no values rather than its
    // neighbour's.
    let workbook = ledger();
    let built = build(&workbook);
    let docs = docs_for_with(&built.graph, Some(&profiles(&workbook, true)));

    let account = docs
        .iter()
        .find(|d| d.label == "Account" && d.sheet.as_deref() == Some("Ledger"))
        .expect("the Account column still gets a node");
    assert!(
        account.values.is_empty(),
        "account carried {:?}",
        account.values
    );
}

#[test]
fn a_column_is_found_by_a_value_it_holds() {
    // `Government` is a value and nothing else — no sheet, table or column is
    // called it — so a hit on it can only have come through a profile.
    let (_root, index) = indexed("found", &ledger(), true);
    let hits = index
        .search("Government", &SearchOptions::default())
        .unwrap();
    assert_eq!(
        columns(&hits),
        [("Archive", "Debt Type")],
        "government found {:?}",
        columns(&hits)
    );

    // And the bound of a column whose values were too many to keep, which is
    // the case `eg search` used to come back blind on.
    let hits = index.search("1069", &SearchOptions::default()).unwrap();
    assert_eq!(
        columns(&hits).first(),
        Some(&("Ledger", "Balance")),
        "1069 found {:?}",
        columns(&hits)
    );
}

#[test]
fn a_value_between_the_bounds_is_not_claimed_to_be_there() {
    // 1030 is in the column and is not in the index — a bound is two cells,
    // not a range test. Indexing it as if it were would offer a hit that
    // cannot be followed to a cell, which is the failure this whole layer is
    // built to avoid; `eg where` is what answers it.
    let (_root, index) = indexed("between", &ledger(), true);
    let hits = index.search("1030", &SearchOptions::default()).unwrap();
    assert!(hits.is_empty(), "1030 matched {:?}", columns(&hits));
}

#[test]
fn a_redacted_profile_puts_nothing_in_the_index() {
    // `--redact-values` writes profiles of counts and types alone, and the
    // values reach this index only through a profile that kept them — so the
    // refusal needs no separate enforcement here, and this is the assertion
    // that it does not.
    let (_root, index) = indexed("redacted", &ledger(), false);
    let hits = index
        .search("Government", &SearchOptions::default())
        .unwrap();
    assert!(hits.is_empty(), "government matched {:?}", columns(&hits));

    // The structure is still there, which is the point of redacting rather
    // than not profiling.
    let hits = index
        .search("Debt Type", &SearchOptions::default())
        .unwrap();
    assert!(!hits.is_empty(), "the column itself went missing");
}

#[test]
fn a_corpus_indexed_without_profiles_is_unchanged() {
    let root = dir("none");
    let workbook = ledger();
    let built = build(&workbook);
    let mut index = TextIndex::open(&root).unwrap();
    index
        .index_built(&built, &workbook.content_hash, &workbook.path)
        .unwrap();
    let hits = index
        .search("Government", &SearchOptions::default())
        .unwrap();
    assert!(hits.is_empty(), "government matched {:?}", columns(&hits));
}

#[test]
fn a_categorical_column_reads_as_a_phrase_and_a_key_column_does_not() {
    // What the embedder is given. Three repeated categories are a sentence
    // about what the column means; seventy numbers are a list, and would push
    // the column's own name out of the window.
    let workbook = ledger();
    let built = build(&workbook);
    let docs = docs_for_with(&built.graph, Some(&profiles(&workbook, true)));

    let debt_type = docs.iter().find(|d| d.label == "Debt Type").unwrap();
    let text = debt_type.embedding_text();
    assert!(text.starts_with("column Debt Type"), "{text}");
    assert!(text.contains("holding"), "{text}");
    assert!(text.contains("Retail"), "{text}");

    let balance = docs.iter().find(|d| d.label == "Balance").unwrap();
    assert!(!balance.embedding_text().contains("holding"));
}

#[test]
fn a_column_named_for_a_value_outranks_one_merely_holding_it() {
    // The ordering `VALUES_BOOST` exists to fix. `Wholesale` heads a column of
    // the rates table and is one of three values in the ledger's Debt Type
    // column; the one it *names* is the better answer, and a value weighed
    // level with a header would have made this a coin toss decided by which
    // node covers more cells — which is the ledger's.
    let (_root, index) = indexed("outrank", &ledger(), true);
    let hits = index
        .search("Wholesale", &SearchOptions::default())
        .unwrap();
    assert_eq!(
        columns(&hits),
        [("Rates", "Wholesale"), ("Ledger", "Debt Type")],
        "wholesale ranked {:?}",
        columns(&hits)
    );
}
