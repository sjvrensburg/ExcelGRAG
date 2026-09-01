//! `check`'s own violation detection — the positive case. `tests/audit.rs`
//! exercises the negative case extensively (graphs `check` stays silent
//! about, which is why `audit` exists), but a broken invariant it is
//! actually supposed to catch was never directly asserted anywhere.

use eg_graph::{build, check, Edge, EdgeKind};
use eg_model::{Cell, CellValue, Sheet, SheetId, Workbook, WorkbookFormat};
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

/// `Sales` reads `Rates`, on a different sheet — a real `CrossSheetRef` edge
/// to break in two unrelated ways at once.
fn two_sheets() -> Workbook {
    Workbook {
        path: "test.xlsx".into(),
        format: Some(WorkbookFormat::Xlsx),
        content_hash: "0".into(),
        sheets: vec![
            grid(0, "Sales", &["Region Net", "North =Rates!A2"]),
            grid(1, "Rates", &["Rate", "1"]),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

#[test]
fn a_correctly_built_graph_has_nothing_to_report() {
    let wb = two_sheets();
    let built = build(&wb);
    assert_eq!(check(&built), vec![]);
}

#[test]
fn an_invalid_root_is_reported_instead_of_panicking() {
    let mut built = build(&two_sheets());
    built.root = NodeIndex::new(built.graph.node_count() + 100);
    let violations = check(&built);
    assert!(
        violations
            .iter()
            .any(|violation| violation.invariant == "root belongs to the graph"),
        "{violations:?}"
    );
}

#[test]
fn a_zero_weight_edge_is_caught() {
    let wb = two_sheets();
    let mut built = build(&wb);
    let edge = built
        .graph
        .edge_indices()
        .find(|&e| !built.graph[e].kind.is_structural())
        .expect("a lifted dependency");
    built.graph[edge].weight = 0;

    let violations = check(&built);
    assert_eq!(violations.len(), 1);
    assert_eq!(
        violations[0].invariant,
        "every edge stands for at least one reference"
    );
}

#[test]
fn two_unrelated_violations_in_the_same_pass_are_both_reported() {
    // L22: three invariants — zero weight, a same-sheet edge that crosses
    // sheets, and a `CROSS_SHEET_REF` that does not — share one pass over
    // the edges. A `break` the moment any one of them fired used to silence
    // the other two for the rest of the graph, even when they were on a
    // completely different edge.
    let wb = two_sheets();
    let mut built = build(&wb);
    let cross_sheet = built
        .graph
        .edge_indices()
        .find(|&e| !built.graph[e].kind.is_structural())
        .expect("a lifted dependency");
    let (source, target) = built.graph.edge_endpoints(cross_sheet).unwrap();

    // Violation 1: the real cross-sheet edge, weighted zero.
    built.graph[cross_sheet].weight = 0;
    // Violation 2, unrelated: a `DependsOn` edge — same-sheet-required —
    // between the same two (different-sheet) nodes.
    built.graph.add_edge(
        source,
        target,
        Edge {
            kind: EdgeKind::DependsOn,
            weight: 1,
        },
    );

    let violations = check(&built);
    let invariants: Vec<&str> = violations.iter().map(|v| v.invariant).collect();
    assert!(
        invariants.contains(&"every edge stands for at least one reference"),
        "{invariants:?}"
    );
    assert!(
        invariants.contains(&"same-sheet edges stay on one sheet"),
        "the second, unrelated violation must not be silenced by the first: {invariants:?}"
    );
}
