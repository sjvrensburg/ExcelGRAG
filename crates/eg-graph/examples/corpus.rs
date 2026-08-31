//! Index workbooks into a corpus, and report what storing bought.
//!
//! Usage: `corpus [--groups] <dir> <workbook>...` to index, `corpus <dir>` to
//! list. `--groups` stores the formula-group layer too, so the cost of keeping
//! it can be measured rather than argued about.
//!
//! Reports counts, sizes and timings, plus sheet and region names. Never cell
//! contents, so it is safe against a confidential workbook.
//!
//! The point of interest is the last line: how long a cold build takes against
//! how long reloading the stored graph takes. If reloading is not dramatically
//! cheaper, the store is not worth keeping.

use std::time::Instant;

use eg_graph::store::Corpus;
use eg_graph::{build_with, GraphOptions};
use eg_ingest::{load_with, LoadOptions};

fn main() {
    let mut groups = false;
    let mut positional: Vec<String> = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--groups" => groups = true,
            _ => positional.push(arg),
        }
    }
    let mut args = positional.into_iter();
    let Some(dir) = args.next() else {
        eprintln!("usage: corpus [--groups] <dir> <workbook>...");
        std::process::exit(2);
    };
    let workbooks: Vec<String> = args.collect();

    let mut corpus = match Corpus::open(&dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not open corpus: {e}");
            std::process::exit(1);
        }
    };

    for path in &workbooks {
        index(&mut corpus, path, groups);
    }

    println!("\ncorpus at {dir}: {} workbook(s)", corpus.len());
    println!(
        "  {:<44} {:>8} {:>12} {:>8} {:>8}",
        "path", "sheets", "cells", "nodes", "edges"
    );
    for (hash, entry) in corpus.entries() {
        println!(
            "  {:<44} {:>8} {:>12} {:>8} {:>8}   {}{}",
            tail(&entry.path, 44),
            entry.sheets,
            entry.cells,
            entry.nodes,
            entry.edges,
            &hash[..hash.len().min(8)],
            if entry.formula_group_nodes {
                " (with groups)"
            } else {
                ""
            }
        );
    }
}

fn index(corpus: &mut Corpus, path: &str, groups: bool) {
    let opts = LoadOptions {
        max_cells: None,
        ..Default::default()
    };

    let cold = Instant::now();
    let loaded = match load_with(path, &opts) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not load {path}: {e}");
            return;
        }
    };
    let load_time = cold.elapsed();
    let hash = loaded.workbook.content_hash.clone();

    // The region-level graph is what the corpus stores by default; the formula
    // groups are rebuilt from the file when something drills into one workbook.
    // Whether that is still the right trade is what `--groups` measures.
    let built = build_with(
        &loaded.workbook,
        &GraphOptions {
            formula_group_nodes: groups,
            ..Default::default()
        },
    );
    let cold_total = cold.elapsed();

    if let Err(e) = corpus.put(
        &hash,
        path,
        loaded.workbook.sheets.len(),
        loaded.workbook.total_cells() as u64,
        groups,
        &built,
    ) {
        eprintln!("could not store {path}: {e}");
        return;
    }

    let warm = Instant::now();
    let stored = match corpus.get(&hash) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("{path}: stored graph did not come back");
            return;
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            return;
        }
    };
    let warm_time = warm.elapsed();
    let reloaded = stored.into_built();

    println!("{path}");
    println!(
        "  sheets {}, cells {}",
        loaded.workbook.sheets.len(),
        loaded.workbook.total_cells()
    );
    println!(
        "  graph  {} nodes, {} edges",
        reloaded.report.total_nodes(),
        reloaded.report.total_edges()
    );

    let violations = eg_graph::check(&reloaded);
    if violations.is_empty() {
        println!("  invariants hold on the reloaded graph");
    } else {
        for v in &violations {
            println!(
                "  INVARIANT VIOLATED after reload: {} — {}",
                v.invariant, v.detail
            );
        }
    }

    println!(
        "  cold   {:.2}s  (load {:.2}s + build {:.2}s)",
        cold_total.as_secs_f64(),
        load_time.as_secs_f64(),
        (cold_total - load_time).as_secs_f64()
    );
    if let Some(bytes) = stored_bytes(corpus, &hash) {
        println!(
            "  stored {:.1} KB of JSON{}",
            bytes as f64 / 1024.0,
            if groups { " (with formula groups)" } else { "" }
        );
    }
    println!(
        "  warm   {:.2}ms reload from the store — {:.0}x faster",
        warm_time.as_secs_f64() * 1000.0,
        cold_total.as_secs_f64() / warm_time.as_secs_f64().max(1e-9)
    );
}

/// The size on disk of one stored graph, for comparing what a layer costs.
fn stored_bytes(corpus: &Corpus, hash: &str) -> Option<u64> {
    std::fs::metadata(corpus.graph_path(hash))
        .ok()
        .map(|m| m.len())
}

fn tail(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let skip = s.chars().count() - (n - 1);
    format!("…{}", s.chars().skip(skip).collect::<String>())
}
