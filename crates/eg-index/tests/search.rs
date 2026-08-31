//! Searching a corpus small enough to know the right answer for.
//!
//! The reference workbook says whether the index is fast and how big it gets.
//! These say whether the hit it returns is the node a person meant.

use eg_graph::store::Corpus;
use eg_graph::{build, NodeKind};
use eg_index::{IndexError, SearchOptions, TextIndex};
use eg_model::{Cell, CellValue, DefinedName, Sheet, SheetId, Workbook, WorkbookFormat};

fn grid(id: u16, name: &str, rows: &[&str]) -> Sheet {
    let mut sheet = Sheet::new(SheetId(id), name);
    for (r, line) in rows.iter().enumerate() {
        for (c, tok) in line.split_whitespace().enumerate() {
            if tok == "." {
                continue;
            }
            let cell = match tok.strip_prefix('=') {
                Some(f) => Cell {
                    value: CellValue::Number(0.0),
                    formula: Some(f.to_string()),
                    format: Default::default(),
                },
                None => match tok.parse::<f64>() {
                    Ok(n) => Cell::literal(CellValue::Number(n)),
                    Err(_) => Cell::literal(CellValue::Text(tok.to_string())),
                },
            };
            sheet.set(r as u32, c as u16, cell);
        }
    }
    sheet
}

/// A workbook whose sheet name shares a word with a column header, which is the
/// case that decides whether the field weights are doing anything.
fn sales() -> Workbook {
    Workbook {
        path: "sales.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-sales".into(),
        sheets: vec![
            grid(
                0,
                "Q3 Sales",
                &[
                    "Region Revenue Net",
                    "North 10 =B2*Rates!B2",
                    "South 20 =B3*Rates!B3",
                    "East 30 =B4*Rates!B4",
                ],
            ),
            grid(1, "Rates", &["Country Tariff", "ZA 0.15", "UK 0.2"]),
        ],
        defined_names: vec![DefinedName {
            name: "TaxRate".into(),
            refers_to: "Rates!$B$2".into(),
            scope: None,
        }],
        external_links: Vec::new(),
    }
}

fn payroll() -> Workbook {
    Workbook {
        path: "payroll.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-payroll".into(),
        sheets: vec![grid(
            0,
            "Staff",
            &["Name Salary", "Ada 100", "Grace 120", "Alan 90"],
        )],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

/// Headers written the way spreadsheets actually write them.
fn compounds() -> Workbook {
    Workbook {
        path: "compounds.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-compounds".into(),
        sheets: vec![grid(
            0,
            "Sheet1",
            &[
                "Region NetRevenue FY2024 Tariffs",
                "North 10 11 0.1",
                "South 20 21 0.2",
                "East 30 31 0.3",
            ],
        )],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

fn dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "eg-index-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    base
}

fn indexed(tag: &str, workbooks: &[Workbook]) -> (std::path::PathBuf, TextIndex) {
    let root = dir(tag);
    let mut index = TextIndex::open(&root).unwrap();
    for wb in workbooks {
        let built = build(wb);
        index
            .index_built(&built, &wb.content_hash, &wb.path)
            .unwrap();
    }
    (root, index)
}

fn labels(hits: &[eg_index::Hit]) -> Vec<&str> {
    hits.iter().map(|h| h.label.as_str()).collect()
}

#[test]
fn a_column_outranks_the_sheet_it_shares_a_word_with() {
    let (_root, index) = indexed("rank", &[sales()]);
    let hits = index.search("revenue", &SearchOptions::default()).unwrap();

    assert!(!hits.is_empty(), "revenue matched nothing");
    let top = &hits[0];
    assert_eq!(top.kind, NodeKind::Column);
    assert_eq!(top.label, "Revenue");
    assert_eq!(top.sheet.as_deref(), Some("Q3 Sales"));
    // The hit has to be enough to go back to the workbook with.
    assert_eq!(top.workbook, "hash-sales");
    assert_eq!(top.path, "sales.xlsx");
    assert!(top.a1.as_deref().unwrap().starts_with("'Q3 Sales'!"));
}

#[test]
fn a_table_is_found_by_a_column_it_holds() {
    let (_root, index) = indexed("region", &[sales()]);
    let hits = index
        .search(
            "revenue",
            &SearchOptions {
                kinds: vec![NodeKind::Region],
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(
        hits.len(),
        1,
        "expected the one table, got {:?}",
        labels(&hits)
    );
    assert_eq!(hits[0].kind, NodeKind::Region);
    assert_eq!(hits[0].sheet.as_deref(), Some("Q3 Sales"));
}

#[test]
fn a_compound_identifier_is_found_by_one_of_its_words() {
    let (_root, index) = indexed("compound", &[compounds()]);

    // `Sheet1` is one token to a default tokenizer, so `sheet` would miss every
    // sheet in the corpus.
    let sheets = index.search("sheet", &SearchOptions::default()).unwrap();
    assert!(
        sheets.iter().any(|h| h.label == "Sheet1"),
        "sheet found {:?}",
        labels(&sheets)
    );

    for (query, wanted) in [
        ("revenue", "NetRevenue"),
        ("net", "NetRevenue"),
        ("netrevenue", "NetRevenue"),
        ("2024", "FY2024"),
        ("fy", "FY2024"),
    ] {
        let hits = index
            .search(
                query,
                &SearchOptions {
                    kinds: vec![NodeKind::Column],
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            hits.iter().any(|h| h.label == wanted),
            "{query} should find {wanted}, found {:?}",
            labels(&hits)
        );
    }
}

#[test]
fn a_plural_in_the_workbook_answers_the_singular_typed() {
    let (_root, index) = indexed("stem", &[compounds()]);
    let hits = index
        .search(
            "tariff",
            &SearchOptions {
                kinds: vec![NodeKind::Column],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(labels(&hits), vec!["Tariffs"]);
}

#[test]
fn a_range_is_a_citation_to_hand_back_and_never_something_to_match() {
    // The schema stores `a1` without indexing it, on the reasoning that nobody
    // searches for `$B$7` and that indexing it would let a stray `A1` in a
    // query pull in every node whose range happens to start there.
    let (_root, index) = indexed("ranges", &[sales()]);

    for query in ["A1", "B2", "A1:C4", "C2"] {
        let hits = index
            .search(
                query,
                &SearchOptions {
                    limit: 50,
                    ..Default::default()
                },
            )
            .unwrap();
        // A region with no title is labelled by its range, and that label *is*
        // indexed — so a hit is only wrong if it was matched through the
        // citation of a node whose label says something else.
        for hit in &hits {
            assert!(
                hit.label.contains(query) || hit.a1.is_none(),
                "{query} matched {:?} through its citation {:?}",
                hit.label,
                hit.a1
            );
        }
    }
}

#[test]
fn filters_narrow_by_kind_sheet_and_workbook() {
    let (_root, index) = indexed("filters", &[sales(), payroll()]);

    let names = index
        .search(
            "taxrate",
            &SearchOptions {
                kinds: vec![NodeKind::DefinedName],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(labels(&names), vec!["TaxRate"]);

    let on_rates = index
        .search(
            "country tariff",
            &SearchOptions {
                sheet: Some("Rates".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!on_rates.is_empty());
    assert!(on_rates.iter().all(|h| h.sheet.as_deref() == Some("Rates")));

    // A word that matches every node of both workbooks, so only the filter can
    // be what keeps the other one out.
    let one_book = index
        .search(
            "xlsx",
            &SearchOptions {
                workbook: Some("hash-payroll".into()),
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!one_book.is_empty());
    assert!(one_book.iter().all(|h| h.workbook == "hash-payroll"));
}

/// Two formula groups writing the same formula, one filled down 40 rows and
/// one left at a single cell.
fn ties() -> Workbook {
    let mut rows = vec!["Data Small Big".to_string()];
    for r in 1..=40u32 {
        // Both formulas read column A of their own row, so each column is one
        // group; only the second is filled down.
        let small = if r == 1 {
            format!("=LOOKUP(A{})", r + 1)
        } else {
            ".".to_string()
        };
        rows.push(format!("10 {small} =LOOKUP(A{})", r + 1));
    }
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    Workbook {
        path: "ties.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-ties".into(),
        sheets: vec![grid(0, "Ties", &borrowed)],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

#[test]
fn between_equal_matches_the_one_standing_for_more_cells_wins() {
    let (_root, index) = indexed("ties", &[ties()]);
    let hits = index
        .search(
            "lookup",
            &SearchOptions {
                kinds: vec![NodeKind::FormulaGroup],
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();

    let ranges: Vec<&str> = hits.iter().filter_map(|h| h.a1.as_deref()).collect();
    assert_eq!(hits.len(), 2, "expected two groups, got {ranges:?}");
    // Both write `LOOKUP(A2)`, so text relevance has nothing to separate them
    // by; the group standing for 40 cells is the one to look at first.
    assert!(
        hits[0].a1.as_deref().unwrap().contains("C2:C41"),
        "the 40-cell group should lead, got {ranges:?}"
    );
    assert!(hits[0].score > hits[1].score);
}

#[test]
fn size_does_not_outweigh_an_exact_match() {
    // The Revenue column covers three cells; the sheet it is on covers twelve
    // and matches only through context. Size must not flip that.
    let (_root, index) = indexed("size", &[sales()]);
    let hits = index.search("revenue", &SearchOptions::default()).unwrap();
    assert_eq!(hits[0].kind, NodeKind::Column);
    assert_eq!(hits[0].label, "Revenue");
}

#[test]
fn reindexing_a_workbook_replaces_it_rather_than_doubling_it() {
    let (_root, mut index) = indexed("reindex", &[sales()]);
    let before = index.len().unwrap();

    let wb = sales();
    index
        .index_built(&build(&wb), &wb.content_hash, &wb.path)
        .unwrap();

    assert_eq!(index.len().unwrap(), before);
    let hits = index
        .search(
            "revenue",
            &SearchOptions {
                kinds: vec![NodeKind::Column],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(labels(&hits), vec!["Revenue"]);
}

#[test]
fn the_index_says_which_workbooks_it_holds() {
    let (_root, mut index) = indexed("contains", &[sales()]);
    assert!(index.contains("hash-sales").unwrap());
    assert!(!index.contains("hash-payroll").unwrap());

    index.forget("hash-sales").unwrap();
    assert!(!index.contains("hash-sales").unwrap());
}

#[test]
fn a_rebuilt_index_reports_holding_nothing() {
    // What this guards: a caller deciding whether to index by asking some
    // *other* index, and so never noticing that this one was reset.
    let root = dir("contains-rebuild");
    {
        let mut index = TextIndex::open(&root).unwrap();
        let wb = sales();
        index
            .index_built(&build(&wb), &wb.content_hash, &wb.path)
            .unwrap();
        assert!(index.contains("hash-sales").unwrap());
    }
    std::fs::remove_dir_all(root.join("text")).unwrap();

    let index = TextIndex::open(&root).unwrap();
    assert!(!index.contains("hash-sales").unwrap());
}

#[test]
fn forgetting_a_workbook_leaves_the_others() {
    let (_root, mut index) = indexed("forget", &[sales(), payroll()]);
    index.forget("hash-payroll").unwrap();

    let salary = index.search("salary", &SearchOptions::default()).unwrap();
    assert!(salary.is_empty(), "payroll survived: {:?}", labels(&salary));

    let revenue = index.search("revenue", &SearchOptions::default()).unwrap();
    assert!(!revenue.is_empty(), "sales was dropped too");
}

#[test]
fn a_query_full_of_formula_punctuation_is_not_an_error() {
    let (_root, index) = indexed("lenient", &[sales()]);
    // Every one of these is invalid query-parser syntax. A person who pastes a
    // formula they saw should get results, not a parse failure.
    for query in ["=B2*Rates!B2", "'Q3 Sales'!", "SUM(B:B", "revenue^^"] {
        let hits = index.search(query, &SearchOptions::default());
        assert!(hits.is_ok(), "{query} failed: {:?}", hits.err());
    }
}

#[test]
fn an_empty_query_returns_nothing_rather_than_everything() {
    let (_root, index) = indexed("empty", &[sales()]);
    assert!(index
        .search("", &SearchOptions::default())
        .unwrap()
        .is_empty());
    assert!(index
        .search("   ", &SearchOptions::default())
        .unwrap()
        .is_empty());
}

#[test]
fn the_index_reopens_over_what_is_already_there() {
    let root = dir("reopen");
    {
        let mut index = TextIndex::open(&root).unwrap();
        let wb = sales();
        index
            .index_built(&build(&wb), &wb.content_hash, &wb.path)
            .unwrap();
    }
    let index = TextIndex::open(&root).unwrap();
    assert!(index.len().unwrap() > 0);
    assert!(!index
        .search("revenue", &SearchOptions::default())
        .unwrap()
        .is_empty());
}

fn a_schema_mismatch(tag: &str) -> std::path::PathBuf {
    let root = dir(tag);
    {
        let mut index = TextIndex::open(&root).unwrap();
        let wb = sales();
        index
            .index_built(&build(&wb), &wb.content_hash, &wb.path)
            .unwrap();
        assert!(index.len().unwrap() > 0);
    }

    // Stand in for a schema change by writing a tantivy index of a different
    // shape into the same directory.
    std::fs::remove_dir_all(root.join("text")).unwrap();
    std::fs::create_dir_all(root.join("text")).unwrap();
    let mut other = tantivy::schema::Schema::builder();
    other.add_text_field("something-else", tantivy::schema::TEXT);
    tantivy::Index::create_in_dir(root.join("text"), other.build()).unwrap();
    root
}

#[test]
fn open_refuses_a_stale_schema_rather_than_silently_discarding_it() {
    // `open` is what a read-only verb like `ask` calls: it must not delete
    // yesterday's index out from under a question just because this version
    // of `eg` writes a different schema. `open_or_reset` is the only place
    // that decision belongs, and only the indexing path calls it.
    let root = a_schema_mismatch("schema-readonly");
    let before: Vec<_> = std::fs::read_dir(root.join("text"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    match TextIndex::open(&root) {
        Err(IndexError::StaleSchema { .. }) => {}
        Ok(_) => panic!("a stale schema must be refused, not opened"),
        Err(other) => panic!("wrong error: {other}"),
    }

    let after: Vec<_> = std::fs::read_dir(root.join("text"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        before, after,
        "a refusal must not have touched the directory"
    );
}

#[test]
fn open_or_reset_rebuilds_over_a_stale_schema() {
    let root = a_schema_mismatch("schema-reset");
    let index = TextIndex::open_or_reset(&root).unwrap();
    assert_eq!(
        index.len().unwrap(),
        0,
        "rebuilt empty, which only the indexing path may do"
    );
}

#[test]
fn a_graph_out_of_the_corpus_indexes_the_same_as_a_fresh_one() -> Result<(), IndexError> {
    let root = dir("corpus");
    let wb = sales();
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put(
            &wb.content_hash,
            &wb.path,
            wb.sheets.len(),
            wb.total_cells() as u64,
            true,
            &built,
        )
        .unwrap();
    let stored = corpus.get(&wb.content_hash).unwrap().unwrap();

    let mut index = TextIndex::open(&root)?;
    index.index_stored(&stored)?;

    let hits = index.search("revenue", &SearchOptions::default())?;
    assert_eq!(hits[0].label, "Revenue");
    assert_eq!(hits[0].sheet.as_deref(), Some("Q3 Sales"));
    Ok(())
}

#[test]
fn a_read_only_index_does_not_hold_the_writer_lock() -> Result<(), IndexError> {
    // Every read path opens a `TextIndex`: `eg ask`, `eg search`, and `eg
    // serve`, which holds one for as long as the server runs. Opening the
    // writer eagerly reserved 64 MB of indexing arena and took tantivy's
    // exclusive directory lock for each of them, so a running server locked
    // the corpus against `eg index` — and against a second reader.
    let (root, mut writer) = indexed("readonly-lock", &[sales()]);

    let reader = TextIndex::open(&root)?;
    let another = TextIndex::open(&root)?;
    assert!(!reader
        .search("revenue", &SearchOptions::default())?
        .is_empty());
    assert!(!another
        .search("revenue", &SearchOptions::default())?
        .is_empty());

    // And a write still works while those two readers are open, because
    // neither of them ever asked for the lock.
    writer.forget("hash-sales")?;
    assert!(TextIndex::open(&root)?.is_empty()?);
    Ok(())
}
