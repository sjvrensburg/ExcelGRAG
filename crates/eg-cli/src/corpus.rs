//! The verbs that work on a corpus: building one, asking it a question, and
//! serving it.

use std::time::Instant;

use eg_graph::store::Corpus;
use eg_graph::{build_with, AuditOptions, GraphOptions, NodeKind, MAX_STORED_FORMULA_GROUPS};
use eg_index::vector::{embeddable, VectorIndex};
use eg_index::{Embedder, SearchOptions, TextIndex};
use eg_ingest::{load_with, LoadOptions};
use eg_retrieve::{expand, find, render, ExpandOptions, Fusion, RenderOptions};
use eg_structure::{detect_regions, profile_table, read_table, ProfileOptions, Profiles};

/// Read workbooks into the corpus, then bring both indexes up to date.
///
/// The three stages are separable on purpose. Storing a graph is the expensive,
/// once-per-workbook part; the lexical index is cheap; the vector index needs a
/// model that may not be reachable, and a corpus with only the first two still
/// answers questions by word.
pub fn index(
    dir: &str,
    workbooks: &[String],
    reindex: bool,
    lexical_only: bool,
    profile: bool,
    redact_values: bool,
) -> Result<(), String> {
    let mut corpus = Corpus::open(dir).map_err(|e| format!("could not open the corpus: {e}"))?;
    // Storing is per-workbook and can fail partway through a list — a bad path
    // in workbook 3 of 5 must not read as "nothing happened" when 1 and 2 are
    // already on disk. `completed` is what to tell the caller when that
    // happens: a rerun of the same command is harmless (storage is keyed by
    // content hash) and finishes the job.
    let mut completed: Vec<&str> = Vec::new();

    for path in workbooks {
        let stored: Result<(), String> = (|| {
            let started = Instant::now();
            let loaded = load_with(
                path,
                &LoadOptions {
                    max_cells: None,
                    ..Default::default()
                },
            )
            .map_err(|e| format!("could not load {path}: {e}"))?;
            let read = started.elapsed();

            // The formula-group layer is stored with the rest, because rebuilding
            // it costs a full ingest of the source file and keeping it costs a few
            // hundred kilobytes. A workbook of one-off formulas groups into nothing
            // and would blow that out, so past the budget the layer is dropped and
            // goes back to being rebuilt on demand.
            let mut built = build_with(
                &loaded.workbook,
                &GraphOptions {
                    formula_group_nodes: true,
                    ..Default::default()
                },
            );
            let groups = built.report.nodes_of(NodeKind::FormulaGroup) as usize;
            let stored_groups = groups <= MAX_STORED_FORMULA_GROUPS;
            if !stored_groups {
                // Taken off the graph already in hand, not rebuilt without it:
                // the two differ only in these nodes, and building again would
                // repeat region detection and the whole dependency lift to
                // arrive at the same place.
                built.drop_formula_groups();
            }
            // Profiles are the workbook's *data* — distinct values, sums — where
            // everything else stored is structure, so they are written separately
            // and are separately refusable. `--no-profiles` skips them entirely;
            // `--redact-values` keeps the counts and types and drops what came out
            // of the cells.
            let profiles = if profile {
                let opts = ProfileOptions {
                    values: !redact_values,
                    ..Default::default()
                };
                let mut columns = Vec::new();
                for sheet in &loaded.workbook.sheets {
                    for region in detect_regions(sheet) {
                        if let Some(table) = read_table(sheet, &region) {
                            columns.extend(profile_table(sheet, &table, &opts));
                        }
                    }
                }
                Some(Profiles {
                    columns,
                    values: opts.values,
                })
            } else {
                None
            };

            // Checked before it is stored, not when someone remembers to run the
            // example. The structural invariants are free; the audit re-derives
            // every dependency edge from the cells, which is a pass over the
            // workbook's formulas — 2.7s against the 17s already spent getting
            // here, and the only thing that catches an edge lifted to the wrong
            // region. A workbook is still stored when it fails: the finding is
            // about this code, and a corpus missing an edge is more use than no
            // corpus at all. It is said loudly instead.
            let violations = eg_graph::check(&built);
            let audit = eg_graph::audit(&loaded.workbook, &built.graph, &AuditOptions::default());

            // Stored absolute: an MCP client starts `eg-mcp`/`eg serve` with its
            // own working directory, documented as not necessarily this one, so a
            // relative path written here would resolve against the wrong
            // directory the moment a cell-level tool tried to open it. Falls back
            // to the path as typed only if the file has, oddly, already vanished
            // before it could even be canonicalized.
            let stored_path = std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.clone());

            corpus
                .put(
                    &loaded.workbook.content_hash,
                    &stored_path,
                    loaded.workbook.sheets.len(),
                    loaded.workbook.total_cells() as u64,
                    stored_groups,
                    &built,
                )
                .map_err(|e| format!("could not store {path}: {e}"))?;
            if let Some(profiles) = &profiles {
                corpus
                    .put_profiles(&loaded.workbook.content_hash, &stored_path, profiles)
                    .map_err(|e| format!("could not store profiles for {path}: {e}"))?;
            } else {
                // Absence is authoritative: a re-index without `--profile` must not
                // leave an earlier run's value-bearing profiles file (and manifest
                // flags) on disk claiming to be current.
                corpus
                    .forget_profiles(&loaded.workbook.content_hash)
                    .map_err(|e| format!("could not clear stale profiles for {path}: {e}"))?;
            }

            println!(
                "{path}\n  {} sheets, {} cells read in {:.1}s → {} nodes, {} edges in {:.1}s",
                loaded.workbook.sheets.len(),
                loaded.workbook.total_cells(),
                read.as_secs_f64(),
                built.graph.node_count(),
                built.graph.edge_count(),
                started.elapsed().as_secs_f64() - read.as_secs_f64(),
            );
            if !stored_groups {
                println!(
                "  {groups} formula groups is past the {MAX_STORED_FORMULA_GROUPS} the store keeps; \
                 they will be rebuilt on demand"
            );
            }
            if let Some(profiles) = &profiles {
                let categorical = profiles.categorical().count();
                println!(
                    "  {} column(s) profiled, {categorical} of them categorical{}",
                    profiles.len(),
                    if profiles.values {
                        ""
                    } else {
                        " (counts and types only — values redacted)"
                    }
                );
            }
            if violations.is_empty() && audit.agrees() {
                println!(
                    "  {} lifted edges agree with the cells they came from ({:.1}s)",
                    audit.edges_agreed,
                    audit.audit_time.as_secs_f64()
                );
            }
            for violation in &violations {
                println!("  BROKEN: {} — {}", violation.invariant, violation.detail);
            }
            for finding in &audit.findings {
                println!("  BROKEN: {} — {}", finding.kind.as_str(), finding.detail);
            }
            if audit.findings_total as usize > audit.findings.len() {
                println!(
                    "  ... and {} more findings not shown",
                    audit.findings_total as usize - audit.findings.len()
                );
            }
            for warning in &loaded.warnings {
                println!("  warning: {warning}");
            }
            Ok(())
        })();
        if let Err(e) = stored {
            if !completed.is_empty() {
                eprintln!(
                    "eg index: stored {} of {} workbook(s) before this failure: {}",
                    completed.len(),
                    workbooks.len(),
                    completed.join(", ")
                );
                eprintln!(
                    "eg index: re-running the same `eg index` command is safe — storage is \
                     keyed by content hash, so it will pick up where this left off"
                );
            }
            return Err(e);
        }
        completed.push(path);
    }

    let mut text = TextIndex::open_or_reset(dir)
        .map_err(|e| format!("could not open the lexical index: {e}"))?;
    // The model is loaded only if something needs embedding, because loading it
    // is seconds and a re-run with nothing to do should cost nothing.
    let mut embedder: Option<(Embedder, VectorIndex)> = None;
    // Once, not once per workbook: loading the model is seconds even when it
    // fails, and a machine that cannot reach it will not reach it on the
    // second workbook either.
    let mut no_embedder = lexical_only;
    let hashes: Vec<String> = corpus.entries().map(|(hash, _)| hash.to_string()).collect();
    let (mut lexical_docs, mut embedded) = (0usize, 0usize);

    for hash in &hashes {
        // The two indexes are asked separately: the lexical one rebuilds itself
        // empty on a schema change, so inferring its contents from the vector
        // index would mean silently searching nothing.
        let want_text = reindex || !text.contains(hash).unwrap_or(false);
        let want_vectors = !no_embedder
            && (reindex
                || match &embedder {
                    Some((_, vectors)) => !vectors.contains(hash),
                    None => true,
                });
        if !want_text && !want_vectors {
            continue;
        }
        let Some(stored) = corpus
            .get(hash)
            .map_err(|e| format!("could not read the stored graph {}: {e}", short(hash)))?
        else {
            continue;
        };

        if want_text {
            lexical_docs += text
                .index_stored(&stored)
                .map_err(|e| format!("could not index {}: {e}", short(hash)))?;
        }

        if want_vectors {
            if embedder.is_none() {
                match eg_retrieve::embedder(dir) {
                    Ok(pair) => embedder = Some(pair),
                    Err(e) => {
                        println!("  no semantic half ({e}); indexing by word only");
                        no_embedder = true;
                        continue;
                    }
                }
            }
            let Some((embedder, vectors)) = embedder.as_mut() else {
                continue;
            };
            if !reindex && vectors.contains(hash) {
                continue;
            }
            let docs = embeddable(&stored.graph);
            let made = embedder
                .embed_documents(&docs)
                .map_err(|e| format!("could not embed {}: {e}", short(hash)))?;
            embedded += vectors
                .put(hash, &stored.path, &docs, &made)
                .map_err(|e| format!("could not store vectors for {}: {e}", short(hash)))?;
        }
    }

    println!(
        "\ncorpus at {dir}: {} workbook(s), {lexical_docs} lexical document(s) and {embedded} vector(s) added",
        corpus.len()
    );
    Ok(())
}

/// The defaults, with the semantic half turned off on request.
fn fusion(lexical_only: bool) -> Fusion {
    Fusion {
        lexical_only,
        ..Default::default()
    }
}

/// Refuses a directory that has never been indexed, rather than letting a
/// read-only verb materialize an empty corpus and answer NOTHING MATCHED —
/// indistinguishable from a real miss, and the usual cause is a typo'd path.
/// Only `index` should ever create a corpus; every other verb calls this
/// first.
fn require_corpus(dir: &str) -> Result<(), String> {
    if std::path::Path::new(dir).join("manifest.json").exists() {
        return Ok(());
    }
    Err(format!(
        "no corpus at {dir} (no manifest.json) — check the path, or run `eg index {dir} <workbook>` first"
    ))
}

/// Resolves `--workbook` the way MCP's `context` tool does: a hash prefix, a
/// full stored path, or a bare filename, whichever picks out exactly one
/// workbook. `None` (no filter given) is passed straight through so `ask` and
/// `search` keep searching the whole corpus by default.
fn resolve_workbook(corpus: &Corpus, wanted: Option<&str>) -> Result<Option<String>, String> {
    let Some(wanted) = wanted else {
        return Ok(None);
    };
    let matches: Vec<&str> = corpus
        .entries()
        .filter(|(hash, entry)| {
            hash.starts_with(wanted)
                || entry.path == wanted
                || std::path::Path::new(&entry.path)
                    .file_name()
                    .is_some_and(|n| n == wanted)
        })
        .map(|(hash, _)| hash)
        .collect();
    match matches.as_slice() {
        [one] => Ok(Some((*one).to_string())),
        [] => Err(format!("no workbook in this corpus matches {wanted:?}")),
        many => Err(format!(
            "{wanted:?} matches {} workbooks; be more specific",
            many.len()
        )),
    }
}

pub struct AskOptions {
    pub seeds: usize,
    pub hops: usize,
    pub budget: usize,
    pub children: usize,
    pub chars: usize,
    pub workbook: Option<String>,
    pub sheet: Option<String>,
    pub lexical_only: bool,
}

pub fn ask(dir: &str, query: &str, options: AskOptions) -> Result<(), String> {
    require_corpus(dir)?;
    let corpus = Corpus::open(dir).map_err(|e| format!("could not open the corpus: {e}"))?;
    let workbook = resolve_workbook(&corpus, options.workbook.as_deref())?;
    let search_options = SearchOptions {
        limit: options.seeds.max(1),
        workbook,
        sheet: options.sheet,
        ..Default::default()
    };
    let found = find(dir, query, &search_options, &fusion(options.lexical_only))
        .map_err(|e| e.to_string())?;
    if let Some(warning) = found.warning() {
        println!("{warning}\n");
    }
    if found.is_empty() {
        return Ok(());
    }
    // Always, above the passage. The failure this fixes is that a passage which
    // missed the right table read exactly like one that found it.
    println!("Matched: {}\n", found.evidence());
    let hits = found.hits;

    let found = expand(
        &corpus,
        &hits,
        &ExpandOptions {
            hops: options.hops,
            budget: options.budget.max(1),
            children: options.children,
            ..Default::default()
        },
    )
    .map_err(|e| format!("expansion failed: {e}"))?;
    let rendered = render(
        &found,
        &RenderOptions {
            max_chars: options.chars.max(200),
            ..Default::default()
        },
    );

    print!("{}", rendered.text);
    println!(
        "\n---\n{} node(s) from {} seed(s), {} citation(s){}",
        found.total_nodes(),
        hits.len(),
        rendered.citations.len(),
        if rendered.omitted > 0 {
            format!(", {} omitted to fit", rendered.omitted)
        } else {
            String::new()
        }
    );
    for hash in &found.missing_workbooks {
        println!(
            "{}: matched in the index but is not in the corpus — re-run `eg index`",
            short(hash)
        );
    }
    for workbook in &found.workbooks {
        if !workbook.stale_seeds.is_empty() {
            println!(
                "{}: {} seed(s) the index ranked no longer exist in the stored graph \
                 — re-run `eg index` to refresh it",
                short(&workbook.content_hash),
                workbook.stale_seeds.len()
            );
        }
    }
    Ok(())
}

pub fn search(
    dir: &str,
    query: &str,
    limit: usize,
    workbook: Option<String>,
    sheet: Option<String>,
    lexical_only: bool,
) -> Result<(), String> {
    require_corpus(dir)?;
    let corpus = Corpus::open(dir).map_err(|e| format!("could not open the corpus: {e}"))?;
    let workbook = resolve_workbook(&corpus, workbook.as_deref())?;
    let options = SearchOptions {
        limit: limit.max(1),
        workbook,
        sheet,
        ..Default::default()
    };
    let found = find(dir, query, &options, &fusion(lexical_only)).map_err(|e| e.to_string())?;
    if let Some(warning) = found.warning() {
        println!("{warning}\n");
    }
    if found.is_empty() {
        return Ok(());
    }
    // `search` is the diagnostic verb, so it says how it did even when there is
    // no warning: the counts are what a person tuning a query needs.
    println!("match: {} — {}", found.verdict().as_str(), found.evidence());
    let hits = found.hits;
    println!("{} hit(s) for {query:?}", hits.len());
    for hit in &hits {
        println!("  {:.2}  {:<8} {}", hit.score, hit.kind.as_str(), hit.label);
        if let Some(a1) = &hit.a1 {
            println!("          {a1}");
        }
    }
    Ok(())
}

pub fn workbooks(dir: &str) -> Result<(), String> {
    require_corpus(dir)?;
    let corpus = Corpus::open(dir).map_err(|e| format!("could not open the corpus: {e}"))?;
    if corpus.is_empty() {
        println!("The corpus at {dir} is empty. Add a workbook with `eg index`.");
        return Ok(());
    }
    println!("{} workbook(s) at {dir}", corpus.len());
    for (hash, entry) in corpus.entries() {
        println!("  {}  {}", short(hash), entry.path);
        println!(
            "    {} sheets, {} cells, {} nodes, {} edges",
            entry.sheets, entry.cells, entry.nodes, entry.edges
        );
    }
    Ok(())
}

pub fn serve(dir: &str, redact_values: bool) -> Result<(), String> {
    require_corpus(dir)?;
    let state = eg_mcp::State::open(dir, redact_values)?;
    eprintln!(
        "eg serve: {} workbook(s) from {dir}{}",
        state.corpus.len(),
        if redact_values {
            ", values redacted"
        } else {
            ""
        }
    );
    let mut server = eg_mcp::Server::new(state);
    let stdin = std::io::stdin();
    eg_mcp::serve(&mut server, stdin.lock(), std::io::stdout()).map_err(|e| e.to_string())
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}
