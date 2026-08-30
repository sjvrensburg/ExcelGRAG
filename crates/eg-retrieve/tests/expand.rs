//! Expansion against workbooks small enough to know the right answer for.
//!
//! The reference workbook says whether the walk stays bounded. These say
//! whether what it brings back is the context a reader asked for, and whether
//! every node can say why it is there.

use eg_graph::store::Corpus;
use eg_graph::{build, EdgeKind, NodeKind};
use eg_index::Hit;
use eg_model::{Cell, CellValue, Sheet, SheetId, Workbook, WorkbookFormat};
use eg_retrieve::{expand, ExpandOptions, Retrieved, RetrievedNode, Role};

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

/// Three sheets in a chain: Report reads Sales, Sales reads Rates. Two
/// dependency hops from the report is the whole model.
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

/// One table with many columns, which is the shape every hub in the reference
/// workbook has.
fn wide() -> Workbook {
    let header: Vec<String> = (0..40).map(|i| format!("Col{i}")).collect();
    let body: Vec<String> = (0..40).map(|i| format!("{i}")).collect();
    let rows = [header.join(" "), body.join(" "), body.join(" ")];
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    Workbook {
        path: "wide.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-wide".into(),
        sheets: vec![grid(0, "Wide", &borrowed)],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

fn dir(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "eg-retrieve-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// Store a workbook's graph and hand back a corpus over it.
fn corpus_of(tag: &str, workbooks: &[Workbook]) -> (std::path::PathBuf, Corpus) {
    let root = dir(tag);
    let mut corpus = Corpus::open(&root).unwrap();
    for wb in workbooks {
        let built = build(wb);
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
    }
    (root, corpus)
}

/// A hit standing for the node whose label matches, so the tests name nodes the
/// way a reader would rather than by index.
fn hit_for(corpus: &Corpus, hash: &str, label: &str, kind: NodeKind) -> Hit {
    let stored = corpus.get(hash).unwrap().unwrap();
    let (index, node) = stored
        .graph
        .node_indices()
        .map(|i| (i, &stored.graph[i]))
        .find(|(_, n)| n.kind() == kind && n.label() == label)
        .unwrap_or_else(|| panic!("no {kind:?} labelled {label}"));
    Hit {
        score: 1.0,
        workbook: hash.to_string(),
        path: stored.path.clone(),
        node: index.index() as u32,
        kind: node.kind(),
        sheet: None,
        label: node.label(),
        a1: None,
    }
}

fn labels(found: &Retrieved) -> Vec<&str> {
    found.workbooks[0]
        .nodes
        .iter()
        .map(|n| n.label.as_str())
        .collect()
}

fn find<'a>(found: &'a Retrieved, label: &str) -> &'a RetrievedNode {
    found.workbooks[0]
        .nodes
        .iter()
        .find(|n| n.label == label)
        .unwrap_or_else(|| panic!("{label} is not in {:?}", labels(found)))
}

#[test]
fn a_seed_arrives_with_the_table_and_sheet_that_name_it() {
    let (_root, corpus) = corpus_of("ancestry", &[chain()]);
    let seed = hit_for(&corpus, "hash-chain", "Net", NodeKind::Column);
    let found = expand(&corpus, &[seed], &ExpandOptions::default()).unwrap();

    let net = find(&found, "Net");
    assert_eq!(net.role, Role::Seed);
    assert_eq!(net.sheet.as_deref(), Some("Sales"));

    // Up the containment tree: region, sheet, workbook. Every one of them
    // names the node that it contains.
    let sheet = find(&found, "Sales");
    assert_eq!(sheet.kind, NodeKind::Sheet);
    assert!(matches!(sheet.role, Role::Ancestor { .. }));
    assert!(found.workbooks[0]
        .nodes
        .iter()
        .any(|n| n.kind == NodeKind::Workbook));
}

#[test]
fn ancestry_does_not_count_as_a_hop() {
    // Otherwise a column would spend its whole hop budget being named.
    let (_root, corpus) = corpus_of("hops", &[chain()]);
    let seed = hit_for(&corpus, "hash-chain", "Net", NodeKind::Column);
    let found = expand(
        &corpus,
        &[seed],
        &ExpandOptions {
            hops: 0,
            ..Default::default()
        },
    )
    .unwrap();

    assert!(found.workbooks[0].nodes.iter().all(|n| n.hops == 0));
    assert!(found.workbooks[0]
        .nodes
        .iter()
        .any(|n| n.kind == NodeKind::Sheet));
}

#[test]
fn one_hop_reaches_what_a_column_reads_and_two_reaches_what_that_reads() {
    let (_root, corpus) = corpus_of("chain", &[chain()]);
    let seed = hit_for(&corpus, "hash-chain", "Total", NodeKind::Column);

    let one = expand(
        &corpus,
        std::slice::from_ref(&seed),
        &ExpandOptions {
            hops: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        one.workbooks[0]
            .nodes
            .iter()
            .any(|n| n.sheet.as_deref() == Some("Sales")),
        "one hop should reach Sales: {:?}",
        labels(&one)
    );
    assert!(
        !one.workbooks[0]
            .nodes
            .iter()
            .any(|n| n.sheet.as_deref() == Some("Rates")),
        "one hop should not reach Rates: {:?}",
        labels(&one)
    );

    let two = expand(
        &corpus,
        std::slice::from_ref(&seed),
        &ExpandOptions {
            hops: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        two.workbooks[0]
            .nodes
            .iter()
            .any(|n| n.sheet.as_deref() == Some("Rates")),
        "two hops should reach Rates: {:?}",
        labels(&two)
    );
}

#[test]
fn an_input_and_a_dependent_are_told_apart() {
    let (_root, corpus) = corpus_of("direction", &[chain()]);
    let seed = hit_for(&corpus, "hash-chain", "Sales", NodeKind::Sheet);
    let found = expand(&corpus, &[seed], &ExpandOptions::default()).unwrap();

    // Sales reads Rates, and Report reads Sales. Getting these the wrong way
    // round would invert every provenance answer the layer above gives.
    let inputs: Vec<&str> = found.workbooks[0]
        .nodes
        .iter()
        .filter(|n| matches!(n.role, Role::Input { .. }))
        .map(|n| n.sheet.as_deref().unwrap_or_default())
        .collect();
    let dependents: Vec<&str> = found.workbooks[0]
        .nodes
        .iter()
        .filter(|n| matches!(n.role, Role::Dependent { .. }))
        .map(|n| n.sheet.as_deref().unwrap_or_default())
        .collect();

    assert!(inputs.contains(&"Rates"), "inputs were {inputs:?}");
    assert!(
        dependents.contains(&"Report"),
        "dependents were {dependents:?}"
    );
    assert!(!inputs.contains(&"Report"));
    assert!(!dependents.contains(&"Rates"));
}

#[test]
fn a_wide_table_does_not_flood_the_result() {
    // The failure this guards against is the one the reference workbook's
    // degree distribution predicts: a column, its region, and then all 40 of
    // that region's other columns.
    let (_root, corpus) = corpus_of("wide", &[wide()]);
    let seed = hit_for(&corpus, "hash-wide", "Col7", NodeKind::Column);
    let found = expand(&corpus, &[seed], &ExpandOptions::default()).unwrap();

    let columns = found.workbooks[0]
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Column)
        .count();
    assert_eq!(columns, 1, "pulled in {:?}", labels(&found));
    assert!(!found.truncated());
}

#[test]
fn children_are_opt_in_and_capped() {
    let (_root, corpus) = corpus_of("children", &[wide()]);
    let seed = hit_for(&corpus, "hash-wide", "Col7", NodeKind::Column);
    let found = expand(
        &corpus,
        &[seed],
        &ExpandOptions {
            children: 5,
            ..Default::default()
        },
    )
    .unwrap();

    let children = found.workbooks[0]
        .nodes
        .iter()
        .filter(|n| matches!(n.role, Role::Child { .. }))
        .count();
    assert_eq!(children, 5, "got {:?}", labels(&found));
}

#[test]
fn a_sheet_that_only_names_a_seed_does_not_bring_its_other_tables() {
    // Seed a region of a sheet that has several. The sheet comes along to name
    // it; its other tables are siblings of the answer, not context for it.
    let wb = Workbook {
        path: "many.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-many".into(),
        sheets: vec![
            grid(
                0,
                "Hub",
                &[
                    "A B",
                    "x =Far!B2",
                    ". .",
                    "C D",
                    "y =Far!B3",
                    ". .",
                    "E F",
                    "z =Far!B4",
                ],
            ),
            grid(1, "Far", &["K V", "p 1", "q 2", "r 3"]),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let (_root, corpus) = corpus_of("siblings", &[wb]);

    let stored = corpus.get("hash-many").unwrap().unwrap();
    let regions_on_hub = stored
        .graph
        .node_weights()
        .filter(|n| n.kind() == NodeKind::Region)
        .count();
    assert!(regions_on_hub >= 3, "fixture needs several regions");

    // Seed the first region directly.
    let (index, node) = stored
        .graph
        .node_indices()
        .map(|i| (i, &stored.graph[i]))
        .find(|(_, n)| n.kind() == NodeKind::Region)
        .unwrap();
    let seed = Hit {
        score: 1.0,
        workbook: "hash-many".into(),
        path: stored.path.clone(),
        node: index.index() as u32,
        kind: node.kind(),
        sheet: None,
        label: node.label(),
        a1: None,
    };

    let found = expand(&corpus, &[seed], &ExpandOptions::default()).unwrap();
    let siblings = found.workbooks[0]
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Region && matches!(n.role, Role::Child { .. }))
        .count();
    assert_eq!(
        siblings,
        0,
        "the sheet's other tables came along: {:?}",
        labels(&found)
    );
}

#[test]
fn the_workbook_root_is_never_enumerated() {
    // A column seed showing its table's other columns is the useful case, and
    // the test above covers it. This is the one that is not: four of a file's
    // sheets, picked in build order, tell a reader nothing.
    let (_root, corpus) = corpus_of("childscope", &[chain()]);
    let seed = hit_for(&corpus, "hash-chain", "Net", NodeKind::Column);
    let found = expand(
        &corpus,
        &[seed],
        &ExpandOptions {
            children: 3,
            hops: 0,
            ..Default::default()
        },
    )
    .unwrap();

    let sheets_pulled_in = found.workbooks[0]
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Sheet && matches!(n.role, Role::Child { .. }))
        .count();
    assert_eq!(
        sheets_pulled_in,
        0,
        "the workbook root's sheets came along: {:?}",
        labels(&found)
    );
}

#[test]
fn the_budget_caps_the_result_and_says_that_it_did() {
    let (_root, corpus) = corpus_of("budget", &[wide()]);
    let seed = hit_for(&corpus, "hash-wide", "Col7", NodeKind::Column);
    let found = expand(
        &corpus,
        &[seed],
        &ExpandOptions {
            children: 100,
            budget: 6,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(found.total_nodes(), 6);
    assert!(
        found.truncated(),
        "a capped expansion that does not say so reads as a complete one"
    );
}

#[test]
fn a_node_reached_from_two_children_still_names_the_path_to_each() {
    // `role` records whichever child arrived first, so a path rebuilt from it
    // is right for that one and truncated for the other. `parent` is the
    // structural fact, and `ancestry` uses it.
    let (_root, corpus) = corpus_of("paths", &[chain()]);
    let a = hit_for(&corpus, "hash-chain", "Net", NodeKind::Column);
    let b = hit_for(&corpus, "hash-chain", "Tariff", NodeKind::Column);
    let found = expand(&corpus, &[a, b], &ExpandOptions::default()).unwrap();
    let workbook = &found.workbooks[0];

    for label in ["Net", "Tariff"] {
        let node = workbook.nodes.iter().find(|n| n.label == label).unwrap();
        let path: Vec<&str> = workbook
            .ancestry(node.node)
            .iter()
            .map(|n| n.label.as_str())
            .collect();
        // workbook, sheet, region — all three, for both columns.
        assert_eq!(path.len(), 3, "{label} came back with {path:?}");
        assert_eq!(path[0], "chain.xlsx");
    }
}

#[test]
fn ancestry_stops_at_what_the_budget_allowed_rather_than_inventing_it() {
    let (_root, corpus) = corpus_of("shortpath", &[chain()]);
    let seed = hit_for(&corpus, "hash-chain", "Net", NodeKind::Column);
    let found = expand(
        &corpus,
        &[seed],
        &ExpandOptions {
            budget: 2,
            ..Default::default()
        },
    )
    .unwrap();
    let workbook = &found.workbooks[0];

    let net = workbook.nodes.iter().find(|n| n.label == "Net").unwrap();
    assert!(workbook.ancestry(net.node).len() < 3);
    assert!(found.truncated());
}

#[test]
fn every_node_but_a_seed_names_what_reached_it() {
    let (_root, corpus) = corpus_of("provenance", &[chain()]);
    let seed = hit_for(&corpus, "hash-chain", "Net", NodeKind::Column);
    let found = expand(&corpus, &[seed], &ExpandOptions::default()).unwrap();

    let present: Vec<u32> = found.workbooks[0].nodes.iter().map(|n| n.node).collect();
    for node in &found.workbooks[0].nodes {
        match node.role.from() {
            None => assert_eq!(node.role, Role::Seed, "{} has no origin", node.label),
            Some(from) => assert!(
                present.contains(&from),
                "{} was reached from a node that is not in the result",
                node.label
            ),
        }
    }
}

#[test]
fn heavier_edges_are_followed_first() {
    // Two sheets read by one column, one of them from far more cells. Under a
    // budget that fits only one, the heavy edge is the one worth spending it on.
    let wb = Workbook {
        path: "weights.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-weights".into(),
        sheets: vec![
            grid(
                0,
                "Main",
                &[
                    "Row Value",
                    "a =Heavy!B2",
                    "b =Heavy!B3",
                    "c =Heavy!B4",
                    "d =Light!B2",
                ],
            ),
            grid(1, "Heavy", &["K V", "x 1", "y 2", "z 3"]),
            grid(2, "Light", &["K V", "x 1"]),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let (_root, corpus) = corpus_of("weights", &[wb]);
    let seed = hit_for(&corpus, "hash-weights", "Main", NodeKind::Sheet);

    let found = expand(
        &corpus,
        &[seed],
        &ExpandOptions {
            hops: 1,
            budget: 4,
            ..Default::default()
        },
    )
    .unwrap();

    let sheets: Vec<&str> = found.workbooks[0]
        .nodes
        .iter()
        .filter_map(|n| n.sheet.as_deref())
        .collect();
    assert!(sheets.contains(&"Heavy"), "reached {sheets:?}");
}

#[test]
fn a_heavy_detour_does_not_hide_what_is_closer() {
    // Main reads Heavy from three cells and Light from one, and Heavy also
    // reads Light. Weight-first takes Light through Heavy, records it at two
    // hops, and then never queues its edges — so Deep, which is genuinely two
    // hops from Main, is lost.
    let wb = Workbook {
        path: "detour.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "hash-detour".into(),
        sheets: vec![
            grid(
                0,
                "Main",
                &[
                    "Row Value",
                    "a =Heavy!B2",
                    "b =Heavy!B3",
                    "c =Heavy!B4",
                    "d =Light!B2",
                ],
            ),
            grid(1, "Heavy", &["K V", "x =Light!B3", "y 2", "z 3"]),
            grid(2, "Light", &["K V", "p =Deep!B2", "q =Deep!B3"]),
            grid(3, "Deep", &["K V", "m 1", "n 2"]),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    };
    let (_root, corpus) = corpus_of("detour", &[wb]);
    let seed = hit_for(&corpus, "hash-detour", "Main", NodeKind::Sheet);

    let found = expand(
        &corpus,
        &[seed],
        &ExpandOptions {
            hops: 2,
            ..Default::default()
        },
    )
    .unwrap();

    let sheets: Vec<&str> = found.workbooks[0]
        .nodes
        .iter()
        .filter_map(|n| n.sheet.as_deref())
        .collect();
    assert!(sheets.contains(&"Deep"), "reached only {sheets:?}");

    // And Light is recorded at the distance it actually is from the seed.
    let light = found.workbooks[0]
        .nodes
        .iter()
        .filter(|n| n.sheet.as_deref() == Some("Light"))
        .map(|n| n.hops)
        .min()
        .expect("Light was not reached");
    assert_eq!(light, 1, "Light is one hop from Main, recorded as {light}");
}

#[test]
fn a_seed_whose_workbook_is_gone_is_reported_rather_than_dropped() {
    let (_root, corpus) = corpus_of("missing", &[chain()]);
    let mut seed = hit_for(&corpus, "hash-chain", "Net", NodeKind::Column);
    seed.workbook = "hash-that-is-not-there".into();

    let found = expand(&corpus, &[seed], &ExpandOptions::default()).unwrap();
    assert!(found.workbooks.is_empty());
    assert_eq!(found.missing_workbooks, vec!["hash-that-is-not-there"]);
}

#[test]
fn a_hit_pointing_past_the_end_of_the_graph_is_skipped_not_panicked_on() {
    let (_root, corpus) = corpus_of("stale", &[chain()]);
    let mut seed = hit_for(&corpus, "hash-chain", "Net", NodeKind::Column);
    seed.node = 100_000;

    let found = expand(&corpus, &[seed], &ExpandOptions::default()).unwrap();
    assert_eq!(found.total_nodes(), 0);
}

#[test]
fn two_hits_in_one_workbook_read_the_graph_once_and_share_their_context() {
    let (_root, corpus) = corpus_of("shared", &[chain()]);
    let a = hit_for(&corpus, "hash-chain", "Net", NodeKind::Column);
    let b = hit_for(&corpus, "hash-chain", "Tariff", NodeKind::Column);
    let found = expand(&corpus, &[a, b], &ExpandOptions::default()).unwrap();

    assert_eq!(found.workbooks.len(), 1);
    assert_eq!(found.workbooks[0].seeds().count(), 2);

    // One workbook node, not two: a node reached twice is one node with one
    // reason, not a duplicate for each path that found it.
    let roots = found.workbooks[0]
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Workbook)
        .count();
    assert_eq!(roots, 1);
}

#[test]
fn min_weight_drops_the_lightest_dependencies() {
    let (_root, corpus) = corpus_of("minweight", &[chain()]);
    let seed = hit_for(&corpus, "hash-chain", "Sales", NodeKind::Sheet);
    let found = expand(
        &corpus,
        &[seed],
        &ExpandOptions {
            min_weight: u64::MAX,
            ..Default::default()
        },
    )
    .unwrap();

    assert!(
        found.workbooks[0]
            .nodes
            .iter()
            .all(|n| !matches!(n.role, Role::Input { .. } | Role::Dependent { .. })),
        "a dependency survived an impossible weight floor: {:?}",
        labels(&found)
    );
}

#[test]
fn the_edge_kind_that_reached_a_node_is_recorded() {
    let (_root, corpus) = corpus_of("edgekind", &[chain()]);
    let seed = hit_for(&corpus, "hash-chain", "Sales", NodeKind::Sheet);
    let found = expand(&corpus, &[seed], &ExpandOptions::default()).unwrap();

    let crossed = found.workbooks[0].nodes.iter().any(|n| {
        matches!(
            n.role,
            Role::Input {
                kind: EdgeKind::CrossSheetRef,
                ..
            } | Role::Dependent {
                kind: EdgeKind::CrossSheetRef,
                ..
            }
        )
    });
    assert!(
        crossed,
        "no cross-sheet edge recorded: {:?}",
        labels(&found)
    );
}
