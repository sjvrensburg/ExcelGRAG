//! Whether the right node comes back for a question a person would ask.
//!
//! The layers below this one each have a check that fails loudly — the reader
//! is diffed against a second reader, the graph's edges are re-derived from the
//! cells, every formula is recomputed against what Excel cached. Retrieval had
//! none, so a change to the tokenizer, the cell-count multiplier, the fusion or
//! the walk's budget could quietly make answers worse and nothing would say so.
//!
//! The workbook here is small and made up, and that is the point: every answer
//! below is one a reader would give without argument, so a failure is a change
//! in retrieval and never a difference of opinion. It measures the same
//! pipeline `eg ask` runs — [`eg_retrieve::find`] and then [`expand`] — under
//! the same defaults, by word only, because a model download is not a thing a
//! test may depend on.
//!
//! The number this holds down is a *floor*, not a target. Retrieval that got
//! better should raise it; the reason to touch it in the other direction has to
//! be written down.

use eg_graph::build;
use eg_graph::store::Corpus;
use eg_index::{SearchOptions, TextIndex};
use eg_model::{Cell, CellValue, Sheet, SheetId, Workbook, WorkbookFormat};
use eg_retrieve::{expand, find, render, ExpandOptions, Fusion, RenderOptions};

fn grid(id: u16, name: &str, rows: &[&str]) -> Sheet {
    let mut sheet = Sheet::new(SheetId(id), name);
    for (r, line) in rows.iter().enumerate() {
        for (c, token) in line.split('|').enumerate() {
            let token = token.trim();
            if token.is_empty() || token == "." {
                continue;
            }
            let cell = match token.strip_prefix('=') {
                Some(formula) => Cell {
                    value: CellValue::Number(0.0),
                    formula: Some(formula.to_string()),
                    format: Default::default(),
                },
                None => match token.parse::<f64>() {
                    Ok(n) => Cell::literal(CellValue::Number(n)),
                    Err(_) => Cell::literal(CellValue::Text(token.to_string())),
                },
            };
            sheet.set(r as u32, c as u16, cell);
        }
    }
    sheet
}

/// A small debtors book, shaped like the real one: a working sheet of per-
/// customer columns, a rate table it looks into, and a summary that adds it up.
fn workbook() -> Workbook {
    Workbook {
        path: "debtors.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-answers".into(),
        sheets: vec![
            grid(
                0,
                "Work Doc",
                &[
                    "Customer | Debt Type | Total Debt | Discount Rate | PV of expected receipts | Impairment provision",
                    "North | Residential | 1200 | =VLOOKUP(B2,Rates!A:B,2,FALSE) | =C2/(1+D2) | =C2-E2",
                    "South | Business | 3400 | =VLOOKUP(B3,Rates!A:B,2,FALSE) | =C3/(1+D3) | =C3-E3",
                    "East | Residential | 900 | =VLOOKUP(B4,Rates!A:B,2,FALSE) | =C4/(1+D4) | =C4-E4",
                ],
            ),
            grid(
                1,
                "Rates",
                &[
                    "Debt Type | Discount Rate",
                    "Residential | 0.08",
                    "Business | 0.11",
                    "Indigent | 0.15",
                ],
            ),
            grid(
                2,
                "Summary",
                &[
                    "Measure | Amount",
                    "Total debt outstanding | =SUM('Work Doc'!C2:C4)",
                    "Provision for doubtful debts | =SUM('Work Doc'!F2:F4)",
                    "Net receivable | =B2-B3",
                ],
            ),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

/// A question, the nodes any reader would accept as its answer, and — where
/// the answer is currently out of reach — why.
struct Question {
    ask: &'static str,
    want: &'static [&'static str],
    /// `Some` when retrieval is known not to answer this, and the reason. Kept
    /// in the set rather than deleted: a question dropped for being
    /// inconvenient is a gap nobody is measuring, and the assertion below is
    /// that the misses are *exactly* these — so one that starts working says
    /// so, and a new one still fails.
    known_gap: Option<&'static str>,
}

/// Deliberately phrased the way a person asks rather than the way the workbook
/// is spelled, because a search that only answers its own vocabulary answers
/// nothing.
const QUESTIONS: &[Question] = &[
    Question {
        ask: "provision for doubtful debts",
        want: &["Provision for doubtful debts", "Impairment provision"],
        known_gap: None,
    },
    Question {
        ask: "discount rate",
        want: &["Discount Rate"],
        known_gap: None,
    },
    Question {
        ask: "present value of expected receipts",
        want: &["PV of expected receipts"],
        known_gap: None,
    },
    Question {
        ask: "total debt",
        want: &["Total Debt", "Total debt outstanding"],
        known_gap: None,
    },
    Question {
        ask: "what rates are used for each debt type",
        want: &["Rates", "Discount Rate"],
        known_gap: None,
    },
    Question {
        ask: "impairment",
        want: &["Impairment provision"],
        known_gap: None,
    },
    Question {
        ask: "customer",
        want: &["Customer"],
        // Region detection reads the leftmost column as row labels, so it heads
        // nothing and gets no column node — see the test at the bottom of this
        // file. Nothing in the graph is called "Customer", so no amount of
        // ranking will return it. Asking about the thing a table is *keyed by*
        // is an ordinary question, and this is the honest record that it does
        // not work.
        known_gap: Some("the row-label column gets no node"),
    },
    Question {
        ask: "summary",
        want: &["Summary"],
        known_gap: None,
    },
];

fn corpus_dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "eg-answers-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// A corpus and a lexical index over the fixture, the way `eg index` builds one.
fn indexed(tag: &str) -> std::path::PathBuf {
    let root = corpus_dir(tag);
    let wb = workbook();
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
    let mut text = TextIndex::open(&root).unwrap();
    text.index_built(&built, &wb.content_hash, &wb.path)
        .unwrap();
    root
}

/// Where the first acceptable answer lands, and whether the passage cites it.
fn answer(root: &std::path::Path, q: &Question) -> (Option<usize>, bool) {
    let dir = root.to_str().unwrap();
    let hits = find(
        dir,
        q.ask,
        &SearchOptions {
            limit: 8,
            ..Default::default()
        },
        &Fusion::lexical(),
    )
    .unwrap();
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
    let in_context = found
        .workbooks
        .iter()
        .flat_map(|w| w.nodes.iter())
        .any(|n| q.want.iter().any(|w| n.label.eq_ignore_ascii_case(w)))
        || rendered
            .citations
            .iter()
            .any(|c| q.want.iter().any(|w| c.eq_ignore_ascii_case(w)));
    (rank, in_context)
}

#[test]
fn the_passage_answers_every_question_but_the_ones_recorded_as_gaps() {
    // The number that matters: the passage is the product, and a node ranked
    // first and then squeezed out by the budget has answered nothing.
    let root = indexed("context");
    let missed: Vec<&str> = QUESTIONS
        .iter()
        .filter(|q| !answer(&root, q).1)
        .map(|q| q.ask)
        .collect();
    let _ = std::fs::remove_dir_all(&root);

    let expected: Vec<&str> = QUESTIONS
        .iter()
        .filter(|q| q.known_gap.is_some())
        .map(|q| q.ask)
        .collect();
    assert_eq!(
        missed, expected,
        "the questions the passage does not answer are not the ones recorded"
    );
}

#[test]
fn the_right_node_is_at_or_near_the_top_of_the_ranking() {
    // A floor, not a target. Ranking is what decides which node seeds the walk,
    // so it moving is worth knowing about even when the passage still covers
    // the answer.
    let root = indexed("ranking");
    let answerable: Vec<&Question> = QUESTIONS.iter().filter(|q| q.known_gap.is_none()).collect();
    let ranks: Vec<(&str, Option<usize>)> = answerable
        .iter()
        .map(|q| (q.ask, answer(&root, q).0))
        .collect();
    let _ = std::fs::remove_dir_all(&root);
    let n = answerable.len();

    let found = ranks.iter().filter(|(_, r)| r.is_some()).count();
    assert_eq!(
        found, n,
        "some questions rank nothing acceptable in their first 8: {ranks:?}"
    );

    let first = ranks.iter().filter(|(_, r)| *r == Some(1)).count();
    assert!(
        first * 4 >= n * 3,
        "only {first} of {n} questions put an acceptable answer first: {ranks:?}"
    );

    let mrr: f64 = ranks
        .iter()
        .map(|(_, r)| r.map_or(0.0, |r| 1.0 / r as f64))
        .sum::<f64>()
        / n as f64;
    assert!(
        mrr >= 0.85,
        "mean reciprocal rank fell to {mrr:.3}, floor is 0.850: {ranks:?}"
    );
}

#[test]
fn what_the_graph_has_no_node_for_cannot_be_found() {
    // The diagnosis behind the known gap above, asserted rather than assumed:
    // region detection treats the leftmost column as row labels, so it heads
    // nothing and gets no column node. There is no node named "Customer" for
    // any amount of ranking work to return.
    let wb = workbook();
    let built = build(&wb);
    let headers: Vec<String> = built
        .graph
        .node_weights()
        .filter(|n| n.kind() == eg_graph::NodeKind::Column)
        .map(|n| n.label())
        .collect();
    assert!(
        headers.iter().any(|h| h == "Total Debt"),
        "the body columns are there: {headers:?}"
    );
    assert!(
        !headers.iter().any(|h| h == "Customer"),
        "the row-label column is not: {headers:?}"
    );
}
