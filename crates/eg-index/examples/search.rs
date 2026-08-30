//! Index a corpus of workbook graphs, then search it.
//!
//! Usage: `search <corpus-dir> [options] <query>...`
//!
//!   --kind <k>       only these node kinds; repeatable. `formula-group` too.
//!   --sheet <name>   only nodes on this sheet
//!   --workbook <h>   only this workbook, by content hash prefix shown by the
//!                    corpus example
//!   --limit <n>      how many hits to show, default 10
//!   --reindex        rebuild every workbook's documents, even if indexed
//!
//! Build the corpus first:
//!
//! ```sh
//! cargo run --release -p eg-graph --example corpus -- index private/book.xlsb
//! cargo run --release -p eg-index --example search -- index revenue
//! ```
//!
//! Prints node labels — sheet names, table titles, column headers, defined
//! names — and A1 ranges. Never cell values, so it is safe against a
//! confidential workbook.
//!
//! The numbers to watch are the index size against the corpus size, and query
//! latency. If searching is not far cheaper than walking the graphs, there is
//! no reason for an index to exist.

use std::time::Instant;

use eg_graph::store::Corpus;
use eg_graph::NodeKind;
use eg_index::{SearchOptions, TextIndex};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("usage: search <corpus-dir> [--kind k] [--sheet s] [--workbook h] [--limit n] [--reindex] <query>...");
        std::process::exit(2);
    };

    let mut opts = SearchOptions::default();
    let mut reindex = false;
    let mut words: Vec<String> = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--reindex" => reindex = true,
            "--kind" => match args.next().as_deref().and_then(parse_kind) {
                Some(k) => opts.kinds.push(k),
                None => {
                    eprintln!("--kind wants one of: {}", kind_list());
                    std::process::exit(2);
                }
            },
            "--sheet" => opts.sheet = args.next(),
            "--workbook" => opts.workbook = args.next(),
            "--limit" => {
                opts.limit = args.next().and_then(|n| n.parse().ok()).unwrap_or(10);
            }
            _ => words.push(arg),
        }
    }
    let query = words.join(" ");

    let corpus = match Corpus::open(&dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not open corpus: {e}");
            std::process::exit(1);
        }
    };
    let mut index = match TextIndex::open(&dir) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("could not open index: {e}");
            std::process::exit(1);
        }
    };

    if corpus.is_empty() {
        eprintln!("corpus at {dir} is empty — index a workbook with the eg-graph corpus example");
        std::process::exit(1);
    }

    // A workbook already indexed is skipped, because the index is keyed by the
    // same content hash the corpus is: if the file had changed, its hash would
    // have and it would not be found here at all.
    let existing = index.len().unwrap_or(0);
    let hashes: Vec<String> = corpus.entries().map(|(h, _)| h.to_string()).collect();
    let building = Instant::now();
    let mut documents = 0usize;
    let mut indexed = 0usize;
    if reindex || existing == 0 {
        for hash in &hashes {
            let stored = match corpus.get(hash) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    eprintln!("{}: listed in the manifest but not stored", short(hash));
                    continue;
                }
                Err(e) => {
                    eprintln!("{}: {e}", short(hash));
                    continue;
                }
            };
            match index.index_stored(&stored) {
                Ok(n) => {
                    documents += n;
                    indexed += 1;
                }
                Err(e) => eprintln!("{}: {e}", short(hash)),
            }
        }
    }
    let build_time = building.elapsed();

    println!("corpus at {dir}: {} workbook(s)", corpus.len());
    if indexed > 0 {
        println!(
            "  indexed {indexed} workbook(s), {documents} documents in {:.2}s",
            build_time.as_secs_f64()
        );
    } else {
        println!("  already indexed — pass --reindex to rebuild");
    }
    println!(
        "  {} documents, {:.1} MiB on disk at {}",
        index.len().unwrap_or(0),
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
            "  {:>6.2}  {:<16} {:<40} {}",
            hit.score,
            hit.kind.as_str(),
            truncate(&hit.label, 40),
            hit.a1
                .as_deref()
                .unwrap_or_else(|| hit.sheet.as_deref().unwrap_or(""))
        );
    }
    if hits.is_empty() {
        println!("  nothing matched");
    }
}

/// The first few characters of a hash, by character and not by byte, so an
/// unexpected manifest key cannot panic the error path that reports it.
fn short(hash: &str) -> String {
    hash.chars().take(8).collect()
}

fn parse_kind(arg: &str) -> Option<NodeKind> {
    NodeKind::parse(&arg.replace(['-', '_'], " ").to_lowercase())
}

fn kind_list() -> String {
    NodeKind::ALL
        .iter()
        .map(|k| k.as_str().replace(' ', "-"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    format!("{}…", s.chars().take(n - 1).collect::<String>())
}
