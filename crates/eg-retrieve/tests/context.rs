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
use eg_retrieve::{
    expand, render, ExpandOptions, RenderOptions, Retrieved, RetrievedNode, Role, WorkbookContext,
};
use std::collections::BTreeMap;

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
fn a_cut_passage_drops_the_relations_that_point_into_what_was_cut() {
    // The failure this guards is a rendered entry reading `reads: [37]` when
    // the passage stops at [12]. An agent will cite [37].
    let found = retrieved(
        "dropped",
        "Net",
        NodeKind::Column,
        &ExpandOptions::default(),
    );
    let full = render(&found, &RenderOptions::default());
    assert!(
        full.text.contains("reads") || full.text.contains("read by"),
        "the fixture must have relations for this test to mean anything"
    );

    for ceiling in [200, 260, 320, 400, 500, 700] {
        let rendered = render(
            &found,
            &RenderOptions {
                max_chars: ceiling,
                ..Default::default()
            },
        );
        let highest = entries(&rendered.text).len();
        for n in referenced(&rendered.text) {
            assert!(
                n <= highest,
                "at a ceiling of {ceiling} the passage refers to [{n}] and \
                 lists only {highest} entries:\n{}",
                rendered.text
            );
        }
    }
}

#[test]
fn the_ceiling_holds_across_several_workbooks() {
    // The budget used to be applied only to entries, and only after the first
    // of each workbook — so ten workbooks meant ten headings, ten preambles and
    // ten unconditional entries, whatever the caller asked for.
    let one = retrieved("many", "Net", NodeKind::Column, &ExpandOptions::default());
    let mut many = Retrieved::default();
    for i in 0..10 {
        let mut copy = one.workbooks[0].clone();
        copy.content_hash = format!("hash-{i}");
        copy.path = format!("book{i}.xlsx");
        many.workbooks.push(copy);
    }

    let ceiling = 1_200;
    let rendered = render(
        &many,
        &RenderOptions {
            max_chars: ceiling,
            ..Default::default()
        },
    );

    // The trailing "left out to fit" notice is written after the ceiling on
    // purpose — a passage has to be able to say it was cut.
    assert!(
        body(&rendered.text).chars().count() <= ceiling,
        "asked for {ceiling} characters and got {}",
        body(&rendered.text).chars().count()
    );
    assert!(!rendered.text.is_empty());

    // `omitted > 0` was the old assertion and it passed on the first
    // workbook's cut alone, while nine workbooks vanished without a word. The
    // count has to account for every node that was retrieved and not printed.
    let retrieved: usize = many.total_nodes();
    let printed = entries(&rendered.text).len();
    assert_eq!(
        rendered.omitted,
        retrieved - printed,
        "{retrieved} retrieved, {printed} printed, {} reported omitted",
        rendered.omitted
    );
    assert!(
        rendered.omitted_workbooks > 0,
        "workbooks dropped whole were not counted"
    );
    assert!(
        rendered.text.contains("do not appear above at all"),
        "the passage never mentions the workbooks it dropped:\n{}",
        rendered.text
    );
}

/// The body of a passage: everything before the trailing "left out" footer,
/// which is written after the ceiling on purpose so a cut can announce itself.
///
/// Every character it leaves behind is slack in an assertion about the
/// ceiling, so it strips the footer exactly and nothing more.
fn body(text: &str) -> &str {
    let Some(at) = text.find(" further node(s) were retrieved") else {
        return text;
    };
    // The footer is exactly "\n{digits} further node(s)…", so strip the digits
    // and then the one newline before them. The previous version accepted
    // either at each step, so it walked back across the blank line closing the
    // passage and went on eating digits that were real content — the `3` of
    // `A1:B3`, and six more of an `A1:BM115004`. That is slack in every
    // assertion about the ceiling, which is what this helper exists to remove.
    let bytes = text.as_bytes();
    let mut start = at;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start > 0 && bytes[start - 1] == b'\n' {
        start -= 1;
    }
    &text[..start]
}

#[test]
fn the_ceiling_holds_at_every_size_and_not_just_the_convenient_ones() {
    // A single value passes on slack. The overshoot this guards against was
    // about thirty characters wide and only appeared where the cut heading —
    // which is longer than the uncut one — was not what got measured.
    let found = retrieved("sweep", "Net", NodeKind::Column, &ExpandOptions::default());

    for ceiling in (200..=3_000).step_by(1) {
        let rendered = render(
            &found,
            &RenderOptions {
                max_chars: ceiling,
                ..Default::default()
            },
        );
        let shown = entries(&rendered.text).len();
        // The first entry is documented as unconditional; everything past that
        // is inside the ceiling or the ceiling means nothing.
        if shown <= 1 {
            continue;
        }
        assert!(
            body(&rendered.text).chars().count() <= ceiling,
            "at a ceiling of {ceiling} the body ran to {} characters:\n{}",
            body(&rendered.text).chars().count(),
            rendered.text
        );
    }
}

/// `n` copies of the fixture's workbook, so tests can reach the path where
/// `already` is non-zero and the unconditional-first-entry escape is spent.
fn several(found: &Retrieved, n: usize) -> Retrieved {
    let mut many = Retrieved::default();
    for i in 0..n {
        let mut copy = found.workbooks[0].clone();
        copy.content_hash = format!("hash-{i}");
        copy.path = format!("book{i}.xlsx");
        many.workbooks.push(copy);
    }
    many
}

#[test]
fn a_passage_that_fits_is_not_announced_as_cut() {
    // Charging a workbook for the longer "cut to fit" heading meant a passage
    // with room to spare was trimmed anyway, and then said so.
    //
    // Three workbooks, not one: the second and third carry a non-zero running
    // total and have no unconditional first entry, so an over-charge there
    // drops a whole workbook rather than trimming an entry — a different branch
    // from the one a single-workbook fixture reaches.
    let found = retrieved("nocut", "Net", NodeKind::Column, &ExpandOptions::default());
    for count in [1, 3] {
        let many = several(&found, count);
        let whole = render(&many, &RenderOptions::default());
        let full_size = body(&whole.text).chars().count();
        assert_eq!(whole.omitted, 0, "the default ceiling already cut {count}");

        for ceiling in full_size..full_size + 40 {
            let rendered = render(
                &many,
                &RenderOptions {
                    max_chars: ceiling,
                    ..Default::default()
                },
            );
            assert_eq!(
                rendered.omitted, 0,
                "a {full_size}-character passage of {count} workbook(s) was cut \
                 at a ceiling of {ceiling}:\n{}",
                rendered.text
            );
            assert_eq!(rendered.omitted_workbooks, 0);
            assert!(!rendered.text.contains("the rest cut to fit"));
        }
    }
}

#[test]
fn a_later_workbook_is_cut_by_a_character_and_not_dropped_whole() {
    // The earlier test could not fail: with one workbook the first entry is
    // unconditional, so `fits >= 1` however badly the heading was measured, and
    // no heading arithmetic could ever drop it. A second workbook has no such
    // escape, so this is where over-charging shows up.
    let found = retrieved("later", "Net", NodeKind::Column, &ExpandOptions::default());
    let two = several(&found, 2);
    let whole = render(&two, &RenderOptions::default());
    let full = body(&whole.text).chars().count();

    // One character short of the whole thing: something must give, and it must
    // be one entry rather than an entire workbook.
    let rendered = render(
        &two,
        &RenderOptions {
            max_chars: full - 1,
            ..Default::default()
        },
    );
    assert_eq!(
        rendered.omitted_workbooks, 0,
        "a workbook was dropped whole to save one character:\n{}",
        rendered.text
    );
    assert!(rendered.omitted > 0);
    assert!(body(&rendered.text).chars().count() < full);
}

/// A workbook of `n` plain nodes, distinct and without ancestry.
///
/// Wide on purpose. The fixture used everywhere else has eight nodes, and the
/// heading's two forms are then the same length whatever the cut — "1 of 8"
/// against "7 of 8" — so a whole class of measurement error is invisible to it.
/// Above ten the digit counts diverge, which is where charging for a number
/// that will not be written starts to cost an entry.
fn wide_workbook(n: usize) -> Retrieved {
    let nodes: Vec<RetrievedNode> = (0..n)
        .map(|i| RetrievedNode {
            node: i as u32,
            kind: NodeKind::Column,
            label: format!("Column {i}"),
            a1: Some(format!("Sheet1!A{}:A{}", i + 1, i + 200)),
            sheet: Some("Sheet1".into()),
            parent: None,
            role: if i == 0 {
                Role::Seed
            } else {
                Role::Ancestor { of: 0 }
            },
            hops: 0,
            score: if i == 0 { Some(1.0) } else { None },
        })
        .collect();

    Retrieved {
        workbooks: vec![WorkbookContext {
            content_hash: "hash-wide".into(),
            path: "wide.xlsx".into(),
            nodes,
            truncated: false,
        }],
        missing_workbooks: Vec::new(),
    }
}

#[test]
fn the_passage_is_as_full_as_the_ceiling_allows() {
    // Not just "within the ceiling" — that passes for a renderer that shows one
    // entry and stops. This checks the other direction: whatever number of
    // entries fits, that is the number returned.
    //
    // The size of a k-entry passage is read off the sweep itself, so the test
    // needs no second implementation of the thing it is checking.
    let found = wide_workbook(121);
    let ceilings = 200..=4_000usize;

    let mut size_of: BTreeMap<usize, usize> = BTreeMap::new();
    let mut shown_at: BTreeMap<usize, usize> = BTreeMap::new();
    for ceiling in ceilings.clone() {
        let rendered = render(
            &found,
            &RenderOptions {
                max_chars: ceiling,
                ..Default::default()
            },
        );
        let k = entries(&rendered.text).len();
        let len = body(&rendered.text).chars().count();
        size_of.entry(k).or_insert(len);
        shown_at.insert(ceiling, k);
    }

    for ceiling in ceilings {
        let shown = shown_at[&ceiling];
        let best = size_of
            .iter()
            .filter(|(_, &len)| len <= ceiling)
            .map(|(&k, _)| k)
            .max()
            .unwrap_or(0)
            // The first entry of a passage is written whatever the ceiling.
            .max(1);
        assert_eq!(
            shown, best,
            "at a ceiling of {ceiling} the passage shows {shown} entries when \
             {best} fit in {} characters",
            size_of[&best]
        );
    }

    // The check above reads its answer out of the same sweep it is checking, so
    // a renderer that undershot by one everywhere would simply never record the
    // better size and would agree with itself. This one does not: how long a
    // k-entry passage is, is a fact about the output format, and it stays true
    // whichever k the renderer decides to return. So ask for exactly that many
    // characters and require exactly that many entries back.
    for (&k, &size) in &size_of {
        let rendered = render(
            &found,
            &RenderOptions {
                max_chars: size,
                ..Default::default()
            },
        );
        assert_eq!(
            entries(&rendered.text).len(),
            k,
            "a {k}-entry passage is {size} characters, and a ceiling of {size} \
             returned {} entries",
            entries(&rendered.text).len()
        );
    }
}

#[test]
fn the_heading_counts_what_is_shown_and_not_what_was_retrieved() {
    // "27 node(s)" above a list ending at [4] is a passage contradicting
    // itself, and the reader can only resolve it from a footer further down.
    let found = retrieved(
        "heading",
        "Net",
        NodeKind::Column,
        &ExpandOptions::default(),
    );
    let rendered = render(
        &found,
        &RenderOptions {
            max_chars: 420,
            ..Default::default()
        },
    );

    let shown = entries(&rendered.text).len();
    assert!(shown > 0 && shown < found.total_nodes(), "shown {shown}");
    assert!(
        rendered
            .text
            .contains(&format!("{shown} of {} node(s)", found.total_nodes())),
        "the heading does not say {shown} of {}:\n{}",
        found.total_nodes(),
        rendered.text
    );
}

#[test]
fn a_pile_of_stale_hashes_does_not_become_the_whole_passage() {
    // One line for all of them, and inside the ceiling. Twenty notices used to
    // be 2.2 KB written before the budget was consulted, which then left no
    // room for any real workbook at all.
    let found = retrieved("stale", "Net", NodeKind::Column, &ExpandOptions::default());
    let mut many = found.clone();
    for i in 0..20 {
        many.missing_workbooks.push(format!("{i:040x}"));
    }

    let rendered = render(
        &many,
        &RenderOptions {
            max_chars: 1_500,
            ..Default::default()
        },
    );
    assert!(
        body(&rendered.text).chars().count() <= 1_500,
        "asked for 1500 characters and got {}",
        body(&rendered.text).chars().count()
    );
    // Counted per workbook, which is what the field holds — "result(s)" made
    // three hits into one evicted workbook read as one loss.
    assert!(rendered
        .text
        .contains("20 workbook(s) matched by the search"));
    assert!(rendered.text.contains("and 12 more"));
    // And the real workbook still got room.
    assert!(!entries(&rendered.text).is_empty(), "{}", rendered.text);
}

#[test]
fn the_ceiling_counts_characters_and_not_bytes() {
    let one = retrieved("utf8", "Net", NodeKind::Column, &ExpandOptions::default());
    let mut wide = Retrieved::default();
    let mut copy = one.workbooks[0].clone();
    // A sheet name in a script where a character is three bytes.
    copy.path = "決算書_2024.xlsx".into();
    wide.workbooks.push(copy);

    let rendered = render(
        &wide,
        &RenderOptions {
            max_chars: 400,
            ..Default::default()
        },
    );
    assert!(body(&rendered.text).chars().count() <= 400 || entries(&rendered.text).len() == 1);
    assert!(rendered.text.contains("決算書"));
}

#[test]
fn the_fact_that_workbooks_are_missing_outlives_a_tiny_ceiling() {
    // The names can go; the fact cannot. A caller not told that data is absent
    // presents what is left as everything there was.
    let found = retrieved(
        "tinynotice",
        "Net",
        NodeKind::Column,
        &ExpandOptions::default(),
    );
    let mut stale = found.clone();
    for i in 0..20 {
        stale.missing_workbooks.push(format!("{i:040x}"));
    }

    let rendered = render(
        &stale,
        &RenderOptions {
            max_chars: 120,
            ..Default::default()
        },
    );
    assert!(
        rendered.text.contains("20 workbook(s)"),
        "the notice vanished under a small ceiling:\n{}",
        rendered.text
    );
    assert!(
        !rendered.text.contains("and 12 more"),
        "the long form was written anyway:\n{}",
        rendered.text
    );
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
fn a_header_shaped_like_a_cell_reference_keeps_its_name() {
    // `FY2024` is column FY row 2024, so no test on the string can tell a
    // header from a range. Blanking it left the node with no name at all.
    let found = retrieved("names", "Net", NodeKind::Column, &ExpandOptions::default());
    let mut renamed = found.clone();
    renamed.workbooks[0].nodes[0].label = "FY2024".into();
    renamed.workbooks[0].nodes[0].a1 = Some("Sales!C2:C99".into());

    let rendered = render(&renamed, &RenderOptions::default());
    assert!(
        rendered.text.contains("FY2024"),
        "the header was blanked:\n{}",
        rendered.text
    );
}

#[test]
fn a_label_that_is_just_its_own_range_is_not_printed_twice() {
    let found = retrieved("dupe", "Net", NodeKind::Column, &ExpandOptions::default());
    let mut same = found.clone();
    same.workbooks[0].nodes[0].label = "C2:C99".into();
    same.workbooks[0].nodes[0].a1 = Some("Sales!C2:C99".into());

    let rendered = render(&same, &RenderOptions::default());
    let first = rendered.text.lines().find(|l| l.starts_with('[')).unwrap();
    assert_eq!(
        first.matches("C2:C99").count(),
        1,
        "the range is printed as both label and citation: {first}"
    );
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
