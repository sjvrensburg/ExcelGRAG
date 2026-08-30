//! Storing and reloading a workbook graph.

use eg_graph::store::{Corpus, FORMAT_VERSION};
use eg_graph::{build, EdgeKind, NodeKind};
use eg_model::{Cell, CellValue, Sheet, SheetId, Workbook, WorkbookFormat};

fn sheet(id: u16, name: &str, rows: &[&str]) -> Sheet {
    let mut sheet = Sheet::new(SheetId(id), name);
    for (r, line) in rows.iter().enumerate() {
        for (c, tok) in line.split_whitespace().enumerate() {
            let cell = match tok.strip_prefix('=') {
                Some(f) => Cell {
                    value: CellValue::Number(0.0),
                    formula: Some(f.to_string()),
                    format: Default::default(),
                },
                None => Cell::literal(CellValue::Text(tok.to_string())),
            };
            sheet.set(r as u32, c as u16, cell);
        }
    }
    sheet
}

fn workbook(hash: &str) -> Workbook {
    Workbook {
        path: "book.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: hash.into(),
        sheets: vec![
            sheet(
                0,
                "Sales",
                &["Region Net", "North =Rates!A2", "South =Rates!A3"],
            ),
            sheet(1, "Rates", &["Rate", "1", "2"]),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

fn dir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "eg-graph-store-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    base
}

#[test]
fn a_stored_graph_reloads_identically() {
    let root = dir();
    let wb = workbook("hash-a");
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    assert!(
        corpus.get("hash-a").unwrap().is_none(),
        "empty to begin with"
    );
    corpus
        .put("hash-a", &wb.path, wb.sheets.len(), 12, true, &built)
        .unwrap();

    // Reopened from disk, not from the in-memory manifest.
    let corpus = Corpus::open(&root).unwrap();
    let stored = corpus.get("hash-a").unwrap().expect("stored");
    assert_eq!(stored.version, FORMAT_VERSION);
    assert!(stored.formula_group_nodes);

    let reloaded = stored.into_built();
    assert_eq!(reloaded.report.total_nodes(), built.report.total_nodes());
    assert_eq!(reloaded.report.total_edges(), built.report.total_edges());
    assert_eq!(
        reloaded.report.edge_weight_of(EdgeKind::CrossSheetRef),
        built.report.edge_weight_of(EdgeKind::CrossSheetRef)
    );
    assert_eq!(reloaded.graph[reloaded.root].kind(), NodeKind::Workbook);
    // The invariants must hold on a graph that came off disk, not only on one
    // just built — nothing else would catch a lossy round trip.
    assert_eq!(eg_graph::check(&reloaded), vec![]);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_changed_workbook_is_a_miss_not_a_stale_hit() {
    // Freshness is by content hash, so a changed file cannot be served from a
    // graph built for the old one, whatever its path or timestamp.
    let root = dir();
    let wb = workbook("hash-before");
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put("hash-before", &wb.path, 2, 12, false, &built)
        .unwrap();

    assert!(corpus.get("hash-after").unwrap().is_none());
    assert!(corpus.get("hash-before").unwrap().is_some());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_same_workbook_under_two_paths_is_stored_once() {
    let root = dir();
    let wb = workbook("hash-same");
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put("hash-same", "a/book.xlsx", 2, 12, false, &built)
        .unwrap();
    corpus
        .put("hash-same", "b/book.xlsx", 2, 12, false, &built)
        .unwrap();

    assert_eq!(corpus.len(), 1);
    let (_, entry) = corpus.entries().next().unwrap();
    assert_eq!(entry.path, "b/book.xlsx", "the latest path wins");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn forgetting_a_workbook_removes_its_file() {
    let root = dir();
    let wb = workbook("hash-gone");
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put("hash-gone", &wb.path, 2, 12, false, &built)
        .unwrap();
    assert_eq!(std::fs::read_dir(root.join("graphs")).unwrap().count(), 1);

    assert!(corpus.forget("hash-gone").unwrap());
    assert!(!corpus.forget("hash-gone").unwrap(), "already gone");
    assert_eq!(std::fs::read_dir(root.join("graphs")).unwrap().count(), 0);
    assert!(corpus.is_empty());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_graph_file_from_another_version_is_a_miss() {
    // Rebuilding costs seconds. Deserialising a file whose shape we no longer
    // understand into something plausible costs a wrong answer.
    let root = dir();
    let wb = workbook("hash-old");
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put("hash-old", &wb.path, 2, 12, false, &built)
        .unwrap();

    let file = std::fs::read_dir(root.join("graphs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let text = std::fs::read_to_string(&file).unwrap();
    let bumped = text.replacen(
        &format!("\"version\":{FORMAT_VERSION}"),
        "\"version\":999999",
        1,
    );
    assert_ne!(bumped, text, "the version field should be present");
    std::fs::write(&file, bumped).unwrap();

    assert!(corpus.get("hash-old").unwrap().is_none());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_missing_graph_file_is_a_miss_not_an_error() {
    // The manifest and the files can drift — a partial copy, a cleaned
    // directory. Refusing to start over that would be the wrong trade.
    let root = dir();
    let wb = workbook("hash-orphan");
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put("hash-orphan", &wb.path, 2, 12, false, &built)
        .unwrap();
    for f in std::fs::read_dir(root.join("graphs")).unwrap() {
        std::fs::remove_file(f.unwrap().path()).unwrap();
    }

    assert!(corpus.get("hash-orphan").unwrap().is_none());

    std::fs::remove_dir_all(&root).ok();
}
