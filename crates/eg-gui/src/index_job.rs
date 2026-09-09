//! Indexing a workbook, from the GUI.
//!
//! This is `eg index` for one workbook, restated so its progress is a stream
//! of log lines over the WebSocket rather than stdout. The stages and their
//! order are the CLI's, because a corpus built here must be the same corpus
//! `eg ask` would answer from: load, build, profile, store, index by word,
//! index by meaning.
//!
//! What is deliberately *not* here is the `check`/`audit` pass the CLI runs.
//! It re-derives every dependency edge from the cells and exists to catch a
//! bug in the lifting code — a developer's diagnostic, at a measurable share
//! of the indexing time. The GUI indexes; `eg check` audits.

use std::sync::Arc;
use std::time::Instant;

use eg_graph::{build_with, GraphOptions, NodeKind, MAX_STORED_FORMULA_GROUPS};
use eg_index::vector::embeddable_with;
use eg_index::TextIndex;
use eg_ingest::LoadOptions;
use eg_structure::{detect_regions, profile_table, read_table, ProfileOptions, Profiles};
use serde::Deserialize;

use crate::app::App;
use crate::dto::WsEvent;

#[derive(Deserialize, Clone)]
pub struct IndexBody {
    /// The workbook to index, as the user named it.
    pub path: String,
    /// Skip the embedding model; index by word only.
    #[serde(default)]
    pub lexical_only: bool,
    /// Profile the columns. On by default; `--redact-values` at startup
    /// governs whether the profiles keep values, not this flag.
    #[serde(default = "default_true")]
    pub profiles: bool,
}

fn default_true() -> bool {
    true
}

pub fn run(app: &Arc<App>, body: &IndexBody) -> Result<(), String> {
    let log = |line: String| app.send(WsEvent::Log { line });
    let started = Instant::now();

    // --- Load -------------------------------------------------------------
    let loaded = eg_ingest::load_with(
        &body.path,
        &LoadOptions {
            max_cells: None,
            ..Default::default()
        },
    )
    .map_err(|e| format!("could not load {}: {e}", body.path))?;
    log(format!(
        "{}: {} sheet(s), {} cell(s) read in {:.1}s",
        body.path,
        loaded.workbook.sheets.len(),
        loaded.workbook.total_cells(),
        started.elapsed().as_secs_f64()
    ));
    for warning in &loaded.warnings {
        log(format!("  warning: {warning}"));
    }

    // --- Build ------------------------------------------------------------
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
        built.drop_formula_groups();
    }
    log(format!(
        "graph built: {} node(s), {} edge(s) in {:.1}s",
        built.graph.node_count(),
        built.graph.edge_count(),
        started.elapsed().as_secs_f64()
    ));

    // --- Profiles ---------------------------------------------------------
    let profiles: Option<Profiles> = if body.profiles {
        let opts = ProfileOptions {
            values: !app.redact_values,
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

    // --- Store ------------------------------------------------------------
    // Absolute, as `eg index` stores it: the corpus outlives whatever
    // working directory this server was started from.
    let stored_path = std::fs::canonicalize(&body.path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| body.path.clone());
    let hash = loaded.workbook.content_hash.clone();
    {
        let mut state = app.engine();
        state
            .corpus
            .put(
                &hash,
                &stored_path,
                loaded.workbook.sheets.len(),
                loaded.workbook.total_cells() as u64,
                stored_groups,
                &built,
            )
            .map_err(|e| format!("could not store {}: {e}", body.path))?;
        match &profiles {
            Some(profiles) => state
                .corpus
                .put_profiles(&hash, &stored_path, profiles)
                .map_err(|e| format!("could not store the profiles for {}: {e}", body.path))?,
            // Absence is authoritative: re-indexing without profiles must
            // not leave an earlier run's value-bearing file claiming to be
            // current.
            None => state
                .corpus
                .forget_profiles(&hash)
                .map_err(|e| format!("could not clear stale profiles for {}: {e}", body.path))?,
        }
    }
    log("graph stored".to_string());

    // --- Lexical index ------------------------------------------------------
    // Read back through the store rather than carrying `built` over: the
    // text index wants the stored shape, and this is the same file the
    // indexes will serve.
    let (stored, profiles) = {
        let state = app.engine();
        let stored = state
            .corpus
            .get(&hash)
            .map_err(|e| format!("could not read back the stored graph: {e}"))?
            .ok_or_else(|| "the graph vanished between storing and indexing it".to_string())?;
        let profiles = state
            .corpus
            .profiles(&hash)
            .map_err(|e| format!("could not read back the profiles: {e}"))?;
        (stored, profiles)
    };
    let mut text = TextIndex::open_or_reset(&app.dir)
        .map_err(|e| format!("could not open the lexical index: {e}"))?;
    // Unconditional, not gated on `!text.contains(&hash)`: this job just
    // loaded and built the workbook fresh, so it is always "refreshed" in
    // `eg index`'s sense (crates/eg-cli/src/corpus.rs's `refreshed` set) —
    // the one case that must always rewrite the documents rather than skip
    // an already-present hash, or a later `--redact-values` re-index of an
    // unchanged file would leave the earlier run's unredacted values behind
    // in `text/` (see CLAUDE.md, "Confidential workbooks").
    let documents = text
        .index_stored_with(&stored, profiles.as_ref())
        .map_err(|e| format!("could not index {}: {e}", body.path))?;
    log(format!("{documents} lexical document(s) added"));

    // --- Vector index -----------------------------------------------------
    if !body.lexical_only {
        match eg_retrieve::embedder(&app.dir) {
            Ok((mut embedder, mut vectors)) => {
                // Also unconditional, for the same reason as the lexical
                // half above.
                let docs = embeddable_with(&stored.graph, profiles.as_ref());
                let made = embedder
                    .embed_documents(&docs)
                    .map_err(|e| format!("could not embed {}: {e}", body.path))?;
                let stored_count = vectors
                    .put(&hash, &stored.path, &docs, &made)
                    .map_err(|e| format!("could not store the vectors for {}: {e}", body.path))?;
                log(format!("{stored_count} vector(s) added"));
            }
            Err(e) => {
                // A corpus with no semantic half still answers by word; say
                // so once and carry on, exactly as `eg index` does.
                log(format!("no semantic half ({e}); indexed by word only"));
            }
        }
    }

    // --- Rotate ------------------------------------------------------------
    app.rotate_engine()?;
    log(format!(
        "done in {:.1}s — {} now searchable",
        started.elapsed().as_secs_f64(),
        body.path
    ));
    Ok(())
}
