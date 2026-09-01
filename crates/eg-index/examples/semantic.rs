//! Search a corpus lexically, semantically, and both at once — side by side.
//!
//! Usage: `semantic <corpus-dir> [options] <query>...`
//!
//!   --kind <k>       only these node kinds; repeatable
//!   --sheet <name>   only nodes on this sheet
//!   --limit <n>      how many hits per list, default 8
//!   --reindex        re-embed every workbook, even if it has vectors
//!
//! Three lists for one question is the point. The lexical index cannot reach a
//! column whose header shares no word with the question; the vector index
//! cannot reliably reach a sheet whose name is an identifier. Where the two
//! lists differ is the argument for running both.
//!
//! ```sh
//! cargo run --release -p eg-graph --example corpus -- index private/book.xlsb
//! cargo run --release -p eg-index --example semantic -- index bad debt written off
//! ```
//!
//! The first run downloads the embedding model, about 130 MB, cached per user.
//! Everything after that is local — no workbook text leaves the machine.
//!
//! Prints node labels and A1 ranges, never cell values.

use std::time::Instant;

use eg_graph::store::Corpus;
use eg_graph::NodeKind;
use eg_index::vector::{embeddable_with, VectorIndex};
use eg_index::{fuse, Embedder, Hit, SearchOptions, TextIndex};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("usage: semantic <corpus-dir> [--kind k] [--sheet s] [--limit n] [--reindex] <query>...");
        std::process::exit(2);
    };

    let mut opts = SearchOptions {
        limit: 8,
        ..Default::default()
    };
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
            "--limit" => match args.next().as_deref().and_then(|n| n.parse::<usize>().ok()) {
                // At least one: both searches clamp to one internally, so a
                // limit of zero would print two hits and an empty hybrid list.
                Some(n) => opts.limit = n.max(1),
                None => {
                    eprintln!("--limit wants a number");
                    std::process::exit(2);
                }
            },
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
    if corpus.is_empty() {
        eprintln!("corpus at {dir} is empty — index a workbook with the eg-graph corpus example");
        std::process::exit(1);
    }

    let loading = Instant::now();
    let mut embedder = match Embedder::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("could not load the embedding model: {e}");
            std::process::exit(1);
        }
    };
    let model_time = loading.elapsed();

    let mut text = match TextIndex::open(&dir) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("could not open the lexical index: {e}");
            std::process::exit(1);
        }
    };
    let mut vectors = match VectorIndex::open(&dir, embedder.name(), embedder.dim()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("could not open the vector index: {e}");
            std::process::exit(1);
        }
    };

    println!("corpus at {dir}: {} workbook(s)", corpus.len());
    println!(
        "  model {} ({} dims), loaded in {:.2}s",
        embedder.name(),
        embedder.dim(),
        model_time.as_secs_f64()
    );

    let hashes: Vec<String> = corpus.entries().map(|(h, _)| h.to_string()).collect();
    let mut embedded = 0usize;
    let mut lexical_docs = 0usize;
    // Timed apart from everything around it. Loading a graph and building the
    // lexical index are not embedding, and folding them into one number would
    // report a model throughput that is really a measure of tantivy.
    let mut embed_time = std::time::Duration::ZERO;
    let mut lexical_time = std::time::Duration::ZERO;

    for hash in &hashes {
        // The two indexes are asked separately. The lexical one rebuilds itself
        // empty on a schema change, so inferring its contents from the vector
        // index would mean silently searching nothing.
        let want_text = reindex || !text.contains(hash).unwrap_or(false);
        let want_vectors = reindex || !vectors.contains(hash);
        if !want_text && !want_vectors {
            continue;
        }
        let stored = match corpus.get(hash) {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("{}: {e}", short(hash));
                continue;
            }
        };

        // The same profiles `eg index` indexes, so this measures the index
        // that verb builds rather than a smaller one — and so that running
        // this over a corpus does not quietly strip the values out of an
        // index that had them.
        let profiles = corpus.profiles(hash).unwrap_or_default();

        if want_text {
            let at = Instant::now();
            match text.index_stored_with(&stored, profiles.as_ref()) {
                Ok(n) => lexical_docs += n,
                Err(e) => eprintln!("{}: {e}", short(hash)),
            }
            lexical_time += at.elapsed();
        }

        if want_vectors {
            let docs = embeddable_with(&stored.graph, profiles.as_ref());
            let at = Instant::now();
            let made = match embedder.embed_documents(&docs) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("{}: {e}", short(hash));
                    continue;
                }
            };
            embed_time += at.elapsed();
            match vectors.put(hash, &stored.path, &docs, &made) {
                Ok(n) => embedded += n,
                Err(e) => eprintln!("{}: {e}", short(hash)),
            }
        }
    }

    if lexical_docs > 0 {
        println!(
            "  indexed {lexical_docs} lexical documents in {:.2}s",
            lexical_time.as_secs_f64()
        );
    }
    if embedded > 0 {
        println!(
            "  embedded {embedded} nodes in {:.2}s — {:.0} nodes/s",
            embed_time.as_secs_f64(),
            embedded as f64 / embed_time.as_secs_f64().max(1e-9)
        );
    }
    if embedded == 0 && lexical_docs == 0 {
        println!("  already indexed — pass --reindex to rebuild");
    }
    println!(
        "  {} vectors over {} workbook(s), {:.1} MiB at {}",
        vectors.len(),
        vectors.workbooks(),
        vectors.size_on_disk() as f64 / (1024.0 * 1024.0),
        vectors.path().display()
    );

    if query.trim().is_empty() {
        println!("\nno query given");
        return;
    }

    let lexical_at = Instant::now();
    let lexical = text.search(&query, &opts).unwrap_or_else(|e| {
        eprintln!("lexical search failed: {e}");
        Vec::new()
    });
    let lexical_search = lexical_at.elapsed();

    let embed_at = Instant::now();
    let q = match embedder.embed_query(&query) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("could not embed the query: {e}");
            std::process::exit(1);
        }
    };
    let embed_time = embed_at.elapsed();

    let scan_at = Instant::now();
    let semantic = vectors.search(&q, &opts);
    let scan_time = scan_at.elapsed();

    let fused = fuse(&[&lexical, &semantic], opts.limit);

    println!("\n{query:?}");
    show(
        &format!("lexical ({:.2}ms)", lexical_search.as_secs_f64() * 1000.0),
        &lexical,
    );
    show(
        &format!(
            "semantic ({:.2}ms embed + {:.2}ms scan of {} vectors)",
            embed_time.as_secs_f64() * 1000.0,
            scan_time.as_secs_f64() * 1000.0,
            vectors.len()
        ),
        &semantic,
    );
    show("hybrid (reciprocal rank fusion)", &fused);
}

fn show(title: &str, hits: &[Hit]) {
    println!("\n  {title}");
    if hits.is_empty() {
        println!("    nothing matched");
        return;
    }
    for hit in hits {
        println!(
            "    {:>7.4}  {:<16} {:<38} {}",
            hit.score,
            hit.kind.as_str(),
            truncate(&hit.label, 38),
            hit.a1.as_deref().unwrap_or_default()
        );
    }
}

/// The first few characters of a hash, for an error line.
///
/// By character and not by byte: the manifest is a file on disk, and slicing a
/// string from one at a fixed byte offset is a panic waiting for the first
/// entry that is not what we assumed.
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
