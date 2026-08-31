//! Check a workbook's lifted dependency edges against its formulas.
//!
//! This is the P8 measurement. `check` proves the graph is self-consistent;
//! this re-derives every dependency edge from the cells and says whether the
//! two agree, which is the only thing that catches an edge pointing at the
//! wrong region.
//!
//! Usage: `cargo run --release --example lifting -- <workbook> [--findings N]`
//!
//! Everything it prints is a count, a ratio, a duration, an A1 address or a
//! region title — the same rule as `graph`, so it is safe to run against a
//! confidential workbook and paste the output. A reference is an address, not a
//! value, so the worked examples carry no cell contents either.

use std::time::Instant;

use eg_graph::{audit, check, AuditOptions, GraphOptions};
use eg_ingest::{load_with, LoadOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: lifting <workbook> [--findings N]");
        std::process::exit(2);
    };
    let rest: Vec<String> = args.collect();
    let max_findings = rest
        .iter()
        .position(|a| a == "--findings")
        .and_then(|i| rest.get(i + 1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(32);

    let load_opts = LoadOptions {
        max_cells: None,
        ..Default::default()
    };
    let started = Instant::now();
    let loaded = match load_with(&path, &load_opts) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not load {path}: {e}");
            std::process::exit(1);
        }
    };
    let load_time = started.elapsed();

    // The graph the corpus stores: formula-group nodes are rebuilt on demand,
    // and lifting does not read them, so this is the graph worth auditing.
    let built = eg_graph::build_with(
        &loaded.workbook,
        &GraphOptions {
            formula_group_nodes: false,
            ..Default::default()
        },
    );
    let violations = check(&built);
    let report = audit(
        &loaded.workbook,
        &built.graph,
        &AuditOptions { max_findings },
    );

    println!("workbook:        {}", loaded.workbook.path);
    println!("  sheets:        {}", loaded.workbook.sheets.len());
    println!("  load time:     {:.2}s", load_time.as_secs_f64());
    println!(
        "  build time:    {:.2}s",
        built.report.build_time.as_secs_f64()
    );

    println!("\nstructural invariants: {}", violations.len());
    for v in &violations {
        println!("  {}: {}", v.invariant, v.detail);
    }

    println!(
        "\nre-derived from the cells in {:.2}s",
        report.audit_time.as_secs_f64()
    );
    println!("  formulas read:       {:>12}", report.formulas_read);
    println!("  references read:     {:>12}", report.references_read);
    println!("  landing in a region: {:>12}", report.references_landed);

    println!("\nedges");
    println!("  {:<22} {:>12}  {:>14}", "", "edges", "references");
    println!(
        "  {:<22} {:>12}  {:>14}",
        "workbook expects", report.edges_expected, report.weight_expected
    );
    println!(
        "  {:<22} {:>12}  {:>14}",
        "graph holds", report.edges_in_graph, report.weight_in_graph
    );
    println!(
        "  {:<22} {:>12}  ({:.1}% of expected)",
        "agreed exactly",
        report.edges_agreed,
        report.agreement() * 100.0
    );

    println!("\nfindings: {}", report.findings_total);
    for finding in &report.findings {
        println!("  {:<22} {}", finding.kind.as_str(), finding.detail);
    }
    if report.findings_total as usize > report.findings.len() {
        println!(
            "  ... {} more not shown",
            report.findings_total as usize - report.findings.len()
        );
    }
    if report.agrees() && violations.is_empty() {
        println!("\nthe graph says exactly what the cells say.");
    }
}
