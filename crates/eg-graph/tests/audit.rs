//! The audit, against graphs whose lifting is right and graphs it has been
//! broken in.
//!
//! An audit that passes on a correct graph proves nothing on its own — so does
//! one that returns nothing ever. Each defect below is one `check` accepts
//! without complaint, which is the whole reason this layer exists, and every
//! test asserts both halves: that the audit catches it, and that `check` does
//! not.

use eg_graph::{audit, build, build_with, check, AuditOptions, Edge, EdgeKind, FindingKind};
use eg_graph::{BuiltGraph, Node, NodeKind, RegionNode};
use eg_model::{Cell, CellValue, RangeRef, Sheet, SheetId, Workbook, WorkbookFormat};
use eg_structure::{RegionKind, RegionSource};
use petgraph::graph::NodeIndex;

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

/// Two sheets, where every row of `Sales` reads one row of `Rates`.
fn two_sheets() -> Workbook {
    workbook(vec![
        grid(
            0,
            "Sales",
            &[
                "Region Net",
                "North =Rates!A2",
                "South =Rates!A3",
                "East =Rates!A4",
            ],
        ),
        grid(1, "Rates", &["Rate", "1", "2", "3"]),
    ])
}

/// `Sales` reads the first of two blocks on `Data`, so an edge can be pointed
/// at the wrong region without ceasing to cross a sheet — which is what keeps
/// `check` quiet about it.
fn two_regions_on_one_sheet() -> Workbook {
    workbook(vec![
        grid(0, "Sales", &["Total", "=Data!A1"]),
        grid(1, "Data", &["1 x", "2 y", ". .", "3 z", "4 w"]),
    ])
}

fn kinds(report: &eg_graph::AuditReport) -> Vec<FindingKind> {
    report.findings.iter().map(|f| f.kind).collect()
}

/// The first dependency edge in the graph, with its endpoints.
fn first_dependency(built: &BuiltGraph) -> (petgraph::graph::EdgeIndex, NodeIndex, NodeIndex) {
    let edge = built
        .graph
        .edge_indices()
        .find(|&e| !built.graph[e].kind.is_structural())
        .expect("the fixture has a lifted dependency");
    let (a, b) = built.graph.edge_endpoints(edge).unwrap();
    (edge, a, b)
}

/// Another region of the same sheet as `not`, so an edge moved onto it keeps
/// whatever kind it had.
fn a_region_other_than_on_the_same_sheet(built: &BuiltGraph, not: NodeIndex) -> NodeIndex {
    let sheet = built.graph[not].sheet();
    built
        .graph
        .node_indices()
        .find(|&i| {
            i != not && built.graph[i].kind() == NodeKind::Region && built.graph[i].sheet() == sheet
        })
        .expect("the fixture has a second region on that sheet")
}

#[test]
fn a_correctly_lifted_graph_has_nothing_to_report() {
    let wb = two_sheets();
    let built = build(&wb);
    let report = audit(&wb, &built.graph, &AuditOptions::default());

    assert!(report.agrees(), "{:?}", report.findings);
    assert_eq!(report.findings_total, 0);
    // The audit must actually have had something to check: a report of zero
    // findings over zero expectations is what a broken audit also returns.
    assert_eq!(report.edges_expected, 1, "one merged CROSS_SHEET_REF");
    assert_eq!(report.edges_agreed, 1);
    assert_eq!(report.weight_expected, 3, "three references behind it");
    assert_eq!(report.weight_in_graph, 3);
    assert_eq!(report.agreement(), 1.0);
}

#[test]
fn the_audit_reads_the_same_formulas_the_build_lifted() {
    let wb = two_sheets();
    let built = build(&wb);
    let report = audit(&wb, &built.graph, &AuditOptions::default());

    let formulas = wb
        .sheets
        .iter()
        .flat_map(|s| s.iter())
        .filter(|(_, c)| c.formula.is_some())
        .count() as u64;
    assert_eq!(report.formulas_read, formulas);
    assert_eq!(report.references_read, built.report.references_scanned);
    // Both halves: a reference that landed in a region either made an edge or
    // was dropped as a self-reference, and the build counts those separately.
    assert_eq!(
        report.references_landed,
        built.report.references_lifted + built.report.references_within_source_region
    );
}

#[test]
fn an_edge_pointed_at_the_wrong_region_is_caught() {
    // The defect `check` is blind to, and the reason for this whole module: a
    // reference lifted to a region it does not land in is still reachable,
    // still on one sheet, still positively weighted.
    let wb = two_regions_on_one_sheet();
    let mut built = build(&wb);
    let (edge, source, target) = first_dependency(&built);
    let elsewhere = a_region_other_than_on_the_same_sheet(&built, target);
    let weight = built.graph[edge];
    built.graph.remove_edge(edge);
    built.graph.add_edge(source, elsewhere, weight);

    assert_eq!(check(&built), vec![], "check accepts the broken graph");

    let report = audit(&wb, &built.graph, &AuditOptions::default());
    assert!(!report.agrees());
    // Seen from both sides: the edge the workbook wanted is gone, and the one
    // that replaced it stands for references that do not exist.
    assert_eq!(
        kinds(&report),
        vec![FindingKind::MissingEdge, FindingKind::UnaccountedEdge]
    );
    assert_eq!(report.edges_agreed, 0);
}

#[test]
fn an_edge_dropped_altogether_is_caught() {
    let wb = two_sheets();
    let mut built = build(&wb);
    let (edge, ..) = first_dependency(&built);
    built.graph.remove_edge(edge);

    assert_eq!(check(&built), vec![]);

    let report = audit(&wb, &built.graph, &AuditOptions::default());
    assert_eq!(kinds(&report), vec![FindingKind::MissingEdge]);
    assert_eq!(report.edges_in_graph, 0);
    assert_eq!(report.edges_expected, 1);
    assert_eq!(report.agreement(), 0.0);
}

#[test]
fn a_weight_that_miscounts_its_references_is_caught() {
    // Weight is what ranks a dependency in retrieval, so a wrong one is not
    // cosmetic: it decides which table an agent is shown first.
    let wb = two_sheets();
    let mut built = build(&wb);
    let (edge, ..) = first_dependency(&built);
    built.graph[edge].weight = 99;

    assert_eq!(check(&built), vec![]);

    let report = audit(&wb, &built.graph, &AuditOptions::default());
    assert_eq!(kinds(&report), vec![FindingKind::WeightDisagrees]);
    assert_eq!(report.weight_in_graph, 99);
    assert_eq!(report.weight_expected, 3);
    let detail = &report.findings[0].detail;
    assert!(detail.contains("99"), "{detail}");
    assert!(detail.contains("Sales!B2"), "cites a formula: {detail}");
}

#[test]
fn the_same_dependency_split_across_two_edges_is_caught() {
    // Merging is what keeps the graph small; two parallel edges of the same
    // kind mean it did not happen, and the weight on each understates the
    // evidence even though the pair sums correctly.
    let wb = two_sheets();
    let mut built = build(&wb);
    let (edge, source, target) = first_dependency(&built);
    built.graph[edge].weight = 2;
    built.graph.add_edge(
        source,
        target,
        Edge {
            kind: EdgeKind::CrossSheetRef,
            weight: 1,
        },
    );

    assert_eq!(check(&built), vec![]);

    let report = audit(&wb, &built.graph, &AuditOptions::default());
    assert_eq!(kinds(&report), vec![FindingKind::ParallelEdges]);
    // The two together still account for every reference, which is exactly why
    // summing alone would have called this graph correct.
    assert_eq!(report.weight_in_graph, report.weight_expected);
}

#[test]
fn two_regions_over_one_cell_are_reported_rather_than_resolved() {
    // Region detection does not overlap today, and nothing in `check` says it
    // may not start. If it did, which region owns a formula — and so where its
    // edges begin — would depend on which was tried first.
    let wb = two_sheets();
    let mut built = build(&wb);
    let region = built
        .graph
        .node_indices()
        .find(|&i| match &built.graph[i] {
            Node::Region(r) => r.range.sheet == SheetId(0),
            _ => false,
        })
        .expect("Sales has a region");
    let Node::Region(existing) = built.graph[region].clone() else {
        unreachable!()
    };
    let twin = built.graph.add_node(Node::Region(RegionNode {
        range: existing.range,
        kind: RegionKind::Block,
        source: RegionSource::Detected,
        title: Some("twin".into()),
        header_rows: 0,
        cell_count: existing.cell_count,
    }));
    let sheet = built
        .graph
        .neighbors_directed(region, petgraph::Direction::Incoming)
        .next()
        .unwrap();
    built
        .graph
        .add_edge(sheet, twin, Edge::new(EdgeKind::Contains));

    let report = audit(&wb, &built.graph, &AuditOptions::default());
    assert!(report
        .findings
        .iter()
        .any(|f| f.kind == FindingKind::AmbiguousContainment));
}

#[test]
fn a_reference_a_region_makes_to_itself_expects_no_edge() {
    // The build drops the self-loop, and an audit that did not would demand an
    // edge on every region in the workbook.
    let wb = workbook(vec![grid(
        0,
        "Sales",
        &["Amount Doubled", "10 =A2*2", "20 =A3*2"],
    )]);
    let built = build(&wb);
    let report = audit(&wb, &built.graph, &AuditOptions::default());

    assert!(report.agrees(), "{:?}", report.findings);
    assert_eq!(report.edges_expected, 0);
    assert_eq!(report.references_read, 2);
    assert_eq!(
        report.references_landed, 2,
        "they landed, then were dropped"
    );
}

#[test]
fn a_range_spanning_two_regions_expects_an_edge_to_each() {
    let wb = workbook(vec![
        grid(0, "Sales", &["Total", "=Data!A1:A4"]),
        grid(1, "Data", &["1 x", "2 y", ". .", "3 z", "4 w"]),
    ]);
    let built = build(&wb);
    let report = audit(&wb, &built.graph, &AuditOptions::default());

    assert!(report.agrees(), "{:?}", report.findings);
    assert_eq!(report.edges_expected, 2);
    assert_eq!(report.references_landed, 1, "one reference, two edges");
    assert_eq!(report.weight_expected, 2);
}

#[test]
fn references_that_leave_the_workbook_are_not_this_audits_business() {
    // An external reference and a dead sheet name are both real findings about
    // the workbook, and neither is a finding about lifting to a region.
    let wb = workbook(vec![grid(
        0,
        "Sales",
        &["Net Other", "=[1]Book!A1 =Gone!A1"],
    )]);
    let built = build(&wb);
    let report = audit(&wb, &built.graph, &AuditOptions::default());

    assert!(report.agrees(), "{:?}", report.findings);
    assert_eq!(report.references_read, 2);
    assert_eq!(report.references_landed, 0);
    assert_eq!(report.edges_expected, 0);
}

#[test]
fn a_graph_without_formula_group_nodes_audits_the_same() {
    // What the corpus stores: groups are 119 MiB and are rebuilt on demand, so
    // the graph that gets audited in anger is this one.
    let wb = two_sheets();
    let stored = build_with(
        &wb,
        &eg_graph::GraphOptions {
            formula_group_nodes: false,
            ..Default::default()
        },
    );
    let report = audit(&wb, &stored.graph, &AuditOptions::default());

    assert!(report.agrees(), "{:?}", report.findings);
    assert_eq!(report.edges_expected, 1);
    assert_eq!(report.weight_expected, 3);
}

#[test]
fn the_findings_are_capped_and_the_count_is_not() {
    let wb = two_sheets();
    let mut built = build(&wb);
    let region = built
        .graph
        .node_indices()
        .find(|&i| built.graph[i].kind() == NodeKind::Region)
        .unwrap();
    // Three edges to nothing the workbook asked for, and room to report one.
    for _ in 0..3 {
        let node = built.graph.add_node(Node::Region(RegionNode {
            range: RangeRef::new(SheetId(0), 900, 900, 901, 901),
            kind: RegionKind::Block,
            source: RegionSource::Detected,
            title: None,
            header_rows: 0,
            cell_count: 0,
        }));
        built
            .graph
            .add_edge(region, node, Edge::new(EdgeKind::DependsOn));
    }

    let report = audit(&wb, &built.graph, &AuditOptions { max_findings: 1 });
    assert_eq!(report.findings_total, 3);
    assert_eq!(report.findings.len(), 1);
}

#[test]
fn the_audit_reports_the_same_findings_in_the_same_order_every_run() {
    let wb = two_regions_on_one_sheet();
    let mut built = build(&wb);
    let (edge, source, target) = first_dependency(&built);
    let elsewhere = a_region_other_than_on_the_same_sheet(&built, target);
    let weight = built.graph[edge];
    built.graph.remove_edge(edge);
    built.graph.add_edge(source, elsewhere, weight);

    let opts = AuditOptions::default();
    let a = audit(&wb, &built.graph, &opts);
    let b = audit(&wb, &built.graph, &opts);
    let details = |r: &eg_graph::AuditReport| -> Vec<String> {
        r.findings.iter().map(|f| f.detail.clone()).collect()
    };
    assert_eq!(details(&a), details(&b));
}
