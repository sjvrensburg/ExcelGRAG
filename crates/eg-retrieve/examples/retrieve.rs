//! Ask a question of a corpus and print the context around the answer.
//!
//! Usage: `retrieve <corpus-dir> [options] <query>...`
//!
//!   --hops <n>       dependency hops from a seed, default 2
//!   --budget <n>     most nodes per workbook, default 40
//!   --children <n>   contained children to show per node, default 0
//!   --seeds <n>      how many hits to expand from, default 5
//!   --lexical        skip the embedding model and search by word only
//!
//! ```sh
//! cargo run --release -p eg-graph --example corpus -- index private/book.xlsb
//! cargo run --release -p eg-index --example semantic -- index warm up the indexes
//! cargo run --release -p eg-retrieve --example retrieve -- index bad debt provision
//! ```
//!
//! Every line says why it is there: a seed came from the index, and everything
//! else names the node that pulled it in and the edge that did it. An expansion
//! nobody can check is an expansion nobody should trust.
//!
//! Prints node labels and A1 ranges, never cell values.

use std::collections::HashSet;
use std::time::Instant;

use eg_graph::store::Corpus;
use eg_index::vector::VectorIndex;
use eg_index::{fuse, Embedder, Hit, SearchOptions, TextIndex};
use eg_retrieve::{expand, ExpandOptions, Role, WorkbookContext};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("usage: retrieve <corpus-dir> [--hops n] [--budget n] [--children n] [--seeds n] [--lexical] <query>...");
        std::process::exit(2);
    };

    let mut opts = ExpandOptions::default();
    let mut seeds = 5usize;
    let mut lexical_only = false;
    let mut words: Vec<String> = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut number = |name: &str| match args.next().as_deref().and_then(|n| n.parse().ok()) {
            Some(n) => n,
            None => {
                eprintln!("{name} wants a number");
                std::process::exit(2);
            }
        };
        match arg.as_str() {
            "--hops" => opts.hops = number("--hops"),
            "--budget" => opts.budget = number("--budget").max(1),
            "--children" => opts.children = number("--children"),
            "--seeds" => seeds = number("--seeds").max(1),
            "--lexical" => lexical_only = true,
            _ => words.push(arg),
        }
    }
    let query = words.join(" ");
    if query.trim().is_empty() {
        eprintln!("nothing to search for");
        std::process::exit(2);
    }

    let corpus = match Corpus::open(&dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not open corpus: {e}");
            std::process::exit(1);
        }
    };
    let text = match TextIndex::open(&dir) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("could not open the lexical index: {e}");
            std::process::exit(1);
        }
    };

    let search_opts = SearchOptions {
        limit: seeds,
        ..Default::default()
    };
    let searching = Instant::now();
    let lexical = text.search(&query, &search_opts).unwrap_or_else(|e| {
        eprintln!("lexical search failed: {e}");
        Vec::new()
    });

    let hits: Vec<Hit> = if lexical_only {
        lexical
    } else {
        match semantic(&dir, &query, &search_opts) {
            Ok(semantic) => fuse(&[&lexical, &semantic], seeds),
            Err(e) => {
                eprintln!("no semantic half ({e}); searching by word only");
                lexical
            }
        }
    };
    let search_time = searching.elapsed();

    if hits.is_empty() {
        println!("{query:?} — nothing matched");
        return;
    }

    let expanding = Instant::now();
    let found = match expand(&corpus, &hits, &opts) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("expansion failed: {e}");
            std::process::exit(1);
        }
    };
    let expand_time = expanding.elapsed();

    println!("{query:?}");
    println!(
        "  {} seed(s) in {:.0}ms, expanded to {} node(s) in {:.2}ms — {} hop(s), budget {}",
        hits.len(),
        search_time.as_secs_f64() * 1000.0,
        found.total_nodes(),
        expand_time.as_secs_f64() * 1000.0,
        opts.hops,
        opts.budget
    );
    for hash in &found.missing_workbooks {
        println!(
            "  {}: matched in the index but not in the corpus — reindex",
            short(hash)
        );
    }

    for workbook in &found.workbooks {
        show(workbook);
    }
}

/// The semantic half, which needs the model. Separated so that a corpus with no
/// vectors, or a machine that cannot reach the model, still retrieves.
fn semantic(dir: &str, query: &str, opts: &SearchOptions) -> Result<Vec<Hit>, String> {
    let mut embedder = Embedder::new().map_err(|e| e.to_string())?;
    let vectors =
        VectorIndex::open(dir, embedder.name(), embedder.dim()).map_err(|e| e.to_string())?;
    if vectors.is_empty() {
        return Err("no vectors stored".to_string());
    }
    let q = embedder.embed_query(query).map_err(|e| e.to_string())?;
    Ok(vectors.search(&q, opts))
}

fn show(workbook: &WorkbookContext) {
    println!("\n{}", workbook.path);
    if workbook.truncated {
        println!("  (budget reached — this is part of the context, not all of it)");
    }

    // Every node retrieved is printed exactly once. A summary line that says 36
    // over a list of 20 is worse than no summary: the reader cannot tell which
    // sixteen were left out, or that any were.
    let mut printed: HashSet<u32> = HashSet::new();

    // Grouped under the seed that reached them, which is the order a person
    // reads in: here is the hit, and here is what stands behind it.
    for seed in workbook.seeds() {
        printed.insert(seed.node);
        println!(
            "\n  {} {}{}",
            seed.kind.as_str(),
            seed.label,
            seed.score
                .map(|s| format!("   [{s:.4}]"))
                .unwrap_or_default()
        );
        if let Some(a1) = &seed.a1 {
            println!("    at {a1}");
        }
        let path = workbook.ancestry(seed.node);
        printed.extend(path.iter().map(|n| n.node));
        println!("    in {}", render_path(&path));

        for node in related_to(workbook, seed.node) {
            printed.insert(node.node);
            println!(
                "    {}: {} {}   {}",
                relation(node).unwrap_or_default(),
                node.kind.as_str(),
                node.label,
                node.a1.as_deref().unwrap_or_default()
            );
            // A second hop hangs off the first, so the chain that explains a
            // number stays visible as a chain.
            for further in related_to(workbook, node.node) {
                printed.insert(further.node);
                println!(
                    "      which {}: {} {}   {}",
                    relation(further).unwrap_or_default(),
                    further.kind.as_str(),
                    further.label,
                    further.a1.as_deref().unwrap_or_default()
                );
            }
        }
    }

    // Whatever the nesting above did not reach: a third hop, or a node whose
    // origin was itself never printed.
    let rest: Vec<&eg_retrieve::RetrievedNode> = workbook
        .nodes
        .iter()
        .filter(|n| !printed.contains(&n.node))
        .collect();
    if rest.is_empty() {
        return;
    }
    println!("\n  also retrieved");
    for node in rest {
        let origin = node
            .role
            .from()
            .and_then(|f| workbook.node(f))
            .map(|f| f.label.as_str())
            .unwrap_or("?");
        println!(
            "    {} {}   {}   ({} {origin})",
            node.kind.as_str(),
            node.label,
            node.a1.as_deref().unwrap_or_default(),
            relation(node).unwrap_or_else(|| node.role.as_str().to_string()),
        );
    }
}

/// The nodes this one pulled in, heaviest dependency first.
fn related_to(workbook: &WorkbookContext, of: u32) -> Vec<&eg_retrieve::RetrievedNode> {
    let mut out: Vec<&eg_retrieve::RetrievedNode> = workbook
        .nodes
        .iter()
        .filter(|n| n.role.from() == Some(of))
        .filter(|n| !matches!(n.role, Role::Ancestor { .. } | Role::Seed))
        .collect();
    out.sort_by_key(|n| match &n.role {
        Role::Input { weight, .. } | Role::Dependent { weight, .. } => std::cmp::Reverse(*weight),
        _ => std::cmp::Reverse(0),
    });
    out
}

/// How a node relates to the one that pulled it in, in words.
fn relation(node: &eg_retrieve::RetrievedNode) -> Option<String> {
    match &node.role {
        Role::Input { kind, weight, .. } => {
            Some(format!("reads ({}, {weight} refs)", kind.as_str()))
        }
        Role::Dependent { kind, weight, .. } => {
            Some(format!("is read by ({}, {weight} refs)", kind.as_str()))
        }
        Role::Child { .. } => Some("contains".to_string()),
        Role::Ancestor { .. } | Role::Seed => None,
    }
}

/// A containment path, outermost first.
fn render_path(path: &[&eg_retrieve::RetrievedNode]) -> String {
    if path.is_empty() {
        return "(nothing above it)".to_string();
    }
    path.iter()
        .map(|n| n.label.as_str())
        .collect::<Vec<_>>()
        .join(" › ")
}

fn short(hash: &str) -> String {
    hash.chars().take(8).collect()
}
