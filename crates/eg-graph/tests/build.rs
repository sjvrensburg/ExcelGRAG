//! Graph building against workbooks small enough to reason about by hand.
//!
//! The reference workbook says whether the build scales; these say whether it
//! is right. Both are needed, and neither substitutes for the other.

use eg_graph::{build, build_with, check, EdgeKind, GraphOptions, Node, NodeKind};
use eg_model::{Cell, CellValue, DefinedName, Sheet, SheetId, Workbook, WorkbookFormat};

fn grid(id: u16, name: &str, rows: &[&str]) -> Sheet {
    let mut sheet = Sheet::new(SheetId(id), name);
    for (r, line) in rows.iter().enumerate() {
        for (c, tok) in line.split_whitespace().enumerate() {
            if tok == "." {
                continue;
            }
            let cell = match tok.strip_prefix('=') {
                Some(formula) => Cell {
                    value: CellValue::Number(0.0),
                    formula: Some(formula.to_string()),
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

fn workbook(sheets: Vec<Sheet>) -> Workbook {
    Workbook {
        path: "test.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "0".into(),
        sheets,
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

#[test]
fn the_structural_layers_are_wired_root_to_group() {
    let wb = workbook(vec![grid(
        0,
        "Sales",
        &["Region Q1 Q2", "North 10 =A2*2", "South 20 =A3*2"],
    )]);
    let built = build(&wb);
    assert_eq!(check(&built), vec![]);

    let r = &built.report;
    assert_eq!(r.nodes_of(NodeKind::Workbook), 1);
    assert_eq!(r.nodes_of(NodeKind::Sheet), 1);
    assert_eq!(r.nodes_of(NodeKind::Region), 1);
    // Two, not three: column A holds the row labels, so it heads nothing and
    // region detection excludes it from the headers.
    assert_eq!(r.nodes_of(NodeKind::Column), 2);
    // `=A2*2` and `=A3*2` share one R1C1 shape, so they are one group.
    assert_eq!(r.nodes_of(NodeKind::FormulaGroup), 1);

    // The group sits under the region and under the column heading it.
    assert_eq!(r.edges_of(EdgeKind::HeaderOf), 1);
}

#[test]
fn identical_references_merge_into_one_weighted_edge() {
    // Every row of the right-hand column reads the lookup table on Rates. One
    // edge should stand for all three, carrying the count.
    let wb = workbook(vec![
        grid(
            0,
            "Sales",
            &[
                "Region Net",
                "North =Rates!A2*2",
                "South =Rates!A3*2",
                "East =Rates!A4*2",
            ],
        ),
        grid(1, "Rates", &["Rate", "1", "2", "3"]),
    ]);
    let built = build(&wb);
    assert_eq!(check(&built), vec![]);

    let r = &built.report;
    assert_eq!(r.references_scanned, 3);
    assert_eq!(r.references_cross_sheet, 3);
    assert_eq!(
        r.edges_of(EdgeKind::CrossSheetRef),
        1,
        "three references to one region are one edge"
    );
    assert_eq!(r.edge_weight_of(EdgeKind::CrossSheetRef), 3);
    assert_eq!(r.edges_of(EdgeKind::DependsOn), 0);
}

#[test]
fn a_reference_within_the_same_region_makes_no_edge() {
    // The overwhelming majority of references in a real workbook look like
    // this. A self-loop on every region would be pure noise.
    let wb = workbook(vec![grid(
        0,
        "Sales",
        &["Amount Doubled", "10 =A2*2", "20 =A3*2"],
    )]);
    let built = build(&wb);
    assert_eq!(check(&built), vec![]);
    assert_eq!(built.report.references_scanned, 2);
    assert_eq!(built.report.references_within_source_region, 2);
    assert_eq!(built.report.edges_of(EdgeKind::DependsOn), 0);
}

#[test]
fn a_reference_to_a_missing_sheet_is_counted_not_dropped() {
    let wb = workbook(vec![grid(0, "Sales", &["Net", "=Gone!A1"])]);
    let built = build(&wb);
    assert_eq!(check(&built), vec![]);

    let r = &built.report;
    assert_eq!(r.references_dangling, 1);
    assert_eq!(r.dangling_examples.len(), 1);
    assert_eq!(r.dangling_examples[0].text, "Gone!A1");
}

#[test]
fn an_external_reference_becomes_a_node_for_the_workbook_we_cannot_open() {
    let wb = workbook(vec![grid(0, "Sales", &["Net", "=[1]Book!A1*2"])]);
    let built = build(&wb);
    assert_eq!(check(&built), vec![]);

    assert_eq!(built.report.references_external, 1);
    assert_eq!(built.report.nodes_of(NodeKind::ExternalWorkbook), 1);
    assert_eq!(built.report.edges_of(EdgeKind::CrossWorkbookRef), 1);
}

#[test]
fn a_defined_name_used_by_a_formula_is_linked_to_it() {
    let mut wb = workbook(vec![grid(0, "Sales", &["Net", "=Tax_Rate*2"])]);
    wb.defined_names.push(DefinedName {
        name: "Tax_Rate".into(),
        refers_to: "Sales!$Z$1".into(),
        scope: None,
    });
    let built = build(&wb);
    assert_eq!(check(&built), vec![]);

    assert_eq!(built.report.names_resolved, 1);
    assert_eq!(built.report.edges_of(EdgeKind::ReferencesName), 1);
    // `SUM` and friends are not defined names; only a real definition counts.
    assert_eq!(built.report.names_not_defined, 0);
}

#[test]
fn a_function_name_is_not_mistaken_for_a_defined_name() {
    let mut wb = workbook(vec![grid(0, "Sales", &["Net", "=SUM(A1:A1)+Tax_Rate"])]);
    wb.defined_names.push(DefinedName {
        name: "Tax_Rate".into(),
        refers_to: "Sales!$Z$1".into(),
        scope: None,
    });
    let built = build(&wb);
    assert_eq!(built.report.names_resolved, 1);
    assert_eq!(built.report.edges_of(EdgeKind::ReferencesName), 1);
}

#[test]
fn every_reference_is_accounted_for_exactly_once() {
    // The buckets partition the references, so a lifting change that loses one
    // shows up as arithmetic rather than as a silently smaller graph. This is
    // also asserted by `check`, and stated here because it is the property the
    // report's totals rest on.
    let wb = workbook(vec![
        grid(
            0,
            "Sales",
            &[
                "Region Net",
                "North =Rates!A2+A2",
                "South =Gone!A1",
                "East =[1]Book!A1",
                "West =ZZ9000",
            ],
        ),
        grid(1, "Rates", &["Rate", "1", "2"]),
    ]);
    let built = build(&wb);
    let r = &built.report;
    let accounted = r.references_lifted
        + r.references_within_source_region
        + r.references_external
        + r.references_dangling
        + r.references_unpopulated_target;
    assert_eq!(accounted, r.references_scanned, "{r:#?}");
    assert!(r.references_unpopulated_target >= 1, "ZZ9000 is empty");
}

#[test]
fn dropping_group_nodes_leaves_the_dependencies_untouched() {
    // Lifting reads formula cells, not group nodes, so the two builds must
    // agree on every dependency edge and differ only in the group layer.
    let wb = workbook(vec![
        grid(
            0,
            "Sales",
            &["Region Net", "North =Rates!A2", "South =Rates!A3"],
        ),
        grid(1, "Rates", &["Rate", "1", "2"]),
    ]);
    let with = build(&wb);
    let without = build_with(
        &wb,
        &GraphOptions {
            formula_group_nodes: false,
            ..Default::default()
        },
    );
    assert_eq!(check(&without), vec![]);
    assert_eq!(without.report.nodes_of(NodeKind::FormulaGroup), 0);
    assert_eq!(
        without.report.edges_of(EdgeKind::CrossSheetRef),
        with.report.edges_of(EdgeKind::CrossSheetRef)
    );
    assert_eq!(
        without.report.edge_weight_of(EdgeKind::CrossSheetRef),
        with.report.edge_weight_of(EdgeKind::CrossSheetRef)
    );
}

#[test]
fn a_range_spanning_two_regions_depends_on_both() {
    // `SUM(A1:A4)` crosses the blank row that separates the two blocks. Both
    // are real precedents, and attributing to only one would drop half the
    // provenance of the total.
    let wb = workbook(vec![
        grid(0, "Sales", &["Total", "=Data!A1:A4"]),
        grid(1, "Data", &["1 x", "2 y", ". .", "3 z", "4 w"]),
    ]);
    let built = build(&wb);
    assert_eq!(check(&built), vec![]);
    assert_eq!(built.report.nodes_of(NodeKind::Region), 3);
    assert_eq!(
        built.report.edges_of(EdgeKind::CrossSheetRef),
        2,
        "one edge per region the range touches"
    );
}

#[test]
fn the_graph_is_the_same_on_every_build() {
    // Edges are accumulated in a hash map, whose order is not stable. Sorting
    // before insertion is what makes two runs comparable when a number moves.
    let wb = workbook(vec![
        grid(
            0,
            "Sales",
            &["Region Net", "North =Rates!A2", "South =Rates!A3"],
        ),
        grid(1, "Rates", &["Rate", "1", "2"]),
    ]);
    let a = build(&wb);
    let b = build(&wb);
    let labels = |g: &eg_graph::BuiltGraph| -> Vec<(String, String, EdgeKind, u64)> {
        g.graph
            .edge_indices()
            .map(|e| {
                let (x, y) = g.graph.edge_endpoints(e).unwrap();
                let w = g.graph[e];
                (g.graph[x].label(), g.graph[y].label(), w.kind, w.weight)
            })
            .collect()
    };
    assert_eq!(labels(&a), labels(&b));
}

#[test]
fn every_node_carries_the_cells_it_stands_for() {
    // The graph holds no cell values, so a node that cannot name its range
    // cannot be turned back into a citation, and an answer resting on it could
    // not be checked against the workbook.
    let wb = workbook(vec![grid(
        0,
        "Sales",
        &["Region Q1", "North =A2*2", "South =A3*2"],
    )]);
    let built = build(&wb);
    for node in built.graph.node_weights() {
        match node {
            Node::Region(_) | Node::Column(_) | Node::FormulaGroup(_) => {
                assert!(node.range().is_some(), "{node:?}");
                assert!(node.sheet().is_some(), "{node:?}");
            }
            _ => {}
        }
    }
}

#[test]
fn a_sheet_qualified_name_resolves_in_the_sheet_it_names() {
    // `Rates!Tax_Rate` names the definition scoped to Rates, wherever the
    // formula lives. Resolved against the formula's own sheet instead, it
    // silently points at a different definition with the same name — a wrong
    // answer that every structural invariant is blind to, because the edge it
    // produces is perfectly well formed.
    let mut wb = workbook(vec![
        grid(0, "Sales", &["Net", "=Rates!Tax_Rate*2"]),
        grid(1, "Rates", &["Rate", "1"]),
    ]);
    wb.defined_names.push(DefinedName {
        name: "Tax_Rate".into(),
        refers_to: "Sales!$Z$1".into(),
        scope: Some(SheetId(0)),
    });
    wb.defined_names.push(DefinedName {
        name: "Tax_Rate".into(),
        refers_to: "Rates!$Z$9".into(),
        scope: Some(SheetId(1)),
    });

    let built = build(&wb);
    assert_eq!(check(&built), vec![]);
    assert_eq!(built.report.names_resolved, 1);

    let target = built
        .graph
        .edge_indices()
        .find(|&e| built.graph[e].kind == EdgeKind::ReferencesName)
        .map(|e| built.graph.edge_endpoints(e).unwrap().1)
        .expect("one REFERENCES_NAME edge");
    match &built.graph[target] {
        Node::DefinedName(n) => assert_eq!(n.refers_to, "Rates!$Z$9", "{n:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unqualified_name_still_prefers_its_own_sheet_then_the_workbook() {
    let mut wb = workbook(vec![
        grid(0, "Sales", &["Net", "=Tax_Rate*2"]),
        grid(1, "Rates", &["Rate", "1"]),
    ]);
    wb.defined_names.push(DefinedName {
        name: "Tax_Rate".into(),
        refers_to: "Any!$A$1".into(),
        scope: None,
    });
    wb.defined_names.push(DefinedName {
        name: "Tax_Rate".into(),
        refers_to: "Sales!$Z$1".into(),
        scope: Some(SheetId(0)),
    });
    let built = build(&wb);
    let target = built
        .graph
        .edge_indices()
        .find(|&e| built.graph[e].kind == EdgeKind::ReferencesName)
        .map(|e| built.graph.edge_endpoints(e).unwrap().1)
        .expect("one REFERENCES_NAME edge");
    match &built.graph[target] {
        Node::DefinedName(n) => assert_eq!(n.refers_to, "Sales!$Z$1", "the sheet-scoped one wins"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_name_qualified_by_a_missing_sheet_defines_nothing() {
    let mut wb = workbook(vec![grid(0, "Sales", &["Net", "=Gone!Tax_Rate"])]);
    wb.defined_names.push(DefinedName {
        name: "Tax_Rate".into(),
        refers_to: "Sales!$Z$1".into(),
        scope: None,
    });
    let built = build(&wb);
    assert_eq!(built.report.names_resolved, 0);
    assert_eq!(built.report.edges_of(EdgeKind::ReferencesName), 0);
}
