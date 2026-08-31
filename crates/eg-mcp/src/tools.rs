//! The tools this server offers, and what each one answers.
//!
//! The surface follows the shape of the pipeline underneath it, because that is
//! also the shape of how a question gets answered: find the part of the
//! workbook that matches, read the context around it, then go down to cells
//! when the answer needs a number.
//!
//! Each tool returns text, because text is what lands in the caller's context.
//! The passage renderer already numbers its nodes and hands back citations, so
//! an answer built from these can be checked against the workbook rather than
//! taken on trust — which is the point of the whole stack.
//!
//! Values are the workbook's data. `--redact-values` at startup replaces every
//! one with its kind, so a server pointed at a confidential file can still be
//! asked about structure. The policy lives at startup rather than per call:
//! a caller cannot talk its way past it.

use eg_eval::whatif::{what_if, Blocked, Change, WhatIfOptions};
use eg_eval::{
    cell as cell_fact, cells_in, dependents_of, precedents_of, recompute, Outcome, Target,
};
use eg_index::SearchOptions;
use eg_model::{parse_a1, CellValue, RangeRef, Workbook};
use eg_retrieve::{expand, find_in, render, ExpandOptions, Fusion, RenderOptions, Search};
use serde_json::{json, Value};

use crate::state::State;

/// A tool's declaration, as `tools/list` reports it.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
}

pub const TOOLS: &[Tool] = &[
    Tool {
        name: "workbooks",
        description: "List the workbooks in this corpus: where each came from, how big it is, \
                      and the content hash that names it in every other tool.",
        schema: || json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    },
    Tool {
        name: "search",
        description: "Find the parts of a workbook that match a question, by word and by \
                      meaning. Returns ranked nodes — sheets, tables, columns — with a citation \
                      for each. A hit is a door, not an answer: follow it with `context`.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to look for, in words." },
                    "limit": { "type": "integer", "description": "How many hits, default 8." },
                    "workbook": { "type": "string", "description": "Restrict to one workbook (content hash, path or file name)." },
                    "sheet": { "type": "string", "description": "Restrict to one sheet, by exact name." },
                    "lexical_only": { "type": "boolean", "description": "Skip the embedding model and match by word alone." }
                },
                "required": ["query"],
                "additionalProperties": false
            })
        },
    },
    Tool {
        name: "context",
        description: "Answer a question about a workbook with a cited passage: searches, walks \
                      the graph out to what explains the hits, and renders it. Every node is \
                      numbered and every relation names the node it came from, so the passage \
                      can be checked. Contains no cell values — it says where to look.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The question, in words." },
                    "seeds": { "type": "integer", "description": "How many hits to expand from, default 5." },
                    "hops": { "type": "integer", "description": "Dependency hops from a seed, default 2." },
                    "budget": { "type": "integer", "description": "Most nodes per workbook, default 40." },
                    "children": { "type": "integer", "description": "Contained children to show per node, default 0." },
                    "max_chars": { "type": "integer", "description": "Ceiling on the passage, default 8000." },
                    "lexical_only": { "type": "boolean" }
                },
                "required": ["query"],
                "additionalProperties": false
            })
        },
    },
    Tool {
        name: "read_cells",
        description: "Read the cells of a range: their formulas, and their values unless this \
                      server was started with values redacted. Opening a workbook takes seconds \
                      the first time and is then held open.",
        schema: || cell_schema("The range to read, e.g. \"'Q3 Sales'!B2:D40\"."),
    },
    Tool {
        name: "precedents",
        description: "The cells a formula reads, resolved but not followed. Cheap: the answer is \
                      in the formula's own text.",
        schema: || cell_schema("The cell or range whose formulas to read, e.g. \"Sheet1!D7\"."),
    },
    Tool {
        name: "dependents",
        description: "The cells whose formulas read a range. Expensive: nothing records who \
                      reads a cell, so every formula in the workbook is scanned — seconds on a \
                      large file, not milliseconds.",
        schema: || cell_schema("The range to find readers of."),
    },
    Tool {
        name: "recompute",
        description: "Recompute a formula from the stored values of the cells it reads, and say \
                      whether that agrees with the value the workbook has. Reports the inputs it \
                      used, so the verdict can be checked. What it cannot model it refuses by \
                      name rather than guessing.",
        schema: || cell_schema("The cell or range to recompute, e.g. \"Sheet1!D7\"."),
    },
    Tool {
        name: "what_if",
        description: "Change one or more cells and report every cell that moves because of it. \
                      Nothing is written: the workbook is read-only and the substitution lives \
                      in memory. Expensive in the same way `dependents` is — a full scan of the \
                      workbook's formulas per level of the chain — and it says what it could \
                      not answer rather than reporting those cells as unchanged.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "changes": {
                        "type": "array",
                        "description": "The substitutions to make.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "citation": { "type": "string", "description": "One cell, e.g. \"RATES!B4\"." },
                                "value": { "description": "What to put there: a number, a string, or a boolean." }
                            },
                            "required": ["citation", "value"],
                            "additionalProperties": false
                        }
                    },
                    "workbook": { "type": "string", "description": "Which workbook (content hash, path or file name). Optional when the corpus holds one." },
                    "levels": { "type": "integer", "description": "Levels of the dependency chain to follow, default 8. Each is a full scan." },
                    "limit": { "type": "integer", "description": "Most moved cells to list, default 40." }
                },
                "required": ["changes"],
                "additionalProperties": false
            })
        },
    },
];

fn cell_schema(citation: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "citation": { "type": "string", "description": citation },
            "workbook": { "type": "string", "description": "Which workbook (content hash, path or file name). Optional when the corpus holds one." },
            "limit": { "type": "integer", "description": "Most rows to return, default 40." }
        },
        "required": ["citation"],
        "additionalProperties": false
    })
}

/// Run a tool. `Err` is a message for the caller, not a protocol failure.
pub fn call(state: &mut State, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "workbooks" => workbooks(state),
        "search" => search(state, args),
        "context" => context(state, args),
        "read_cells" => read_cells(state, args),
        "precedents" => precedents(state, args),
        "dependents" => dependents(state, args),
        "recompute" => recompute_tool(state, args),
        "what_if" => what_if_tool(state, args),
        other => Err(format!("no tool called {other:?}")),
    }
}

// ---- arguments ----------------------------------------------------------

fn want_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{key} is required, as a string"))
}

fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn opt_usize(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(default)
}

fn opt_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

// ---- tools --------------------------------------------------------------

fn workbooks(state: &mut State) -> Result<String, String> {
    let mut out = String::new();
    let mut count = 0;
    for (hash, entry) in state.corpus.entries() {
        count += 1;
        out.push_str(&format!(
            "{}  {}\n    {} sheets, {} cells, {} nodes, {} edges\n",
            &hash[..hash.len().min(12)],
            entry.path,
            entry.sheets,
            entry.cells,
            entry.nodes,
            entry.edges
        ));
    }
    if count == 0 {
        return Ok("The corpus is empty. Index a workbook first.".to_string());
    }
    Ok(format!("{count} workbook(s)\n{out}"))
}

/// The search half, shared by `search` and `context`.
/// The one search, against the indexes this server keeps open.
///
/// This used to be a third copy of the pipeline, and the copy is how the
/// fusion weighting came to be missing from the surface an agent talks to.
fn find(state: &mut State, query: &str, opts: &SearchOptions, lexical_only: bool) -> Search {
    let fusion = Fusion {
        lexical_only,
        ..Default::default()
    };
    let (text, mut held) = state.halves();
    if lexical_only {
        held = None;
    }
    let semantic = held
        .as_mut()
        .map(|(embedder, vectors)| (&mut **embedder, &*vectors));
    find_in(text, semantic, query, opts, &fusion)
}

fn search(state: &mut State, args: &Value) -> Result<String, String> {
    let query = want_str(args, "query")?;
    let workbook = match opt_str(args, "workbook") {
        Some(want) => Some(state.resolve(Some(&want))?.0),
        None => None,
    };
    let opts = SearchOptions {
        limit: opt_usize(args, "limit", 8).clamp(1, 100),
        workbook,
        sheet: opt_str(args, "sheet"),
        ..Default::default()
    };
    let found = find(state, &query, &opts, opt_bool(args, "lexical_only"));
    let warning = found.warning();
    let evidence = found.evidence();
    if found.is_empty() {
        return Ok(format!("{query:?} — nothing matched."));
    }
    let hits = found.hits;

    // In front of the results, not after them. A caveat below a list of answers
    // is a caveat most readers never reach.
    let mut out = String::new();
    if let Some(warning) = &warning {
        out.push_str(warning);
        out.push_str("\n\n");
    }
    out.push_str(&format!("Matched: {evidence}\n\n"));
    out.push_str(&format!("{} hit(s) for {query:?}\n", hits.len()));
    for hit in &hits {
        out.push_str(&format!(
            "  {:.2}  {:<8} {}\n",
            hit.score,
            hit.kind.as_str(),
            hit.label
        ));
        if let Some(a1) = &hit.a1 {
            out.push_str(&format!("          {a1}\n"));
        }
    }
    out.push_str(
        "\nFollow one with `context` for what explains it, or `read_cells` for the cells.\n",
    );
    Ok(out)
}

fn context(state: &mut State, args: &Value) -> Result<String, String> {
    let query = want_str(args, "query")?;
    let seeds = opt_usize(args, "seeds", 5).clamp(1, 50);
    let opts = SearchOptions {
        limit: seeds,
        ..Default::default()
    };
    let found = find(state, &query, &opts, opt_bool(args, "lexical_only"));
    let warning = found.warning();
    let evidence = found.evidence();
    if found.is_empty() {
        return Ok(format!("{query:?} — nothing matched."));
    }
    let hits = found.hits;

    let expand_opts = ExpandOptions {
        hops: opt_usize(args, "hops", 2),
        budget: opt_usize(args, "budget", 40).max(1),
        children: opt_usize(args, "children", 0),
        ..Default::default()
    };
    let found =
        expand(&state.corpus, &hits, &expand_opts).map_err(|e| format!("expansion failed: {e}"))?;
    let rendered = render(
        &found,
        &RenderOptions {
            max_chars: opt_usize(args, "max_chars", 8000).max(200),
            ..Default::default()
        },
    );

    let mut out = String::new();
    if let Some(warning) = &warning {
        out.push_str(warning);
        out.push_str("\n\n");
    }
    // Always, above the passage: an answer that missed and an answer that hit
    // used to be indistinguishable to whoever read them.
    out.push_str(&format!("Matched: {evidence}\n\n"));
    out.push_str(&rendered.text);
    out.push_str(&format!(
        "\n---\n{} node(s) from {} seed(s), {} citation(s)",
        found.total_nodes(),
        hits.len(),
        rendered.citations.len()
    ));
    if rendered.omitted > 0 {
        out.push_str(&format!(", {} omitted to fit", rendered.omitted));
    }
    for hash in &found.missing_workbooks {
        out.push_str(&format!(
            "\n{}: matched in the index but is not in the corpus — reindex",
            &hash[..hash.len().min(12)]
        ));
    }
    out.push('\n');
    Ok(out)
}

/// The three cell-level tools all start the same way: name a workbook, open it,
/// and turn a citation into a range on one of its sheets.
fn located(
    state: &mut State,
    args: &Value,
) -> Result<(std::sync::Arc<eg_ingest::Loaded>, RangeRef, String), String> {
    let citation = want_str(args, "citation")?;
    let (_, path) = state.resolve(opt_str(args, "workbook").as_deref())?;
    let (loaded, load_seconds) = state.workbook(&path)?;
    let range = resolve_range(&loaded.workbook, &citation)?;
    let note = match load_seconds {
        Some(seconds) => format!("(opened {path} in {seconds:.1}s)\n"),
        None => String::new(),
    };
    Ok((loaded, range, note))
}

fn resolve_range(workbook: &Workbook, citation: &str) -> Result<RangeRef, String> {
    let parsed = parse_a1(citation).map_err(|e| format!("{citation:?} is not an A1 range: {e}"))?;
    let Some(name) = &parsed.sheet_name else {
        return Err(format!(
            "{citation:?} names no sheet. A citation needs one, e.g. \"Sheet1!B2\"."
        ));
    };
    let id = workbook.sheet_id_by_name(name).ok_or_else(|| {
        let names: Vec<&str> = workbook.sheets.iter().map(|s| s.name.as_str()).collect();
        format!(
            "no sheet called {name:?}. This workbook has: {}",
            names.join(", ")
        )
    })?;
    Ok(parsed.resolve(id))
}

/// A value, or the shape of one when this server is not allowed to say.
fn show(value: &CellValue, redact: bool) -> String {
    if redact {
        format!("<{}>", value.kind().as_str())
    } else {
        match value {
            CellValue::Text(s) => format!("{s:?}"),
            other => other.to_display(),
        }
    }
}

fn read_cells(state: &mut State, args: &Value) -> Result<String, String> {
    let limit = opt_usize(args, "limit", 40).clamp(1, 500);
    let redact = state.redact_values;
    let (loaded, range, note) = located(state, args)?;
    let workbook = &loaded.workbook;
    let (cells, capped) = cells_in(workbook, range, limit);

    let mut out = format!(
        "{note}{} — {} populated cell(s){}\n",
        workbook.cite_range(range),
        cells.len(),
        if capped { ", capped" } else { "" }
    );
    for fact in &cells {
        out.push_str(&format!("  {:<24}", fact.a1));
        if let Some(formula) = &fact.formula {
            out.push_str(&format!(" ={formula}"));
        }
        out.push_str(&format!("  {}\n", show(&fact.value, redact)));
    }
    if cells.is_empty() {
        out.push_str("  nothing populated in that range\n");
    }
    Ok(out)
}

fn precedents(state: &mut State, args: &Value) -> Result<String, String> {
    let limit = opt_usize(args, "limit", 40).clamp(1, 500);
    let (loaded, range, note) = located(state, args)?;
    let workbook = &loaded.workbook;

    let mut out = format!("{note}{} reads\n", workbook.cite_range(range));
    let mut printed = 0;
    for (at, cell) in workbook
        .sheet(range.sheet)
        .into_iter()
        .flat_map(|sheet| sheet.iter_range(range))
    {
        if cell.formula.is_none() {
            continue;
        }
        for reference in precedents_of(workbook, at) {
            if printed >= limit {
                out.push_str("  … more, raise limit\n");
                return Ok(out);
            }
            let target = match &reference.target {
                Target::Cells(r) => workbook.cite_range(*r),
                Target::UnknownSheet(name) => format!("#REF! — no sheet called {name:?}"),
                Target::ExternalWorkbook(token) => {
                    format!("another workbook, written as [{token}]")
                }
            };
            out.push_str(&format!(
                "  {:<24} {} → {target}\n",
                workbook.cite(at),
                reference.text
            ));
            printed += 1;
        }
    }
    if printed == 0 {
        out.push_str("  nothing — no formulas in that range\n");
    }
    Ok(out)
}

fn dependents(state: &mut State, args: &Value) -> Result<String, String> {
    let limit = opt_usize(args, "limit", 40).clamp(1, 500);
    let (loaded, range, note) = located(state, args)?;
    let workbook = &loaded.workbook;

    let (refs, report) = dependents_of(workbook, range, limit);
    let mut out = format!(
        "{note}{} is read by {} formula(s), of {} scanned\n",
        workbook.cite_range(range),
        report.matches,
        report.formulas_scanned
    );
    for reference in &refs {
        out.push_str(&format!(
            "  {:<24} {}\n",
            workbook.cite(reference.from),
            reference.text
        ));
    }
    if refs.is_empty() {
        out.push_str("  nothing in this workbook reads it\n");
    } else if report.capped {
        out.push_str("  … more, raise limit\n");
    }
    Ok(out)
}

fn recompute_tool(state: &mut State, args: &Value) -> Result<String, String> {
    let limit = opt_usize(args, "limit", 40).clamp(1, 500);
    let redact = state.redact_values;
    let (loaded, range, note) = located(state, args)?;
    let workbook = &loaded.workbook;

    let mut out = note;
    let mut printed = 0;
    for (at, _) in workbook
        .sheet(range.sheet)
        .into_iter()
        .flat_map(|sheet| sheet.iter_range(range))
    {
        let Some(result) = recompute(workbook, at) else {
            continue;
        };
        if printed >= limit {
            out.push_str("… more, raise limit\n");
            break;
        }
        printed += 1;
        out.push_str(&format!("{}\n  ={}\n", result.a1, result.formula));
        out.push_str(&match &result.outcome {
            Outcome::Agrees(value) => {
                format!("  agrees with the stored value {}\n", show(value, redact))
            }
            Outcome::Differs { computed, stored } => format!(
                "  DIFFERS — computed {}, stored {}\n",
                show(computed, redact),
                show(stored, redact)
            ),
            Outcome::Unsupported(reason) => format!("  not recomputed: {reason}\n"),
        });
        for input in &result.inputs {
            let detail = match &input.value {
                Some(value) => show(value, redact),
                None => format!("{} cells", input.cells),
            };
            out.push_str(&format!(
                "    {:<24} {:<12} {detail}\n",
                input.a1, input.text
            ));
        }
    }
    if printed == 0 {
        let fact = cell_fact(workbook, range.top_left());
        out.push_str(&match fact {
            Some(fact) => format!(
                "{} holds no formula — {} is a literal, and there is nothing to recompute.\n",
                fact.a1,
                show(&fact.value, redact)
            ),
            None => format!(
                "{} is empty — nothing to recompute.\n",
                workbook.cite_range(range)
            ),
        });
    }
    Ok(out)
}

/// A JSON value as a cell would hold it. An agent writes what it means — a
/// number, a string, a boolean — and anything else is refused rather than
/// coerced into something the workbook would not have held.
fn cell_value(value: &Value) -> Result<CellValue, String> {
    match value {
        Value::Number(n) => n
            .as_f64()
            .map(CellValue::Number)
            .ok_or_else(|| format!("{n} is not a number a cell can hold")),
        Value::String(s) => Ok(CellValue::Text(s.clone())),
        Value::Bool(b) => Ok(CellValue::Bool(*b)),
        Value::Null => Ok(CellValue::Empty),
        other => Err(format!(
            "a cell holds a number, a string, a boolean or nothing — not {other}"
        )),
    }
}

fn what_if_tool(state: &mut State, args: &Value) -> Result<String, String> {
    let redact = state.redact_values;
    let levels = opt_usize(args, "levels", 8).clamp(1, 32);
    let limit = opt_usize(args, "limit", 40).clamp(1, 500);
    let requested = args
        .get("changes")
        .and_then(Value::as_array)
        .ok_or_else(|| "changes is required, as an array of {citation, value}".to_string())?;
    if requested.is_empty() {
        return Err("changes is empty — name at least one cell to change".to_string());
    }

    let (_, path) = state.resolve(opt_str(args, "workbook").as_deref())?;
    let (loaded, load_seconds) = state.workbook(&path)?;
    let workbook = &loaded.workbook;

    let mut changes = Vec::new();
    for change in requested {
        let citation = want_str(change, "citation")?;
        let range = resolve_range(workbook, &citation)?;
        if range.cell_count() != 1 {
            return Err(format!(
                "{citation:?} is {} cells. A change names one.",
                range.cell_count()
            ));
        }
        let value = cell_value(
            change
                .get("value")
                .ok_or_else(|| format!("{citation:?} has no value to put in it"))?,
        )?;
        changes.push(Change::new(range.top_left(), value));
    }

    let impact = what_if(
        workbook,
        &changes,
        &WhatIfOptions {
            max_levels: levels,
            limit,
            ..Default::default()
        },
    );
    let report = &impact.report;

    let mut out = match load_seconds {
        Some(seconds) => format!("(opened {path} in {seconds:.1}s)\n"),
        None => String::new(),
    };
    for applied in &impact.changes {
        out.push_str(&format!(
            "{:<24} {} → {}\n",
            applied.a1,
            show(&applied.before, redact),
            show(&applied.after, redact)
        ));
        if let Some(formula) = &applied.replaced_formula {
            out.push_str(&format!("  replacing ={formula}\n"));
        }
    }
    out.push_str(&format!(
        "\n{} cell(s) downstream over {} level(s): {} moved, {} unchanged, {} with no answer\n",
        report.affected, report.levels, report.moved, report.unchanged, report.blocked
    ));
    if let Some(stopped) = report.stopped {
        out.push_str(&format!(
            "the walk stopped at {} — the change reaches further than this\n",
            stopped.as_str()
        ));
    }

    if !impact.moved.is_empty() {
        out.push_str(&format!(
            "\nmoved ({} of {})\n",
            impact.moved.len(),
            report.moved
        ));
        for moved in &impact.moved {
            out.push_str(&format!("  {:<24} ={}\n", moved.a1, moved.formula));
            out.push_str(&format!(
                "    {} → {}   (level {}){}\n",
                show(&moved.before, redact),
                show(&moved.after, redact),
                moved.level,
                if moved.was_stale {
                    "  — this cell already disagreed with its stored value"
                } else {
                    ""
                }
            ));
        }
        if report.moved_not_listed > 0 {
            out.push_str(&format!(
                "  … {} more, raise limit\n",
                report.moved_not_listed
            ));
        }
    }

    if !impact.unanswered.is_empty() {
        out.push_str(&format!(
            "\nno answer ({} of {}) — the change reaches these and this cannot say where it \
             leaves them\n",
            impact.unanswered.len(),
            report.blocked
        ));
        for blocked in &impact.unanswered {
            let why = match &blocked.reason {
                Blocked::Formula(reason) => reason.to_string(),
                Blocked::Upstream(cause) => format!("reads {cause}, which has no answer"),
                Blocked::Cycle => "circular reference".to_string(),
            };
            out.push_str(&format!("  {:<24} {why}\n", blocked.a1));
        }
    }
    if report.affected == 0 {
        out.push_str("\nnothing in this workbook reads it.\n");
    }
    Ok(out)
}
