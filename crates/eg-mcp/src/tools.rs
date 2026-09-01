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

use eg_eval::query::{query as query_run, Aggregate, Filter, Query, Test};
use eg_eval::whatif::{what_if, Blocked, Change, WhatIfOptions};
use eg_eval::{
    cell as cell_fact, cells_holding, cells_in, dependents_of, precedents_of, recompute, Outcome,
};
use eg_eval::{infer_schema, Lookup};
use eg_index::SearchOptions;
use eg_model::{parse_a1, redact_formula_literals, CellValue, RangeRef, Workbook};
use eg_retrieve::{expand, find_in, render, ExpandOptions, Fusion, RenderOptions, Search};
use eg_structure::{detect_regions, read_table, Table};
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
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "How many hits, default 8." },
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
                    "workbook": { "type": "string", "description": "Restrict to one workbook (content hash, path or file name)." },
                    "sheet": { "type": "string", "description": "Restrict to one sheet, by exact name." },
                    "seeds": { "type": "integer", "minimum": 1, "maximum": 50, "description": "How many hits to expand from, default 5." },
                    "hops": { "type": "integer", "description": "Dependency hops from a seed, default 2." },
                    "budget": { "type": "integer", "minimum": 1, "description": "Most nodes per workbook, default 40." },
                    "children": { "type": "integer", "description": "Contained children to show per node, default 0." },
                    "max_chars": { "type": "integer", "minimum": 200, "description": "Ceiling on the passage, default 8000." },
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
        name: "find_value",
        description: "Find the cells holding a value, by scanning every one of them. Use this \
                      when `search` comes back blind on a figure that is plainly in the \
                      workbook: the index carries a column's values only where there were few \
                      enough to keep, so a number from a large numeric column is in no index and \
                      cannot be. Expensive, for the same reason `dependents` is — nothing \
                      records where a value lives. Exhaustive, so a nil answer means the value \
                      is genuinely not there.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "value": { "description": "The value to find, as a JSON number, string or boolean. A number and the string of its digits are different values; the answer says how many cells hold the other one." },
                    "workbook": { "type": "string", "description": "Which workbook (content hash, path or file name). Optional when the corpus holds one." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Most cells to return, default 40." }
                },
                "required": ["value"],
                "additionalProperties": false
            })
        },
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
        name: "tables",
        description: "The tables of a workbook, or of one sheet: each one's range, its title,                       and its columns with the type of each. Start here before `query_table` — a                       query names columns by header, and this is what the headers are.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "workbook": {"type": "string", "description": "Path or hash. Optional when the corpus holds one."},
                    "sheet": {"type": "string", "description": "Restrict to one sheet, by exact name."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Tables to list. Default 40."}
                },
                "additionalProperties": false
            })
        },
    },
    Tool {
        name: "query_table",
        description: "Filter, group and total the rows of one table — the question a workbook                       only answers if somebody already wrote a cell for it. Names columns by                       header, from `tables`. Every answer carries the range it was computed                       over, because a table's boundaries are inferred and a totals row swept                       into the body would double every sum. Refuses rather than guesses: a                       header naming two columns, or a total over a column that is not numbers.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "table": {"type": "string", "description": "The table's range, from `tables`, e.g. \"'Work Doc'!A1:BM115004\"."},
                    "workbook": {"type": "string", "description": "Path or hash. Optional when the corpus holds one."},
                    "where": {
                        "type": "array",
                        "description": "Conditions a row must all pass.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "column": {"type": "string", "description": "Header, as `tables` gives it."},
                                "test": {"type": "string", "enum": ["is", "is_not", "contains", "one_of", "above", "at_least", "below", "at_most", "blank", "not_blank", "failed"]},
                                "value": {"description": "The value to test against. A list for `one_of`; omitted for `blank`, `not_blank` and `failed`."}
                            },
                            "required": ["column", "test"],
                            "additionalProperties": false
                        }
                    },
                    "group_by": {"type": "array", "items": {"type": "string"}, "description": "Column headers to group by."},
                    "aggregate": {
                        "type": "array",
                        "description": "What to compute. `count` takes no column.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "of": {"type": "string", "enum": ["count", "count_values", "count_distinct", "sum", "mean", "min", "max"]},
                                "column": {"type": "string"}
                            },
                            "required": ["of"],
                            "additionalProperties": false
                        }
                    },
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Groups returned. Default 20."}
                },
                "additionalProperties": false,
                "required": ["table", "aggregate"]
            })
        },
    },
    Tool {
        name: "schema",
        description: "The relations the workbook states in its own lookup formulas: which                       column keys into which table, and what comes back. A spreadsheet has no                       schema and declares one anyway, once per row. Use it to follow a value                       from one table to another. An approximate lookup is reported as a                       *banding* — a set of thresholds, not a key — because joining it on                       equality would be wrong.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "workbook": {"type": "string", "description": "Path or hash. Optional when the corpus holds one."},
                    "sheet": {"type": "string", "description": "Only relations whose formulas live on this sheet."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Relations returned, heaviest first. Default 25."}
                },
                "additionalProperties": false
            })
        },
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
                    "levels": { "type": "integer", "minimum": 1, "maximum": 32, "description": "Levels of the dependency chain to follow, default 8. Each is a full scan." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Most moved cells to list, default 40." }
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
            "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Most rows to return, default 40." }
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
        "find_value" => find_value(state, args),
        "recompute" => recompute_tool(state, args),
        "tables" => tables(state, args),
        "query_table" => query_table(state, args),
        "schema" => schema(state, args),
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

/// What `json!` calls a value, for an error a caller can act on.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// An absent or `null` argument is the default; a *present* one of the wrong
/// JSON type is refused rather than silently treated as absent — a caller
/// that sent `"limit": "3"` typo'd a type, and finding out from a wrong
/// answer instead of an error is worse than a schema was supposed to allow.
fn opt_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("{key} must be a string, got {}", json_kind(other))),
    }
}

fn opt_usize(args: &Value, key: &str, default: usize) -> Result<usize, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| format!("{key} must be a non-negative integer, got {n}")),
        Some(other) => Err(format!(
            "{key} must be an integer, got {}",
            json_kind(other)
        )),
    }
}

fn opt_bounded(
    args: &Value,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, String> {
    let value = opt_usize(args, key, default)?;
    if !(min..=max).contains(&value) {
        return Err(format!(
            "{key} must be between {min} and {max}, got {value}"
        ));
    }
    Ok(value)
}

/// As [`opt_str`], for a list argument. Absent or `null` is the empty list;
/// anything else that is not an array is refused.
///
/// Silently reading a wrong-typed one as absent is worse here than anywhere
/// else on this surface: `where` sent as a single object instead of a list of
/// them drops every condition, and `query_table` then totals the whole table
/// and presents it as the answer to a filtered question.
fn opt_array<'a>(args: &'a Value, key: &str) -> Result<&'a [Value], String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(other) => Err(format!("{key} must be an array, got {}", json_kind(other))),
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("{key} must be a boolean, got {}", json_kind(other))),
    }
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
fn find(
    state: &mut State,
    query: &str,
    opts: &SearchOptions,
    lexical_only: bool,
) -> Result<Search, String> {
    let fusion = Fusion {
        lexical_only,
        ..Default::default()
    };
    let (text, held) = state.halves();
    let semantic = if lexical_only { None } else { held };
    find_in(text, semantic, query, opts, &fusion).map_err(|e| e.to_string())
}

fn search(state: &mut State, args: &Value) -> Result<String, String> {
    let query = want_str(args, "query")?;
    let workbook = match opt_str(args, "workbook")? {
        Some(want) => Some(state.resolve(Some(&want))?.0),
        None => None,
    };
    let opts = SearchOptions {
        limit: opt_bounded(args, "limit", 8, 1, 100)?,
        workbook,
        sheet: opt_str(args, "sheet")?,
        ..Default::default()
    };
    let found = find(state, &query, &opts, opt_bool(args, "lexical_only")?)?;
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
    let seeds = opt_bounded(args, "seeds", 5, 1, 50)?;
    let workbook = match opt_str(args, "workbook")? {
        Some(want) => Some(state.resolve(Some(&want))?.0),
        None => None,
    };
    let opts = SearchOptions {
        limit: seeds,
        workbook,
        sheet: opt_str(args, "sheet")?,
        ..Default::default()
    };
    let found = find(state, &query, &opts, opt_bool(args, "lexical_only")?)?;
    let warning = found.warning();
    let evidence = found.evidence();
    if found.is_empty() {
        return Ok(format!("{query:?} — nothing matched."));
    }
    let hits = found.hits;

    let expand_opts = ExpandOptions {
        hops: opt_usize(args, "hops", 2)?,
        budget: opt_bounded(args, "budget", 40, 1, usize::MAX)?,
        children: opt_usize(args, "children", 0)?,
        ..Default::default()
    };
    let found =
        expand(&state.corpus, &hits, &expand_opts).map_err(|e| format!("expansion failed: {e}"))?;
    let rendered = render(
        &found,
        &RenderOptions {
            max_chars: opt_bounded(args, "max_chars", 8000, 200, usize::MAX)?,
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
    for workbook in &found.workbooks {
        if !workbook.stale_seeds.is_empty() {
            out.push_str(&format!(
                "\n{}: {} seed(s) the index ranked no longer exist in the stored \
                 graph — reindex to refresh it",
                &workbook.content_hash[..workbook.content_hash.len().min(12)],
                workbook.stale_seeds.len()
            ));
        }
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
    let (_, path) = state.resolve(opt_str(args, "workbook")?.as_deref())?;
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

/// The populated cells of a citation, clipped to what the sheet actually uses.
///
/// `Sheet::iter_range` probes the ordered map once per row of the range's
/// *height*, so an unclipped whole-column citation — `Sheet1!A:A`, which
/// `parse_a1` accepts — costs 1,048,576 probes to find whatever handful of
/// cells is really there. `eg_eval::cells_in` already clips for exactly this
/// reason; the tools that walk a range themselves must too.
fn populated_in(
    workbook: &Workbook,
    range: RangeRef,
) -> impl Iterator<Item = (eg_model::CellRef, &eg_model::Cell)> {
    let sheet = workbook.sheet(range.sheet);
    let clipped = sheet
        .and_then(|s| s.used_range())
        .and_then(|used| range.intersection(&used));
    sheet
        .into_iter()
        .flat_map(move |s| clipped.into_iter().flat_map(move |r| s.iter_range(r)))
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

/// A formula's text, with its own literals hidden the same way a value is.
///
/// A formula's literals are the workbook's data as much as any cell's value
/// is — `=IF(A2="Smith, John",B2*0.15,0)` names a person and a rate — so
/// printing it unredacted while the value column shows `<number>` would leak
/// exactly what `--redact-values` exists to withhold.
fn show_formula(formula: &str, redact: bool) -> String {
    if redact {
        redact_formula_literals(formula)
    } else {
        formula.to_string()
    }
}

fn read_cells(state: &mut State, args: &Value) -> Result<String, String> {
    let limit = opt_bounded(args, "limit", 40, 1, 500)?;
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
            out.push_str(&format!(" ={}", show_formula(formula, redact)));
        }
        out.push_str(&format!("  {}\n", show(&fact.value, redact)));
    }
    if cells.is_empty() {
        out.push_str("  nothing populated in that range\n");
    }
    Ok(out)
}

fn precedents(state: &mut State, args: &Value) -> Result<String, String> {
    let limit = opt_bounded(args, "limit", 40, 1, 500)?;
    let (loaded, range, note) = located(state, args)?;
    let workbook = &loaded.workbook;

    let mut out = format!("{note}{} reads\n", workbook.cite_range(range));
    let mut printed = 0;
    for (at, cell) in populated_in(workbook, range) {
        if cell.formula.is_none() {
            continue;
        }
        for reference in precedents_of(workbook, at) {
            if printed >= limit {
                out.push_str("  … more, raise limit\n");
                return Ok(out);
            }
            let target = reference.target.cite(workbook);
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
    let limit = opt_bounded(args, "limit", 40, 1, 500)?;
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

/// Which cells hold a value.
///
/// One workbook, resolved the way every other cell-level tool resolves one.
/// Not the whole corpus: the scan is linear in a workbook and a corpus of large
/// files would be minutes, which is not a thing to do to an agent that asked a
/// question. `eg where corpus/ <value>` on a terminal is where a caller who
/// wants that goes.
fn find_value(state: &mut State, args: &Value) -> Result<String, String> {
    let limit = opt_bounded(args, "limit", 40, 1, 500)?;
    let redact = state.redact_values;
    let probe = cell_value(
        args.get("value")
            .ok_or_else(|| "value is required — the value to find".to_string())?,
    )?;
    if probe == CellValue::Empty {
        return Err("value names nothing to look for; an empty cell is not a value".to_string());
    }
    let (_, path) = state.resolve(opt_str(args, "workbook")?.as_deref())?;
    let (loaded, load_seconds) = state.workbook(&path)?;
    let workbook = &loaded.workbook;

    let (cells, report) = cells_holding(workbook, &probe, limit);
    let mut out = match load_seconds {
        Some(seconds) => format!("(opened {path} in {seconds:.1}s)\n"),
        None => String::new(),
    };
    out.push_str(&format!(
        "{} cell(s) hold {}, of {} scanned\n",
        report.matches,
        show(&probe, redact),
        report.cells_scanned
    ));
    for fact in &cells {
        out.push_str(&format!("  {:<24}", fact.a1));
        if let Some(formula) = &fact.formula {
            out.push_str(&format!(" ={}", show_formula(formula, redact)));
        }
        out.push_str(&format!("  {}\n", show(&fact.value, redact)));
    }
    if report.capped {
        out.push_str("  … more, raise limit\n");
    }
    // The usual reason a value that is on the screen scans as absent. Left
    // unsaid, the answer above reads as "it is not in this workbook".
    if report.other_kind > 0 {
        out.push_str(&format!(
            "  {} more cell(s) show the same characters while holding another type. \
             A number and the string of its digits are different values here: send \
             1612 to find one and \"1612\" to find the other.\n",
            report.other_kind
        ));
    }
    if report.matches == 0 {
        out.push_str(
            "  every populated cell was compared, so this is exhaustive: the value is not \
             in this workbook\n",
        );
    }
    Ok(out)
}

fn recompute_tool(state: &mut State, args: &Value) -> Result<String, String> {
    let limit = opt_bounded(args, "limit", 40, 1, 500)?;
    let redact = state.redact_values;
    let (loaded, range, note) = located(state, args)?;
    let workbook = &loaded.workbook;

    let mut out = note;
    let mut printed = 0;
    for (at, cell) in populated_in(workbook, range) {
        // Checked before recomputing, not after: `recompute` parses and
        // evaluates the formula, and the limit is meant to bound that work,
        // not just the printing of it.
        if cell.formula.is_none() {
            continue;
        }
        if printed >= limit {
            out.push_str("… more, raise limit\n");
            break;
        }
        let Some(result) = recompute(workbook, at) else {
            continue;
        };
        printed += 1;
        out.push_str(&format!(
            "{}\n  ={}\n",
            result.a1,
            show_formula(&result.formula, redact)
        ));
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
    let levels = opt_bounded(args, "levels", 8, 1, 32)?;
    let limit = opt_bounded(args, "limit", 40, 1, 500)?;
    let requested = args
        .get("changes")
        .and_then(Value::as_array)
        .ok_or_else(|| "changes is required, as an array of {citation, value}".to_string())?;
    if requested.is_empty() {
        return Err("changes is empty — name at least one cell to change".to_string());
    }

    let (_, path) = state.resolve(opt_str(args, "workbook")?.as_deref())?;
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
            out.push_str(&format!("  replacing ={}\n", show_formula(formula, redact)));
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
            out.push_str(&format!(
                "  {:<24} ={}\n",
                moved.a1,
                show_formula(&moved.formula, redact)
            ));
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

// ---- tables, queries and the schema --------------------------------------

/// The tables of a workbook, which is what a query names its columns from.
fn tables(state: &mut State, args: &Value) -> Result<String, String> {
    let (_, path) = state.resolve(opt_str(args, "workbook")?.as_deref())?;
    let (loaded, load_seconds) = state.workbook(&path)?;
    let only = opt_str(args, "sheet")?;
    let limit = opt_bounded(args, "limit", 40, 1, 500)?;

    let mut out = String::new();
    if let Some(seconds) = load_seconds {
        out.push_str(&format!("{path} opened in {seconds:.1}s\n\n"));
    }
    let mut listed = 0usize;
    let mut total = 0usize;
    for sheet in &loaded.workbook.sheets {
        if only.as_deref().is_some_and(|name| name != sheet.name) {
            continue;
        }
        for region in detect_regions(sheet) {
            let Some(table) = read_table(sheet, &region) else {
                continue;
            };
            total += 1;
            if listed >= limit {
                continue;
            }
            listed += 1;
            out.push_str(&format!(
                "{}{}\n  {} row(s), {} column(s)\n",
                loaded.workbook.cite_range(table.body),
                table
                    .title
                    .as_deref()
                    .map(|t| format!("  {t:?}"))
                    .unwrap_or_default(),
                table.rows(),
                table.columns.len()
            ));
            for column in &table.columns {
                let name = if column.header.is_empty() {
                    "(no header)"
                } else {
                    &column.header
                };
                out.push_str(&format!(
                    "    {:<38} {:<7} {} populated\n",
                    name,
                    column.kind.as_str(),
                    column.populated
                ));
            }
        }
    }
    if total > listed {
        out.push_str(&format!(
            "\n… {} more table(s) not listed\n",
            total - listed
        ));
    }
    if total == 0 {
        out.push_str("no tables found\n");
    } else {
        out.push_str("\nQuery one with `query_table`, naming its range and its column headers.\n");
    }
    Ok(out)
}

/// Find the table a caller named by its range.
fn table_at(loaded: &eg_ingest::Loaded, citation: &str) -> Result<Table, String> {
    let range = resolve_range(&loaded.workbook, citation)?;
    let sheet = loaded
        .workbook
        .sheet(range.sheet)
        .ok_or_else(|| format!("{citation} names no sheet of this workbook"))?;
    for region in detect_regions(sheet) {
        if region.range == range {
            let table = read_table(sheet, &region)
                .ok_or_else(|| format!("{citation} is a region with no rows under its header"))?;
            return Ok(table);
        }
    }
    Err(format!(
        "{citation} is not a table of this workbook — `tables` lists the ranges that are"
    ))
}

fn query_table(state: &mut State, args: &Value) -> Result<String, String> {
    let citation = want_str(args, "table")?;
    let (_, path) = state.resolve(opt_str(args, "workbook")?.as_deref())?;
    let (loaded, load_seconds) = state.workbook(&path)?;
    let table = table_at(&loaded, &citation)?;

    let mut query = Query {
        limit: opt_bounded(args, "limit", 20, 1, 200)?,
        ..Default::default()
    };
    for filter in opt_array(args, "where")? {
        query.filters.push(read_filter(filter)?);
    }
    for column in opt_array(args, "group_by")? {
        query.group_by.push(
            column
                .as_str()
                .ok_or("group_by takes column headers as strings")?
                .to_string(),
        );
    }
    for aggregate in opt_array(args, "aggregate")? {
        query.aggregates.push(read_aggregate(aggregate)?);
    }

    let answer = query_run(&loaded.workbook, &table, &query).map_err(|e| e.to_string())?;
    let labels: Vec<String> = query.aggregates.iter().map(Aggregate::label).collect();
    // A total *is* a value — a number the workbook never wrote down — so it is
    // redacted like any other when this server was told to.
    let redact = state.redact_values;

    let mut out = String::new();
    if let Some(seconds) = load_seconds {
        out.push_str(&format!("{path} opened in {seconds:.1}s\n\n"));
    }
    // The range first, not last. A table's boundaries are inferred, and an
    // answer nobody can check against the cells it came from is a number with
    // no provenance — which is the one thing this project does not produce.
    out.push_str(&format!(
        "over {}\n{} row(s) scanned, {} matched\n",
        loaded.workbook.cite_range(answer.over),
        answer.rows_scanned,
        answer.rows_matched
    ));
    if answer.rows_with_errors > 0 {
        out.push_str(&format!(
            "{} row(s) dropped: the column a filter tested held an error value\n",
            answer.rows_with_errors
        ));
    }
    if answer.errors_in_aggregates > 0 {
        out.push_str(&format!(
            "{} error cell(s) left out of the totals — check the column before trusting them\n",
            answer.errors_in_aggregates
        ));
    }
    out.push('\n');

    for group in &answer.groups {
        let key = if group.key.is_empty() {
            String::new()
        } else {
            format!(
                "{}  ",
                group
                    .key
                    .iter()
                    .map(|v| show(v, redact))
                    .collect::<Vec<_>>()
                    .join(" / ")
            )
        };
        let mut parts = Vec::new();
        for (i, label) in labels.iter().enumerate() {
            let shown = match (group.values[i], group.counts[i]) {
                (Some(v), _) => show(&CellValue::Number(v), redact),
                (None, Some(c)) => c.to_string(),
                (None, None) => "—".to_string(),
            };
            parts.push(format!("{label} {shown}"));
        }
        out.push_str(&format!(
            "  {key}({} rows)  {}\n",
            group.rows,
            parts.join(", ")
        ));
    }
    if answer.groups.is_empty() {
        out.push_str("  no rows matched\n");
    }
    if answer.groups_not_listed > 0 {
        out.push_str(&format!(
            "  … {} more group(s), raise `limit`\n",
            answer.groups_not_listed
        ));
    }
    Ok(out)
}

fn read_filter(value: &Value) -> Result<Filter, String> {
    let column = value
        .get("column")
        .and_then(Value::as_str)
        .ok_or("each condition needs a `column`")?
        .to_string();
    let test = value
        .get("test")
        .and_then(Value::as_str)
        .ok_or("each condition needs a `test`")?;
    let arg = value.get("value");
    let number = || -> Result<f64, String> {
        arg.and_then(Value::as_f64)
            .ok_or_else(|| format!("`{test}` needs a number in `value`"))
    };
    let cell = || -> Result<CellValue, String> {
        cell_value(arg.ok_or_else(|| format!("`{test}` needs a `value`"))?)
    };
    let test = match test {
        "is" => Test::Is(cell()?),
        "is_not" => Test::IsNot(cell()?),
        "contains" => Test::Contains(
            arg.and_then(Value::as_str)
                .ok_or("`contains` needs text in `value`")?
                .to_string(),
        ),
        "one_of" => Test::OneOf(
            arg.and_then(Value::as_array)
                .ok_or("`one_of` needs a list in `value`")?
                .iter()
                .map(cell_value)
                .collect::<Result<_, _>>()?,
        ),
        "above" => Test::Above(number()?),
        "at_least" => Test::AtLeast(number()?),
        "below" => Test::Below(number()?),
        "at_most" => Test::AtMost(number()?),
        "blank" => Test::Blank,
        "not_blank" => Test::NotBlank,
        "failed" => Test::Failed,
        other => return Err(format!("no test called {other:?}")),
    };
    Ok(Filter { column, test })
}

fn read_aggregate(value: &Value) -> Result<Aggregate, String> {
    let of = value
        .get("of")
        .and_then(Value::as_str)
        .ok_or("each aggregate needs an `of`")?;
    let column = || -> Result<String, String> {
        value
            .get("column")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("`{of}` needs a `column`"))
    };
    Ok(match of {
        "count" => Aggregate::Count,
        "count_values" => Aggregate::CountValues(column()?),
        "count_distinct" => Aggregate::CountDistinct(column()?),
        "sum" => Aggregate::Sum(column()?),
        "mean" => Aggregate::Mean(column()?),
        "min" => Aggregate::Min(column()?),
        "max" => Aggregate::Max(column()?),
        other => return Err(format!("no aggregate called {other:?}")),
    })
}

fn schema(state: &mut State, args: &Value) -> Result<String, String> {
    let (_, path) = state.resolve(opt_str(args, "workbook")?.as_deref())?;
    let (loaded, load_seconds) = state.workbook(&path)?;
    let only = opt_str(args, "sheet")?;
    let limit = opt_bounded(args, "limit", 25, 1, 200)?;

    let found = infer_schema(&loaded.workbook);
    let wanted: Vec<&Lookup> = found
        .lookups
        .iter()
        .filter(|l| match &only {
            Some(name) => loaded
                .workbook
                .sheet(l.from.sheet)
                .is_some_and(|s| &s.name == name),
            None => true,
        })
        .collect();

    let mut out = String::new();
    if let Some(seconds) = load_seconds {
        out.push_str(&format!("{path} opened in {seconds:.1}s\n\n"));
    }
    out.push_str(&format!(
        "{} relation(s) stated by {} lookup formula group(s)",
        found.lookups.len(),
        found.with_lookups
    ));
    if found.unrecognised > 0 || found.unresolvable > 0 {
        out.push_str(&format!(
            " ({} shape(s) not read, {} pointing outside this workbook)",
            found.unrecognised, found.unresolvable
        ));
    }
    out.push_str("\n\n");

    for lookup in wanted.iter().take(limit) {
        out.push_str(&format!(
            "{} {}\n  key {} → {}{}\n",
            lookup.kind.as_str(),
            loaded.workbook.cite_range(lookup.from),
            lookup
                .key
                .map(|k| loaded.workbook.cite_range(k))
                .unwrap_or_else(|| "(computed, no single column)".into()),
            loaded.workbook.cite_range(lookup.table),
            lookup
                .column
                .map(|c| format!(" column {c}"))
                .unwrap_or_default(),
        ));
        if let Some(returns) = lookup.returns {
            out.push_str(&format!(
                "  returns {}\n",
                loaded.workbook.cite_range(returns)
            ));
        }
        out.push_str(&format!("  {} formula cell(s)", lookup.cells));
        if lookup.approximate {
            out.push_str("  [approximate — a banding over thresholds, not a key to join on]");
        }
        out.push('\n');
    }
    if wanted.len() > limit {
        out.push_str(&format!("\n… {} more relation(s)\n", wanted.len() - limit));
    }
    if wanted.is_empty() {
        out.push_str("no lookup relations here\n");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_redaction_drops_literals_the_way_read_cells_and_recompute_print_them() {
        // The exact site `read_cells`/`recompute`/`what_if` all call before
        // splicing a formula into a result: a redacted formula must carry no
        // literal a redacted value wouldn't have shown either.
        let formula = "IF(A2=\"Smith, John\",B2*0.15,0)";
        assert_eq!(show_formula(formula, false), formula);
        assert_eq!(
            show_formula(formula, true),
            "IF(A2=<text>,B2*<number>,<number>)"
        );
        assert_eq!(
            show_formula("VLOOKUP(A1,Rates!A1:B9,2,FALSE)", true),
            "VLOOKUP(A1,Rates!A1:B9,<number>,FALSE)"
        );
    }

    fn grid(id: u16, name: &str, rows: &[&str]) -> eg_model::Sheet {
        let mut sheet = eg_model::Sheet::new(eg_model::SheetId(id), name);
        for (r, line) in rows.iter().enumerate() {
            for (c, tok) in line.split_whitespace().enumerate() {
                let value = match tok.parse::<f64>() {
                    Ok(n) => CellValue::Number(n),
                    Err(_) => CellValue::Text(tok.to_string()),
                };
                sheet.set(r as u32, c as u16, eg_model::Cell::literal(value));
            }
        }
        sheet
    }

    fn one_sheet_workbook(hash: &str, path: &str, sheet_name: &str) -> Workbook {
        Workbook {
            path: path.into(),
            format: Some(eg_model::WorkbookFormat::Xlsx),
            content_hash: hash.into(),
            sheets: vec![grid(
                0,
                sheet_name,
                // "Region" heads the leftmost column, which region detection
                // reads as row labels rather than a headed column (a
                // documented gap, see eg-retrieve's answers test) — so
                // "Revenue" is put over the second column instead, to be sure
                // it gets an actual column node to search on.
                &["Region Revenue", "North 10", "South 20", "East 30"],
            )],
            defined_names: Vec::new(),
            external_links: Vec::new(),
        }
    }

    /// A corpus of two workbooks, each with a "Revenue" column on a
    /// differently-named sheet, so a search for "revenue" with no filter
    /// finds both and a `workbook` filter should find only one.
    fn two_workbook_state() -> (State, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().to_str().expect("utf-8 path");

        let mut corpus = eg_graph::store::Corpus::open(path).expect("corpus opens");
        let mut text = eg_index::TextIndex::open(path).expect("text index opens");
        for wb in [
            one_sheet_workbook("hash-alpha", "alpha.xlsx", "Alpha Sales"),
            one_sheet_workbook("hash-beta", "beta.xlsx", "Beta Sales"),
        ] {
            let built = eg_graph::build(&wb);
            corpus
                .put(
                    &wb.content_hash,
                    &wb.path,
                    wb.sheets.len(),
                    wb.total_cells() as u64,
                    true,
                    &built,
                )
                .expect("stored");
            text.index_built(&built, &wb.content_hash, &wb.path)
                .expect("indexed");
        }
        // Tantivy allows one writer on a directory at a time; `State::open`
        // opens its own.
        drop(text);

        let state = State::open(path, false).expect("state opens over what was just indexed");
        (state, dir)
    }

    #[test]
    fn context_can_be_scoped_to_one_workbook() {
        let (mut state, _dir) = two_workbook_state();

        let unscoped = context(
            &mut state,
            &json!({ "query": "revenue", "lexical_only": true }),
        )
        .expect("answered");
        assert!(unscoped.contains("alpha.xlsx") || unscoped.contains("Alpha"));
        assert!(unscoped.contains("beta.xlsx") || unscoped.contains("Beta"));

        let scoped = context(
            &mut state,
            &json!({ "query": "revenue", "workbook": "hash-alpha", "lexical_only": true }),
        )
        .expect("answered");
        assert!(
            scoped.contains("Alpha") || scoped.contains("alpha.xlsx"),
            "{scoped}"
        );
        assert!(
            !scoped.contains("Beta") && !scoped.contains("beta.xlsx"),
            "a workbook filter must not let the other workbook's nodes into the passage: {scoped}"
        );
    }

    #[test]
    fn a_string_where_an_integer_belongs_is_refused_not_defaulted() {
        // L8: `opt_usize`/`opt_bool`/`opt_str` used to substitute the default
        // for any wrong-typed value, so `"limit": "3"` silently became the
        // default limit instead of an error — indistinguishable from a
        // caller who never set it at all.
        let err = opt_usize(&json!({ "limit": "3" }), "limit", 8).unwrap_err();
        assert!(err.contains("limit") && err.contains("integer"), "{err}");
    }

    #[test]
    fn a_negative_number_for_an_integer_argument_is_refused() {
        let err = opt_usize(&json!({ "limit": -3 }), "limit", 8).unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn a_fractional_number_for_an_integer_argument_is_refused() {
        let err = opt_usize(&json!({ "limit": 3.5 }), "limit", 8).unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn a_number_where_a_boolean_belongs_is_refused_not_defaulted() {
        let err = opt_bool(&json!({ "lexical_only": 1 }), "lexical_only").unwrap_err();
        assert!(
            err.contains("lexical_only") && err.contains("boolean"),
            "{err}"
        );
    }

    #[test]
    fn a_number_where_a_string_belongs_is_refused_not_defaulted() {
        let err = opt_str(&json!({ "sheet": 3 }), "sheet").unwrap_err();
        assert!(err.contains("sheet") && err.contains("string"), "{err}");
    }

    #[test]
    fn a_wrong_typed_list_argument_is_refused_rather_than_dropped() {
        // The same rule as L8, on the arguments that carry a query. `where`
        // sent as one object instead of a list of them used to become no
        // conditions at all — and `query_table` then totalled the whole table
        // and returned it as the answer to a filtered question.
        let err = opt_array(&json!({ "where": { "column": "Type" } }), "where").unwrap_err();
        assert!(err.contains("where") && err.contains("array"), "{err}");
        assert!(opt_array(&json!({ "aggregate": "sum" }), "aggregate").is_err());
        assert_eq!(opt_array(&json!({}), "where"), Ok(&[][..]));
        assert_eq!(opt_array(&json!({ "where": null }), "where"), Ok(&[][..]));
    }

    #[test]
    fn an_absent_or_null_argument_still_takes_its_default() {
        assert_eq!(opt_usize(&json!({}), "limit", 8), Ok(8));
        assert_eq!(opt_usize(&json!({ "limit": null }), "limit", 8), Ok(8));
        assert_eq!(opt_bool(&json!({}), "lexical_only"), Ok(false));
        assert_eq!(opt_str(&json!({}), "sheet"), Ok(None));
        assert_eq!(opt_str(&json!({ "sheet": null }), "sheet"), Ok(None));
    }

    #[test]
    fn a_tool_call_surfaces_the_type_error_rather_than_a_wrong_answer() {
        let (mut state, _dir) = two_workbook_state();
        let err = search(&mut state, &json!({ "query": "revenue", "limit": "3" }))
            .expect_err("a string limit must be refused");
        assert!(err.contains("limit"), "{err}");
    }
}
