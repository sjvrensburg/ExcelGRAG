//! Whether the right node comes back, scored against the demo workbook.
//!
//! `tests/answers.rs` beside this one holds the floor against a workbook built
//! in a few lines of Rust: every answer there is one a reader would give
//! without argument, which is what makes a failure a change in retrieval rather
//! than a difference of opinion. That is worth keeping and it is not enough. It
//! has four columns and no depth, so a walk budget, a fusion weight or a
//! containment rule can only be wrong in ways a toy can show.
//!
//! This scores the same pipeline against `tests/fixtures/demo/impairment.xlsx`
//! — a workbook with two thousand rows, formula columns, a banding, a defined
//! name, cross-sheet dependencies and prose to search. The questions live in
//! `answers.json` beside it, in the format `eg-retrieve --example answers`
//! reads, so the committed floor and the scorer anyone can run are the same
//! questions and the same file.
//!
//! By word only, because a model download is not a thing a test may depend on.
//! The numbers here are a *floor*, not a target: retrieval that got better
//! should raise them, and moving them the other way needs a reason written
//! down.

use std::path::{Path, PathBuf};

use eg_graph::build;
use eg_graph::store::Corpus;
use eg_index::{SearchOptions, TextIndex};
use eg_retrieve::{expand, find, render, ExpandOptions, Fusion, RenderOptions};
use eg_structure::{detect_regions, profile_table, read_table, ProfileOptions, Profiles};
use serde::Deserialize;

#[derive(Deserialize)]
struct Question {
    ask: String,
    want: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    why: String,
    /// Set when retrieval is known not to answer this, and why. Kept in the
    /// file rather than deleted: a question dropped for being inconvenient is a
    /// gap nobody is measuring. The assertion below is that the misses are
    /// *exactly* these, so a new one fails and a fixed one fails too.
    #[serde(default)]
    known_gap: Option<String>,
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/demo")
}

fn questions() -> Vec<Question> {
    let path = fixtures().join("answers.json");
    let text = std::fs::read_to_string(&path).expect("the demo questions are committed");
    serde_json::from_str(&text).expect("answers.json parses")
}

fn corpus_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "eg-demo-answers-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// A corpus and a lexical index over the demo workbook, the way `eg index`
/// builds one.
///
/// With profiles, because `eg index` builds them by default and they are now
/// part of what the index holds: a corpus without them cannot be asked about
/// a workbook in the vocabulary of its values, and a floor measured over one
/// would be a floor under a pipeline nobody runs.
fn indexed(tag: &str) -> PathBuf {
    let root = corpus_dir(tag);
    let loaded =
        eg_ingest::load(fixtures().join("impairment.xlsx")).expect("the demo fixture is committed");
    let wb = &loaded.workbook;
    let built = build(wb);

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
    let mut text = TextIndex::open(&root).unwrap();
    text.index_graph_with(
        &built.graph,
        &wb.content_hash,
        &wb.path,
        Some(&profiled(wb)),
    )
    .unwrap();
    root
}

/// Every column of the workbook, profiled the way `eg index` profiles them.
fn profiled(workbook: &eg_model::Workbook) -> Profiles {
    let opts = ProfileOptions::default();
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

/// Where the first acceptable answer lands, and whether the passage cites it.
fn answer(root: &Path, q: &Question) -> (Option<usize>, bool) {
    let dir = root.to_str().unwrap();
    let found = find(
        dir,
        &q.ask,
        &SearchOptions {
            limit: 8,
            ..Default::default()
        },
        &Fusion::lexical(),
    )
    .unwrap();
    let hits = found.hits;
    let rank = hits
        .iter()
        .position(|h| {
            q.want.iter().any(|w| {
                h.label.eq_ignore_ascii_case(w)
                    || h.a1.as_deref().is_some_and(|a1| a1.eq_ignore_ascii_case(w))
            })
        })
        .map(|i| i + 1);

    let corpus = Corpus::open(root).unwrap();
    let found = expand(
        &corpus,
        &hits,
        &ExpandOptions {
            budget: 40,
            ..Default::default()
        },
    )
    .unwrap();
    let rendered = render(&found, &RenderOptions::default());
    let cited = found
        .workbooks
        .iter()
        .flat_map(|w| w.nodes.iter())
        .any(|n| q.want.iter().any(|w| n.label.eq_ignore_ascii_case(w)))
        || rendered
            .citations
            .iter()
            .any(|c| q.want.iter().any(|w| c.eq_ignore_ascii_case(w)));
    (rank, cited)
}

#[test]
fn the_passage_answers_every_question_but_the_ones_recorded_as_gaps() {
    // The number that matters: the passage is the product, and a node ranked
    // first and then squeezed out of the walk's budget has answered nothing.
    let root = indexed("context");
    let questions = questions();
    let missed: Vec<&str> = questions
        .iter()
        .filter(|q| !answer(&root, q).1)
        .map(|q| q.ask.as_str())
        .collect();
    let _ = std::fs::remove_dir_all(&root);

    let expected: Vec<&str> = questions
        .iter()
        .filter(|q| q.known_gap.is_some())
        .map(|q| q.ask.as_str())
        .collect();
    assert_eq!(
        missed, expected,
        "the questions the passage does not answer are not the ones answers.json records"
    );
}

#[test]
fn the_right_node_is_at_or_near_the_top_of_the_ranking() {
    // Ranking decides which node seeds the walk, so it is worth holding down
    // separately from whether the passage happened to cover the answer anyway.
    let root = indexed("ranking");
    let questions = questions();
    let answerable: Vec<&Question> = questions.iter().filter(|q| q.known_gap.is_none()).collect();
    let ranks: Vec<(&str, Option<usize>)> = answerable
        .iter()
        .map(|q| (q.ask.as_str(), answer(&root, q).0))
        .collect();
    let _ = std::fs::remove_dir_all(&root);
    let n = answerable.len();

    let found = ranks.iter().filter(|(_, r)| r.is_some()).count();
    assert_eq!(
        found, n,
        "some questions rank nothing acceptable in their first 8: {ranks:?}"
    );

    let mrr: f64 = ranks
        .iter()
        .map(|(_, r)| r.map_or(0.0, |r| 1.0 / r as f64))
        .sum::<f64>()
        / n as f64;
    assert!(
        mrr >= MRR_FLOOR,
        "mean reciprocal rank fell to {mrr:.3}, floor is {MRR_FLOOR:.3}: {ranks:?}"
    );
}

/// Measured at 0.833 over the sixteen answerable questions, and set below that
/// so that ordinary movement does not fail the suite and a real regression
/// does.
///
/// It was 0.804 when one question — the column a table is keyed by — could not
/// be answered at all. Giving row-label columns nodes of their own took hit@5
/// to 100% and the passage to citing every answer, and took it to 0.863 over
/// the fourteen questions there were then.
///
/// The move to 0.833 is a **larger set, not a worse ranking**: indexing what a
/// column was profiled to hold added two answerable questions asked in the
/// vocabulary of the workbook's values, and left all fourteen of the others at
/// exactly the ranks they had. One of the two — "which accounts are in
/// Ironwood" — comes fourth, because `accounts` is a word two column headers
/// carry and `Ironwood` is only ever a cell, so the headers win the first
/// three places on a stronger field. That is the ranking working: a value is
/// weaker evidence than a name, and the search says so in its evidence line.
///
/// Retrieval that got better should raise this, and moving it the other way
/// needs a reason written down.
///
/// This is comparable with what `--example answers` prints only while nothing
/// in the file is a recorded gap — the example divides by every question, this
/// by the ones without one.
const MRR_FLOOR: f64 = 0.80;
