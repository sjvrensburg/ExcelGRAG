//! Turns a structural/schema-gate rejection into a corrective look at the
//! workbook, instead of leaving a model to guess a second coordinate.
//!
//! `eg-mcp`'s cell/range tools (`precedents`, `dependents`, `read_cells`,
//! `recompute`, `graph`) now refuse a *range* citation that lands on no
//! table or block `eg-structure` found on its sheet, and `query_table`
//! resolves a `table` citation the same way through `resolve_range` before
//! it ever reaches this shape. Both failures share one fixed prefix
//! (`eg_mcp::tools::STRUCTURAL_GATE_PREFIX`). Separately, `search`/`context`
//! (and every citation-taking tool, through `resolve_address`) refuse a
//! `sheet` naming no sheet this corpus has at all, with the fixed wording
//! `UNKNOWN_SHEET_PREFIX`. This module recognises both by wording rather
//! than reimplementing either check — the same discipline
//! [`crate::blind_scan`] keeps for the "BLIND MATCH" banner, and for the
//! same reason: reword a message there and the matching correction stops
//! firing, loudly, the next time the scorer runs a question shaped to hit
//! it.
//!
//! The correction mirrors [`crate::blind_scan::should_auto_scan`]'s shape:
//! recognise the failure, run one real tool call in the same step under the
//! same [`crate::policy::Policy`] budget and repeat-call dedupe a
//! model-issued call would get, and hand the model real structure to
//! re-orient with instead of a bare "that doesn't exist" and a second guess.
//!
//! A live run against a real, unfamiliar workbook (`tests/fixtures/community
//! /excel-cpu`, not the demo fixture this harness was tuned against) found
//! that a model does not reliably act on `check_sheet_filter`'s own message
//! even though it names the real sheet: told "no sheet called \"\". This
//! corpus has: SOS", Qwen3-4B-Instruct concluded the *search itself* had
//! found nothing, rather than retrying with the sheet it had just been
//! given. `UnknownSheet`'s correction exists for exactly that gap.

use serde_json::{json, Value};

/// `eg_mcp::tools::resolve_address` and `eg_mcp::tools::check_sheet_filter`
/// share this wording for "a sheet was named that this corpus does not
/// have" — see `tools.rs`'s own `"no sheet called {name:?}. This workbook
/// has: …"` / `"…This corpus has: …"`.
const UNKNOWN_SHEET_PREFIX: &str = "no sheet called ";

/// Which gate rejected a call, since the two shapes correct differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// A citation resolves to a real sheet but lands on no known structure.
    Structural,
    /// A citation or a `sheet` filter names a sheet this corpus does not
    /// have at all — scoping a correction to that same name would just
    /// fail again.
    UnknownSheet,
}

/// Which gate, if any, a tool's rejection is — not a policy refusal
/// (`refused: true`, budget/repeat) and not "ran, found nothing"
/// (`ok: false, refused: false`, no gate wording).
pub fn gate(ok: bool, refused: bool, text: &str) -> Option<Gate> {
    if ok || refused {
        return None;
    }
    if text.starts_with(eg_mcp::tools::STRUCTURAL_GATE_PREFIX) {
        Some(Gate::Structural)
    } else if text.starts_with(UNKNOWN_SHEET_PREFIX) {
        Some(Gate::UnknownSheet)
    } else {
        None
    }
}

pub fn should_auto_correct(ok: bool, refused: bool, text: &str) -> bool {
    gate(ok, refused, text).is_some()
}

/// The sheet named by a rejected call's citation (`citation` for the
/// cell/range tools, `table` for `query_table`), for `tables --sheet` to
/// scope its correction to. `None` when the citation named no sheet at all
/// — `tables` unscoped still shows every table in the workbook, which is
/// still a correction, just a wider one.
fn named_sheet(args: &Value) -> Option<String> {
    let citation = args
        .get("citation")
        .or_else(|| args.get("table"))
        .and_then(Value::as_str)?;
    let (sheet, _rest) = citation.split_once('!')?;
    let trimmed = sheet.trim().trim_matches(['\'', '"']);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The corrective tool to run, and the arguments to run it with, for a call
/// named `name`, made with `args`, that `kind` rejected.
pub fn correction(name: &str, args: &Value, kind: Gate) -> (String, Value) {
    // An UnknownSheet rejection on `search`/`context` itself: the sheet the
    // model guessed does not exist, so the safest retry is the identical
    // call with that filter dropped — searching the whole workbook rather
    // than guessing which real sheet was meant.
    if kind == Gate::UnknownSheet && (name == "search" || name == "context") {
        let mut retry = args.clone();
        if let Some(obj) = retry.as_object_mut() {
            obj.remove("sheet");
        }
        return (name.to_string(), retry);
    }

    // Otherwise: show real structure via `tables`. Scoped to the citation's
    // own sheet only for a Structural rejection, where that sheet is real
    // and just the range was wrong; an UnknownSheet rejection means the
    // sheet name itself was invalid, so scoping to it would only fail
    // again — `tables` runs unscoped instead, across the whole workbook.
    let mut correction_args = json!({});
    if kind == Gate::Structural {
        if let Some(sheet) = named_sheet(args) {
            correction_args["sheet"] = json!(sheet);
        }
    }
    if let Some(wb) = args.get("workbook").cloned() {
        correction_args["workbook"] = wb;
    }
    ("tables".to_string(), correction_args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_structural_gate_rejection_and_nothing_else() {
        let text = format!(
            "{}'Sales'!Z1:AA9 does not overlap …",
            eg_mcp::tools::STRUCTURAL_GATE_PREFIX
        );
        assert_eq!(gate(false, false, &text), Some(Gate::Structural));
        assert!(
            !should_auto_correct(true, false, &text),
            "ok, not a rejection"
        );
        assert!(
            !should_auto_correct(false, true, &text),
            "a policy refusal, not this gate"
        );
    }

    #[test]
    fn recognises_an_unknown_sheet_rejection() {
        let text = "no sheet called \"Sheet1\". This workbook has: SOS";
        assert_eq!(gate(false, false, text), Some(Gate::UnknownSheet));
        let text2 = "no sheet called \"\". This corpus has: SOS";
        assert_eq!(gate(false, false, text2), Some(Gate::UnknownSheet));
    }

    #[test]
    fn a_result_that_is_neither_gate_does_not_auto_correct() {
        assert!(!should_auto_correct(
            false,
            false,
            "Debtors!Z1:AA9 — nothing matched."
        ));
    }

    #[test]
    fn query_table_corrects_with_tables_scoped_to_the_named_sheet() {
        let args = json!({ "table": "'Sales'!Z1:AA9", "workbook": "book.xlsx" });
        let (tool, correction_args) = correction("query_table", &args, Gate::Structural);
        assert_eq!(tool, "tables");
        assert_eq!(correction_args["sheet"], json!("Sales"));
        assert_eq!(correction_args["workbook"], json!("book.xlsx"));
    }

    #[test]
    fn a_cell_tool_corrects_with_tables_on_the_same_sheet() {
        let args = json!({ "citation": "Sales!Z1:AA9" });
        let (tool, correction_args) = correction("precedents", &args, Gate::Structural);
        assert_eq!(tool, "tables");
        assert_eq!(correction_args["sheet"], json!("Sales"));
    }

    #[test]
    fn a_citation_naming_no_sheet_corrects_unscoped() {
        let args = json!({ "citation": "not a range" });
        let (tool, correction_args) = correction("precedents", &args, Gate::Structural);
        assert_eq!(tool, "tables");
        assert!(correction_args.get("sheet").is_none());
    }

    #[test]
    fn an_unknown_sheet_filter_on_search_retries_the_same_call_without_it() {
        let args = json!({ "query": "ADDRESS Y fetch unit", "sheet": "" });
        let (tool, retry_args) = correction("search", &args, Gate::UnknownSheet);
        assert_eq!(tool, "search");
        assert_eq!(retry_args["query"], json!("ADDRESS Y fetch unit"));
        assert!(retry_args.get("sheet").is_none());
    }

    #[test]
    fn an_unknown_sheet_filter_on_context_retries_the_same_call_without_it() {
        let args = json!({ "query": "revenue", "sheet": "Sheet1", "workbook": "book.xlsx" });
        let (tool, retry_args) = correction("context", &args, Gate::UnknownSheet);
        assert_eq!(tool, "context");
        assert!(retry_args.get("sheet").is_none());
        assert_eq!(retry_args["workbook"], json!("book.xlsx"));
    }

    #[test]
    fn an_unknown_sheet_in_a_citation_does_not_scope_the_tables_fallback_to_it() {
        // The rejected sheet name itself would just fail the same way again.
        let args = json!({ "citation": "Sheet1!A1:B2" });
        let (tool, correction_args) = correction("precedents", &args, Gate::UnknownSheet);
        assert_eq!(tool, "tables");
        assert!(correction_args.get("sheet").is_none());
    }
}
