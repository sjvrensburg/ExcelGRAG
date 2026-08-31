//! Read the relations a workbook states in its own lookup formulas.
//!
//! Usage: `cargo run --release --example schema -- <workbook> [--limit N]
//! [--show-formulas]`
//!
//! Prints counts, A1 ranges and function names — structure, not data — so it is
//! safe against a confidential workbook. `--show-formulas` adds the
//! representative formula of each relation, which is structure too but reads
//! like content and so is opt-in.

use std::time::Instant;

use eg_eval::{infer_schema, Lookup};
use eg_ingest::{load_with, LoadOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: schema <workbook> [--limit N]");
        std::process::exit(2);
    };
    let rest: Vec<String> = args.collect();
    let limit = rest
        .iter()
        .position(|a| a == "--limit")
        .and_then(|i| rest.get(i + 1))
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(20);

    let loaded = match load_with(
        &path,
        &LoadOptions {
            max_cells: None,
            ..Default::default()
        },
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not load {path}: {e}");
            std::process::exit(1);
        }
    };

    let started = Instant::now();
    let schema = infer_schema(&loaded.workbook);
    let elapsed = started.elapsed();
    let cite = |r: eg_model::RangeRef| loaded.workbook.cite_range(r);

    println!("workbook:        {}", loaded.workbook.path);
    println!("  formula groups examined: {}", schema.groups);
    println!("  of those, doing lookups: {}", schema.with_lookups);
    println!("  relations recovered:     {}", schema.lookups.len());
    println!("  shapes not read:         {}", schema.unrecognised);
    println!("  pointing outside:        {}", schema.unresolvable);
    println!("  in:                      {:.2}s", elapsed.as_secs_f64());

    let keys: Vec<&Lookup> = schema.keys().collect();
    let bandings = schema.lookups.len() - keys.len();
    println!(
        "\n{} key relation(s), {bandings} banding(s) — an approximate lookup is a \
         set of thresholds, not a key",
        keys.len()
    );

    println!("\nheaviest first, by the cells behind them:");
    for lookup in schema.lookups.iter().take(limit) {
        println!(
            "  {:>10} cells  {:<12} {}",
            lookup.cells,
            lookup.kind.as_str(),
            cite(lookup.from)
        );
        println!(
            "        key {}  →  table {}{}{}",
            lookup.key.map(cite).unwrap_or_else(|| "(computed)".into()),
            cite(lookup.table),
            lookup
                .column
                .map(|c| format!(" column {c}"))
                .unwrap_or_default(),
            if lookup.approximate {
                "   [approximate — a banding]"
            } else {
                ""
            }
        );
        if let Some(returns) = lookup.returns {
            println!("        returns {}", cite(returns));
        }
    }
    if schema.lookups.len() > limit {
        println!("  … {} more", schema.lookups.len() - limit);
    }
}
