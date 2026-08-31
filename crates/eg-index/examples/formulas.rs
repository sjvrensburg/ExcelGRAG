//! Index one workbook down to its formula groups, and search the formulas.
//!
//! Usage: `formulas <index-dir> <workbook> [--limit n] <query>...`
//!
//! This is the measurement that decided the formula-group layer is worth
//! keeping: how many documents it adds, how much disk they cost, and whether a
//! search over them still returns in under a millisecond. The answer was yes on
//! every count, and `eg index` now stores and indexes the layer as a matter of
//! course, up to `MAX_STORED_FORMULA_GROUPS`.
//!
//! It is still useful on its own, for a workbook past that ceiling or one not
//! in a corpus at all. The index goes in its own directory, not the corpus's,
//! because a graph built with groups and one built without are not two versions
//! of the same thing.
//!
//! ```sh
//! cargo run --release -p eg-index --example formulas -- private/formulas private/book.xlsb vlookup
//! ```
//!
//! Prints formulas, which are structure rather than data, and A1 ranges. Never
//! cell values.

use std::time::Instant;

use eg_graph::{build_with, GraphOptions, NodeKind};
use eg_index::{SearchOptions, TextIndex};
use eg_ingest::{load_with, LoadOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(dir), Some(path)) = (args.next(), args.next()) else {
        eprintln!("usage: formulas <index-dir> <workbook> [--limit n] <query>...");
        std::process::exit(2);
    };

    let mut opts = SearchOptions {
        // Only the formula groups: the point here is the layer the corpus does
        // not keep, and the region-level nodes are already searchable through
        // the corpus index.
        kinds: vec![NodeKind::FormulaGroup],
        ..Default::default()
    };
    let mut words: Vec<String> = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--limit" => opts.limit = args.next().and_then(|n| n.parse().ok()).unwrap_or(10),
            _ => words.push(arg),
        }
    }
    let query = words.join(" ");

    let loading = Instant::now();
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
    let load_time = loading.elapsed();

    let building = Instant::now();
    let built = build_with(&loaded.workbook, &GraphOptions::default());
    let build_time = building.elapsed();

    let mut index = match TextIndex::open(&dir) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("could not open index: {e}");
            std::process::exit(1);
        }
    };

    let indexing = Instant::now();
    let documents = match index.index_built(&built, &loaded.workbook.content_hash, &path) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("could not index {path}: {e}");
            std::process::exit(1);
        }
    };
    let index_time = indexing.elapsed();

    println!("{path}");
    println!(
        "  {} sheets, {} cells, {} nodes ({} formula groups)",
        loaded.workbook.sheets.len(),
        loaded.workbook.total_cells(),
        built.report.total_nodes(),
        built.report.nodes_of(NodeKind::FormulaGroup)
    );
    println!(
        "  load {:.2}s, build {:.2}s, index {:.2}s for {documents} documents",
        load_time.as_secs_f64(),
        build_time.as_secs_f64(),
        index_time.as_secs_f64()
    );
    println!(
        "  {:.1} MiB on disk at {}",
        index.size_on_disk() as f64 / (1024.0 * 1024.0),
        index.path().display()
    );

    if query.trim().is_empty() {
        println!("\nno query given");
        return;
    }

    let searching = Instant::now();
    let hits = match index.search(&query, &opts) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("search failed: {e}");
            std::process::exit(1);
        }
    };
    let search_time = searching.elapsed();

    println!(
        "\n{:?} — {} hit(s) in {:.2}ms",
        query,
        hits.len(),
        search_time.as_secs_f64() * 1000.0
    );
    for hit in &hits {
        println!(
            "  {:>6.2}  {:<52} {}",
            hit.score,
            truncate(&hit.label, 52),
            hit.a1.as_deref().unwrap_or("")
        );
    }
    if hits.is_empty() {
        println!("  nothing matched");
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    format!("{}…", s.chars().take(n - 1).collect::<String>())
}
