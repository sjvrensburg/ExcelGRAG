//! Rendering an expansion into a passage.
//!
//! What is under test is not how the prose reads. It is that the passage
//! accounts for every node retrieved, that every relation points at a number a
//! reader can look up, and that a citation in the list is a citation in the
//! text — the properties an agent's answer will be checked against.

use eg_graph::store::Corpus;
use eg_graph::{build, NodeKind};
use eg_index::Hit;
use eg_model::{Cell, CellValue, Sheet, SheetId, Workbook, WorkbookFormat};
use eg_retrieve::{expand, render, ExpandOptions, RenderOptions, Retrieved};

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

fn chain() -> Workbook {
    Workbook {
        path: "chain.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-chain".into(),
        sheets: vec![
            grid(
                0,
                "Report",
                &["Line Total", "North =Sales!B2", "South =Sales!B3"],
            ),
            grid(
                1,
                "Sales",
                &["Region Net", "North =Rates!B2", "South =Rates!B3"],
            ),
            grid(2, "Rates", &["Country Tariff", "ZA 0.15", "UK 0.2"]),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

fn dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "eg-retrieve-ctx-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    base
}

fn retrieved(tag: &str, label: &str, kind: NodeKind, opts: &ExpandOptions) -> Retrieved {
    let root = dir(tag);
    let wb = chain();
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
    let (index, node) = stored
        .graph
        .node_indices()
        .map(|i| (i, &stored.graph[i]))
        .find(|(_, n)| n.kind() == kind && n.label() == label)
        .unwrap_or_else(|| panic!("no {kind:?} labelled {label}"));
    let seed = Hit {
        score: 1.0,
        workbook: wb.content_hash.clone(),
        path: wb.path.clone(),
        node: index.index() as u32,
        kind: node.kind(),
        sheet: None,
        label: node.label(),
        a1: None,
    };
    expand(&corpus, &[seed], opts).unwrap()
}

/// The `[n]` markers that open an entry, in order.
fn entries(text: &str) -> Vec<usize> {
    text.lines()
        .filter_map(|l| l.strip_prefix('['))
        .filter_map(|l| l.split(']').next())
        .filter_map(|n| n.parse().ok())
        .collect()
}

/// Every `[n]` mentioned anywhere, entries and relations alike.
fn referenced(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('[') {
        rest = &rest[at + 1..];
        if let Some(end) = rest.find(']') {
            if let Ok(n) = rest[..end].parse() {
                out.push(n);
            }
        }
    }
    out
}

#[test]
fn every_retrieved_node_appears_exactly_once() {
    let found = retrieved("once", "Net", NodeKind::Column, &ExpandOptions::default());
    let rendered = render(&found, &RenderOptions::default());

    let numbers = entries(&rendered.text);
    assert_eq!(
        numbers.len(),
        found.total_nodes(),
        "{} nodes rendered as {} entries",
        found.total_nodes(),
        numbers.len()
    );
    assert_eq!(
        numbers,
        (1..=found.total_nodes()).collect::<Vec<_>>(),
        "the numbering has a gap or a repeat"
    );
    assert_eq!(rendered.omitted, 0);
}

#[test]
fn no_relation_points_at_a_number_that_is_not_there() {
    // A dangling `[12]` in a passage is worse than a missing one: the agent
    // will cite it.
    let found = retrieved(
        "dangling",
        "Net",
        NodeKind::Column,
        &ExpandOptions::default(),
    );
    let rendered = render(&found, &RenderOptions::default());

    let highest = entries(&rendered.text).len();
    for n in referenced(&rendered.text) {
        assert!(
            n >= 1 && n <= highest,
            "the passage refers to [{n}] and only has {highest} entries"
        );
    }
}

#[test]
fn a_relation_survives_only_if_both_its_ends_do() {
    // Under a budget tight enough to drop nodes, the numbers that remain must
    // still all resolve.
    let found = retrieved(
        "budget",
        "Net",
        NodeKind::Column,
        &ExpandOptions {
            budget: 4,
            ..Default::default()
        },
    );
    let rendered = render(
        &found,
        &RenderOptions {
            max_chars: 400,
            ..Default::default()
        },
    );

    let highest = entries(&rendered.text).len();
    assert!(highest > 0, "nothing was rendered at all");
    assert!(
        rendered.omitted > 0,
        "this budget should have cut something"
    );
    for n in referenced(&rendered.text) {
        assert!(n <= highest, "[{n}] points past the end of the passage");
    }
}

#[test]
fn a_cut_passage_says_it_was_cut_and_by_how_much() {
    let found = retrieved("cut", "Net", NodeKind::Column, &ExpandOptions::default());
    let rendered = render(
        &found,
        &RenderOptions {
            max_chars: 300,
            ..Default::default()
        },
    );

    assert!(rendered.omitted > 0);
    assert!(
        rendered.text.contains("left out to fit"),
        "a cut passage that does not say so reads as a complete one"
    );
    // Never mid-entry: the last line of the list is a whole entry.
    assert!(rendered.text.ends_with('\n'));
}

#[test]
fn the_citations_handed_back_are_the_ones_in_the_text() {
    let found = retrieved("cites", "Net", NodeKind::Column, &ExpandOptions::default());
    let rendered = render(&found, &RenderOptions::default());

    assert!(!rendered.citations.is_empty());
    for citation in &rendered.citations {
        assert!(
            rendered.text.contains(citation),
            "{citation} is offered as a citation and is not in the passage"
        );
    }
    // And nothing was cited that the expansion did not carry.
    let known: Vec<&str> = found.workbooks[0]
        .nodes
        .iter()
        .filter_map(|n| n.a1.as_deref())
        .collect();
    for citation in &rendered.citations {
        assert!(known.contains(&citation.as_str()));
    }
}

#[test]
fn seeds_are_marked_and_come_first() {
    let found = retrieved("seeds", "Net", NodeKind::Column, &ExpandOptions::default());
    let rendered = render(&found, &RenderOptions::default());

    let first = rendered.text.lines().find(|l| l.starts_with('[')).unwrap();
    assert!(
        first.contains('*'),
        "the first entry is not the seed: {first}"
    );
    assert!(first.contains("Net"));

    let marked = rendered
        .text
        .lines()
        .filter(|l| l.starts_with('[') && l.contains('*'))
        .count();
    assert_eq!(marked, found.workbooks[0].seeds().count());
}

#[test]
fn weights_can_be_turned_off() {
    let found = retrieved(
        "weights",
        "Net",
        NodeKind::Column,
        &ExpandOptions::default(),
    );
    let with = render(&found, &RenderOptions::default());
    let without = render(
        &found,
        &RenderOptions {
            weights: false,
            ..Default::default()
        },
    );

    assert!(with.text.contains("refs"));
    assert!(!without.text.contains("refs"));
    assert!(without.text.len() < with.text.len());
    // The same nodes either way; only the annotation changed.
    assert_eq!(entries(&with.text).len(), entries(&without.text).len());
}

#[test]
fn a_workbook_that_left_the_corpus_is_said_out_loud() {
    let mut found = retrieved("gone", "Net", NodeKind::Column, &ExpandOptions::default());
    found.workbooks.clear();
    found.missing_workbooks.push("deadbeefcafe".into());

    let rendered = render(&found, &RenderOptions::default());
    assert!(rendered.text.contains("deadbeef"));
    assert!(rendered.text.contains("Reindex"));
}

#[test]
fn the_best_hit_survives_a_budget_too_small_for_the_preamble() {
    // A ceiling below the boilerplate would otherwise produce a passage that is
    // all explanation and no content.
    let found = retrieved("tiny", "Net", NodeKind::Column, &ExpandOptions::default());
    let rendered = render(
        &found,
        &RenderOptions {
            max_chars: 1,
            ..Default::default()
        },
    );

    assert_eq!(entries(&rendered.text).len(), 1);
    assert!(rendered.text.contains("Net"));
    assert_eq!(rendered.omitted, found.total_nodes() - 1);
}

#[test]
fn an_empty_expansion_renders_to_nothing_rather_than_a_confident_heading() {
    let rendered = render(&Retrieved::default(), &RenderOptions::default());
    assert!(rendered.text.is_empty());
    assert!(rendered.citations.is_empty());
    assert_eq!(rendered.omitted, 0);
}

#[test]
fn the_workbook_is_named_once_and_not_on_every_line() {
    let found = retrieved("header", "Net", NodeKind::Column, &ExpandOptions::default());
    let rendered = render(&found, &RenderOptions::default());

    let mentions = rendered.text.matches("chain.xlsx").count();
    // The heading, and the workbook root's own entry. Not once per node.
    assert!(
        mentions <= 2,
        "the workbook path appears {mentions} times:\n{}",
        rendered.text
    );
}
