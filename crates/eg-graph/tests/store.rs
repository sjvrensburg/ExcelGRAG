//! Storing and reloading a workbook graph.

use eg_graph::store::{Corpus, StoreError, FORMAT_VERSION};
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
fn a_failed_forget_restores_the_in_memory_manifest() {
    let root = dir();
    let wb = workbook("hash-forget-failure");
    let built = build(&wb);
    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put(&wb.content_hash, &wb.path, 2, 12, false, &built)
        .unwrap();

    let manifest = root.join("manifest.json");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();
    assert!(corpus.forget(&wb.content_hash).is_err());
    assert!(
        corpus.entries().any(|(hash, _)| hash == wb.content_hash),
        "an operation returning Err must remain retryable in this handle"
    );

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

#[test]
fn a_file_whose_shape_belongs_to_another_version_is_a_miss() {
    // The version bump *is* the shape change, so a stored file from another
    // version does not deserialise into this one's `StoredGraph`. Reaching the
    // version field only after deserialising makes the gate unreachable in
    // exactly the case it exists for, and turns a stale cache into a hard
    // error that no rebuild can get past.
    let root = dir();
    let wb = workbook("hash-shape");
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put("hash-shape", &wb.path, 2, 12, false, &built)
        .unwrap();

    let file = std::fs::read_dir(root.join("graphs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(
        &file,
        br#"{"version":999999,"content_hash":"hash-shape","shape_from_the_future":[]}"#,
    )
    .unwrap();
    assert!(corpus.get("hash-shape").unwrap().is_none());

    // And the same for a manifest, whose `Entry` shape moves with the version.
    std::fs::write(
        root.join("manifest.json"),
        br#"{"version":999999,"workbooks":{"hash-shape":{"path":"book.xlsx"}}}"#,
    )
    .unwrap();
    let fresh = Corpus::open(&root).expect("a foreign manifest is discarded, not an error");
    assert!(fresh.is_empty());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_root_outside_the_graph_is_a_miss() {
    // A petgraph index means nothing except against the graph it came from.
    // Handed back unchecked, a root one past the end panics in the first
    // caller that looks the node up, a long way from the file that caused it.
    let root = dir();
    let wb = workbook("hash-root");
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put("hash-root", &wb.path, 2, 12, false, &built)
        .unwrap();

    let file = std::fs::read_dir(root.join("graphs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let text = std::fs::read_to_string(&file).unwrap();
    let broken = text.replacen("\"root\":0", "\"root\":4294967295", 1);
    assert_ne!(broken, text, "the root field should be present");
    std::fs::write(&file, broken).unwrap();

    assert!(corpus.get("hash-root").unwrap().is_none());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn node_indices_survive_the_round_trip() {
    // Every node payload carries indices implicitly: `root` is one, and every
    // edge is a pair of them. A serialisation that renumbered nodes would
    // still reload into a valid graph — just one describing a different
    // workbook — so this compares index for index, not totals.
    let root = dir();
    let wb = workbook("hash-idx");
    let built = build(&wb);

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put("hash-idx", &wb.path, wb.sheets.len(), 12, true, &built)
        .unwrap();
    let reloaded = corpus
        .get("hash-idx")
        .unwrap()
        .expect("stored")
        .into_built();

    assert_eq!(reloaded.root, built.root);
    assert_eq!(reloaded.graph.node_count(), built.graph.node_count());
    for i in built.graph.node_indices() {
        assert_eq!(reloaded.graph[i], built.graph[i], "node {i:?} moved");
    }

    let edges = |g: &eg_graph::Graph| -> Vec<(usize, usize, EdgeKind, u64)> {
        g.edge_indices()
            .map(|e| {
                let (a, b) = g.edge_endpoints(e).unwrap();
                (a.index(), b.index(), g[e].kind, g[e].weight)
            })
            .collect()
    };
    assert_eq!(edges(&reloaded.graph), edges(&built.graph));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn profiles_are_stored_beside_the_graph_and_not_inside_it() {
    // Everything in `graphs/` is structure, and a reader can hand that whole
    // directory to someone who may not see the workbook. A profile carries
    // distinct values and sums, which are the workbook's data — so it is its
    // own file, and the invariant about the graph stays true rather than
    // becoming a footnote.
    let root = dir();
    let wb = workbook("hash-profiles");
    let built = build(&wb);
    let profiles = eg_structure::Profiles {
        columns: vec![eg_structure::ColumnProfile {
            header: "Debt Type".into(),
            range: eg_model::RangeRef::new(SheetId(0), 1, 1, 3, 1),
            kind: eg_structure::ColumnKind::Text,
            populated: 3,
            empty: 0,
            errors: 0,
            distinct: Some(vec![eg_structure::ValueCount {
                value: "Retail".into(),
                count: 2,
                truncated: false,
            }]),
            distinct_count: Some(1),
            numeric: None,
        }],
        values: true,
    };

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put(&wb.content_hash, &wb.path, 1, 3, true, &built)
        .unwrap();
    assert_eq!(
        corpus.profiles(&wb.content_hash).unwrap(),
        None,
        "a workbook nobody profiled is an ordinary state, not an error"
    );

    corpus
        .put_profiles(&wb.content_hash, &wb.path, &profiles)
        .unwrap();
    let back = corpus.profiles(&wb.content_hash).unwrap().unwrap();
    assert_eq!(back, profiles);

    // Two files, and the graph's is untouched by any of it.
    let graph_bytes = std::fs::read(corpus.graph_path(&wb.content_hash)).unwrap();
    assert!(
        !String::from_utf8_lossy(&graph_bytes).contains("Retail"),
        "no value reached the graph file"
    );
    assert!(corpus.profiles_path(&wb.content_hash).exists());

    // And withdrawable on its own.
    corpus.forget_profiles(&wb.content_hash).unwrap();
    assert_eq!(corpus.profiles(&wb.content_hash).unwrap(), None);
    assert!(
        corpus.get(&wb.content_hash).unwrap().is_some(),
        "the graph is still there"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn forgetting_a_workbook_removes_its_profiles_too() {
    // A "forgotten" workbook's cell values must not go on being served: the
    // manifest is what `profiles()` now consults first, but the file itself
    // must also be gone, not just orphaned on disk.
    let root = dir();
    let wb = workbook("hash-forget-profiles");
    let built = build(&wb);
    let profiles = eg_structure::Profiles {
        columns: vec![eg_structure::ColumnProfile {
            header: "Debt Type".into(),
            range: eg_model::RangeRef::new(SheetId(0), 1, 1, 3, 1),
            kind: eg_structure::ColumnKind::Text,
            populated: 3,
            empty: 0,
            errors: 0,
            distinct: Some(vec![eg_structure::ValueCount {
                value: "Retail".into(),
                count: 2,
                truncated: false,
            }]),
            distinct_count: Some(1),
            numeric: None,
        }],
        values: true,
    };

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put(&wb.content_hash, &wb.path, 1, 3, true, &built)
        .unwrap();
    corpus
        .put_profiles(&wb.content_hash, &wb.path, &profiles)
        .unwrap();
    assert!(corpus.profiles_path(&wb.content_hash).exists());

    assert!(corpus.forget(&wb.content_hash).unwrap());
    assert!(
        !corpus.profiles_path(&wb.content_hash).exists(),
        "the profiles file must not outlive the workbook it describes"
    );
    assert_eq!(corpus.profiles(&wb.content_hash).unwrap(), None);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profiles_are_a_miss_when_the_manifest_does_not_list_them() {
    // `profiles()` must consult the manifest before reading the file — a stray
    // or stale file on disk with no matching manifest entry (an orphan left by
    // a partial write, or a filename-prefix collision) must never be served.
    let root = dir();
    let wb = workbook("hash-orphan-profile");
    let built = build(&wb);
    let profiles = eg_structure::Profiles {
        columns: Vec::new(),
        values: true,
    };

    let mut corpus = Corpus::open(&root).unwrap();
    corpus
        .put(&wb.content_hash, &wb.path, 1, 3, true, &built)
        .unwrap();
    corpus
        .put_profiles(&wb.content_hash, &wb.path, &profiles)
        .unwrap();
    corpus.forget_profiles(&wb.content_hash).unwrap();

    // The file may still exist on disk (forget_profiles removes it too, so
    // recreate it directly to simulate an orphan/collision) but the manifest
    // no longer claims it.
    std::fs::write(corpus.profiles_path(&wb.content_hash), b"{}").unwrap();
    assert_eq!(
        corpus.profiles(&wb.content_hash).unwrap(),
        None,
        "a file with no manifest entry must not be served"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_corpus_written_before_profiles_existed_still_opens() {
    // The manifest gained two fields. Defaulting them rather than bumping the
    // format is the difference between reading an old corpus as "no profiles"
    // and dropping every corpus on disk.
    let root = dir();
    let wb = workbook("hash-old");
    {
        let mut corpus = Corpus::open(&root).unwrap();
        corpus
            .put(&wb.content_hash, &wb.path, 1, 3, true, &build(&wb))
            .unwrap();
    }
    let manifest = root.join("manifest.json");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let stripped = text
        .replace(",\n      \"profiled_columns\": 0", "")
        .replace(",\n      \"profile_values\": false", "");
    assert_ne!(stripped, text, "the fields were there to strip");
    std::fs::write(&manifest, stripped).unwrap();

    let corpus = Corpus::open(&root).unwrap();
    let entry = corpus.entries().next().expect("the workbook survived");
    assert_eq!(entry.1.profiled_columns, 0);
    assert!(!entry.1.profile_values);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn profiles_for_a_workbook_the_corpus_does_not_hold_are_refused_not_orphaned() {
    // The manifest is the corpus: `profiles` reads through it, and so does
    // `forget`. A profiles file written for a hash the manifest does not list
    // could never be read back and would never be cleaned up — and this is the
    // one file the store writes that carries the workbook's own values, which
    // is the last thing to leave lying in a directory nobody is tracking.
    let root = dir();
    let mut corpus = Corpus::open(&root).unwrap();

    let profiles = eg_structure::Profiles {
        columns: Vec::new(),
        values: true,
    };
    let err = corpus
        .put_profiles("never-stored", "book.xlsx", &profiles)
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::NotInCorpus { content_hash } if content_hash == "never-stored"),
        "{err}"
    );
    assert!(
        !corpus.profiles_path("never-stored").exists(),
        "refused before anything was written, not after"
    );
    assert_eq!(corpus.profiles("never-stored").unwrap(), None);
}
