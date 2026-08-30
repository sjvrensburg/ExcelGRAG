//! Build the graph for a workbook and report what it looks like.
//!
//! This is the P3a measurement. Everything it prints is a count, a ratio, a
//! duration or an A1 address; sheet and region titles are structure a reader
//! needs and are shown, but no cell contents are, so it is safe to run against
//! a confidential workbook and paste the output.
//!
//! Usage: `cargo run --release --example graph -- <workbook> [--no-groups]`
//!
//! The footprint figure is measured, not estimated: a counting allocator wraps
//! the system one and reports live bytes across the build.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use eg_graph::{check, degree_stats, EdgeKind, GraphOptions, NodeKind};
use eg_ingest::{load_with, LoadOptions};

/// Tracks live allocated bytes so the graph's footprint can be measured.
///
/// Approximate in one direction only: it counts what the allocator was asked
/// for, not the allocator's own overhead, so the true resident size is a little
/// larger. It is never an underestimate of what we allocated.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new_size) };
        if !q.is_null() {
            let live = LIVE.load(Ordering::Relaxed) + new_size - layout.size();
            LIVE.store(live, Ordering::Relaxed);
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: graph <workbook> [--no-groups]");
        std::process::exit(2);
    };
    let groups = !args.any(|a| a == "--no-groups");

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
    let after_load = LIVE.load(Ordering::Relaxed);

    let opts = GraphOptions {
        formula_group_nodes: groups,
        ..Default::default()
    };
    let built = eg_graph::build_with(&loaded.workbook, &opts);
    let after_build = LIVE.load(Ordering::Relaxed);
    let r = &built.report;

    println!("workbook:        {}", loaded.workbook.path);
    println!("  sheets:        {}", loaded.workbook.sheets.len());
    println!("  cells:         {}", loaded.workbook.total_cells());
    println!("  load time:     {:.2}s", load_time.as_secs_f64());
    println!(
        "  formula groups as nodes: {}",
        if groups { "yes" } else { "no" }
    );

    println!("\nnodes: {}", r.total_nodes());
    for kind in NodeKind::ALL {
        println!("  {:<20} {}", kind.as_str(), r.nodes_of(kind));
    }

    println!("\nedges: {}", r.total_edges());
    println!("  {:<20} {:>12}  {:>14}", "kind", "edges", "references");
    for kind in EdgeKind::ALL {
        let edges = r.edges_of(kind);
        let weight = r.edge_weight_of(kind);
        if kind.is_structural() {
            println!("  {:<20} {edges:>12}  {:>14}", kind.as_str(), "-");
        } else {
            println!("  {:<20} {edges:>12}  {weight:>14}", kind.as_str());
        }
    }
    println!(
        "  lifting ratio:  {:.1} references per edge",
        r.lifting_ratio()
    );

    println!("\nreferences: {}", r.references_scanned);
    println!("  lifted to an edge:        {}", r.references_lifted);
    println!(
        "  within the same region:   {}",
        r.references_within_source_region
    );
    println!("  cross-sheet:              {}", r.references_cross_sheet);
    println!("  into another workbook:    {}", r.references_external);
    println!("  to a missing sheet:       {}", r.references_dangling);
    println!(
        "  to an empty target:       {}",
        r.references_unpopulated_target
    );
    println!(
        "defined names: {} used, {} tokens matched nothing (mostly functions)",
        r.names_resolved, r.names_not_defined
    );

    println!("\nbuild time:      {:.2}s", r.build_time.as_secs_f64());
    println!("  workbook in memory:  {:>8.1} MiB", mib(after_load));
    println!(
        "  graph adds:          {:>8.1} MiB",
        mib(after_build.saturating_sub(after_load))
    );
    println!(
        "  peak during build:   {:>8.1} MiB",
        mib(PEAK.load(Ordering::Relaxed))
    );
    if r.total_nodes() > 0 {
        let per = (after_build.saturating_sub(after_load)) as f64 / r.total_nodes() as f64;
        println!("  per node:            {per:>8.0} bytes");
    }

    let degrees = degree_stats(&built.graph, 8);
    println!("\ndegree distribution (in + out):");
    for (lower, count) in &degrees.buckets {
        if *count == 0 {
            continue;
        }
        println!("  {lower:>8}+  {count}");
    }
    println!("  max out-degree: {}", degrees.max_out);
    println!("  max in-degree:  {}", degrees.max_in);
    println!("\nmost connected nodes:");
    for (label, inc, out) in &degrees.hubs {
        println!("  in {inc:>7}  out {out:>7}  {}", truncate(label, 60));
    }

    if !r.unknown_sheets.is_empty() {
        println!("\nreferences to sheets the workbook does not have:");
        for (name, count) in &r.unknown_sheets {
            println!("  {count:>10}  {name}");
        }
    }

    if !r.dangling_examples.is_empty() {
        println!(
            "\nunresolved references (first {}):",
            r.dangling_examples.len()
        );
        for d in &r.dangling_examples {
            println!(
                "  {}  {}  {:?}",
                loaded.workbook.cite(d.from),
                d.text,
                d.reason
            );
        }
    }

    let violations = check(&built);
    println!();
    if violations.is_empty() {
        println!("all invariants hold");
    } else {
        for v in &violations {
            println!("INVARIANT VIOLATED: {} — {}", v.invariant, v.detail);
        }
        std::process::exit(1);
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n - 1).collect();
    format!("{head}…")
}
