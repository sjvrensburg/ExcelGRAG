//! Storing and searching vectors.
//!
//! These use vectors made by hand rather than by the model. What is under test
//! is the store and the scan — that a set survives a round trip bit for bit,
//! that the filters bite, that a model change is noticed. Whether the model
//! places `Recoverability` near "bad debt" is the model's business, and pinning
//! it in a test would pin the model version instead.

use eg_graph::{build, NodeKind};
use eg_index::doc::NodeDoc;
use eg_index::embed::normalize;
use eg_index::vector::{embeddable, VectorIndex};
use eg_index::SearchOptions;
use eg_model::{Cell, CellValue, DefinedName, Sheet, SheetId, Workbook, WorkbookFormat};

const DIM: usize = 8;
const MODEL: &str = "test-model";

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
                    "North 10 =B2*2",
                    "South 20 =B3*2",
                    "East 30 =B4*2",
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

fn dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "eg-index-vec-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// A deterministic stand-in for the model: every document gets a unit vector
/// derived from its label, so two labels are near each other only if they are
/// the same label.
fn fake_vectors(docs: &[NodeDoc]) -> Vec<Vec<f32>> {
    docs.iter()
        .map(|d| {
            let mut v = vec![0.0f32; DIM];
            for (i, b) in d.label.bytes().enumerate() {
                v[i % DIM] += f32::from(b) / 255.0;
            }
            normalize(&mut v);
            v
        })
        .collect()
}

fn vector_for(label: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    for (i, b) in label.bytes().enumerate() {
        v[i % DIM] += f32::from(b) / 255.0;
    }
    normalize(&mut v);
    v
}

fn indexed(tag: &str) -> (std::path::PathBuf, VectorIndex, Vec<NodeDoc>) {
    let root = dir(tag);
    let wb = sales();
    let built = build(&wb);
    let docs = embeddable(&built.graph);
    let vectors = fake_vectors(&docs);

    let mut index = VectorIndex::open(&root, MODEL, DIM).unwrap();
    index
        .put(&wb.content_hash, &wb.path, &docs, &vectors)
        .unwrap();
    (root, index, docs)
}

#[test]
fn formula_groups_are_left_out_of_what_gets_embedded() {
    let built = build(&sales());
    let docs = embeddable(&built.graph);

    assert!(
        docs.iter().all(|d| d.kind != NodeKind::FormulaGroup),
        "a formula group reached the embedder"
    );
    // Everything else is still there.
    let groups = built.report.nodes_of(NodeKind::FormulaGroup) as usize;
    assert_eq!(docs.len(), built.graph.node_count() - groups);
    assert!(groups > 0, "the fixture should have had groups to exclude");
}

#[test]
fn the_nearest_vector_to_a_label_is_that_label() {
    let (_root, index, _docs) = indexed("nearest");
    let hits = index.search(&vector_for("Revenue"), &SearchOptions::default());
    assert_eq!(hits[0].label, "Revenue");
    assert!(hits[0].score > 0.99, "score was {}", hits[0].score);
}

#[test]
fn hits_come_back_in_descending_order_and_within_the_limit() {
    let (_root, index, _docs) = indexed("order");
    let hits = index.search(
        &vector_for("Revenue"),
        &SearchOptions {
            limit: 3,
            ..Default::default()
        },
    );
    assert_eq!(hits.len(), 3);
    for pair in hits.windows(2) {
        assert!(
            pair[0].score >= pair[1].score,
            "{:?} then {:?}",
            pair[0].score,
            pair[1].score
        );
    }
}

#[test]
fn filters_bite_on_a_vector_search_too() {
    let (_root, index, _docs) = indexed("filters");
    let q = vector_for("Revenue");

    let columns = index.search(
        &q,
        &SearchOptions {
            kinds: vec![NodeKind::Column],
            limit: 50,
            ..Default::default()
        },
    );
    assert!(!columns.is_empty());
    assert!(columns.iter().all(|h| h.kind == NodeKind::Column));

    let on_rates = index.search(
        &q,
        &SearchOptions {
            sheet: Some("Rates".into()),
            limit: 50,
            ..Default::default()
        },
    );
    assert!(!on_rates.is_empty());
    assert!(on_rates.iter().all(|h| h.sheet.as_deref() == Some("Rates")));

    let elsewhere = index.search(
        &q,
        &SearchOptions {
            workbook: Some("no-such-hash".into()),
            ..Default::default()
        },
    );
    assert!(elsewhere.is_empty());
}

#[test]
fn a_set_survives_reopening_bit_for_bit() {
    let (root, index, _docs) = indexed("reopen");
    let before = index.search(&vector_for("Revenue"), &SearchOptions::default());

    let reopened = VectorIndex::open(&root, MODEL, DIM).unwrap();
    assert_eq!(reopened.len(), index.len());
    let after = reopened.search(&vector_for("Revenue"), &SearchOptions::default());

    assert_eq!(before.len(), after.len());
    for (b, a) in before.iter().zip(&after) {
        assert_eq!(b.node, a.node);
        // Exactly, not nearly: the numbers are stored as raw floats precisely
        // so that reloading cannot move a ranking.
        assert_eq!(b.score, a.score);
    }
}

#[test]
fn vectors_from_another_model_are_not_loaded() {
    let (root, _index, _docs) = indexed("model");
    let other = VectorIndex::open(&root, "a-different-model", DIM).unwrap();
    assert_eq!(other.len(), 0);
    assert!(!other.contains("hash-sales"));

    // Same model, different width — also not comparable.
    let wider = VectorIndex::open(&root, MODEL, DIM * 2).unwrap();
    assert_eq!(wider.len(), 0);
}

#[test]
fn reindexing_replaces_a_workbook_rather_than_doubling_it() {
    let (_root, mut index, docs) = indexed("reindex");
    let before = index.len();
    let wb = sales();
    index
        .put(&wb.content_hash, &wb.path, &docs, &fake_vectors(&docs))
        .unwrap();
    assert_eq!(index.len(), before);
    assert_eq!(index.workbooks(), 1);
}

#[test]
fn forgetting_a_workbook_removes_its_files_too() {
    let (root, mut index, _docs) = indexed("forget");
    assert!(index.forget("hash-sales").unwrap());
    assert_eq!(index.len(), 0);
    assert!(!index.forget("hash-sales").unwrap());

    let reopened = VectorIndex::open(&root, MODEL, DIM).unwrap();
    assert_eq!(reopened.len(), 0);
}

#[test]
fn a_vector_of_the_wrong_width_is_refused_rather_than_stored() {
    let root = dir("width");
    let wb = sales();
    let built = build(&wb);
    let docs = embeddable(&built.graph);
    let mut index = VectorIndex::open(&root, MODEL, DIM).unwrap();

    let wrong: Vec<Vec<f32>> = docs.iter().map(|_| vec![0.0; DIM + 1]).collect();
    assert!(index
        .put(&wb.content_hash, &wb.path, &docs, &wrong)
        .is_err());

    let short = vec![vec![0.0; DIM]; docs.len() - 1];
    assert!(index
        .put(&wb.content_hash, &wb.path, &docs, &short)
        .is_err());

    assert_eq!(index.len(), 0, "a refused put must leave nothing behind");
}

#[test]
fn a_query_of_the_wrong_width_returns_nothing_rather_than_garbage() {
    let (_root, index, _docs) = indexed("qwidth");
    assert!(index
        .search(&[0.0; DIM + 3], &SearchOptions::default())
        .is_empty());
}

/// The one test that loads the real model.
///
/// Ignored by default: it downloads about 130 MB the first time and takes
/// seconds every time, neither of which belongs in `cargo test`. Run it with
/// `cargo test -p eg-index -- --ignored` when the embedding path changes.
///
/// It asserts the property the vector half exists for, and nothing finer: that
/// two ways of saying the same thing land closer together than two ways of
/// saying different things. Pinning actual scores would pin the model version.
#[test]
#[ignore = "downloads and runs the embedding model"]
fn the_model_puts_related_phrases_closer_than_unrelated_ones() {
    use eg_index::embed::similarity;
    use eg_index::Embedder;

    let mut embedder = Embedder::new().unwrap();
    assert_eq!(embedder.dim(), 384);

    let texts: Vec<String> = [
        "column Recoverability in Impairment",
        "column Bad Debt Provision in Impairment",
        "column Postal Code in Addresses",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let vectors = embedder.embed_texts(&texts).unwrap();

    let related = similarity(&vectors[0], &vectors[1]);
    let unrelated = similarity(&vectors[0], &vectors[2]);
    assert!(
        related > unrelated,
        "recoverability/bad debt scored {related}, recoverability/postal code {unrelated}"
    );

    // Every vector comes back unit length, which is what makes the dot product
    // a cosine in the scan.
    for v in &vectors {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
    }

    // The query side gets the instruction prefix, so it is not the same vector
    // as the same words embedded as a passage.
    let as_query = embedder.embed_query(&texts[0]).unwrap();
    assert!(similarity(&as_query, &vectors[0]) < 0.999);
}
