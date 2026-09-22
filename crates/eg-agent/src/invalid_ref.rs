//! Turns a structural/schema-gate rejection into a corrective look at the
//! workbook, instead of leaving a model to guess a second coordinate.
//!
//! `eg-mcp`'s cell/range tools (`precedents`, `dependents`, `read_cells`,
//! `recompute`, `graph`) now refuse a *range* citation that lands on no
//! table or block `eg-structure` found on its sheet, and `query_table`
//! resolves a `table` citation the same way through `resolve_range` before
//! it ever reaches this shape. Both failures share one fixed prefix
//! (`eg_mcp::tools::STRUCTURAL_GATE_PREFIX`) so this module can recognise
//! them by wording rather than reimplementing the check — the same
//! discipline [`crate::blind_scan`] keeps for the "BLIND MATCH" banner, and
//! for the same reason: reword the message there and this stops firing,
//! loudly, the next time the scorer runs a question shaped to hit it.
//!
//! The correction mirrors [`crate::blind_scan::should_auto_scan`]'s shape:
//! recognise the failure, run one real tool call in the same step under the
//! same [`crate::policy::Policy`] budget and repeat-call dedupe a
//! model-issued call would get, and hand the model real structure to
//! re-orient with instead of a bare "that doesn't exist" and a second guess.
//! `tables`, scoped to the sheet the rejected citation named, is that
//! structure for every gated tool alike — `context` was considered instead,
//! but it takes a free-text search query, not a citation, and would have
//! searched for the literal rejected coordinate rather than showing what is
//! really on its sheet.

use serde_json::{json, Value};

/// Whether a tool's result is this family of rejection: not a policy
/// refusal (`refused: true`, budget/repeat), not "ran, found nothing"
/// (`ok: false, refused: false`, no gate prefix) — specifically the
/// structural/schema gate naming a citation that does not correspond to
/// real structure.
pub fn should_auto_correct(ok: bool, refused: bool, text: &str) -> bool {
    !ok && !refused && text.starts_with(eg_mcp::tools::STRUCTURAL_GATE_PREFIX)
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

/// The corrective tool to run, and the arguments to run it with, for a
/// rejected call made with `args`.
pub fn correction(args: &Value) -> (&'static str, Value) {
    let mut correction_args = json!({});
    if let Some(sheet) = named_sheet(args) {
        correction_args["sheet"] = json!(sheet);
    }
    if let Some(wb) = args.get("workbook").cloned() {
        correction_args["workbook"] = wb;
    }
    ("tables", correction_args)
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
        assert!(should_auto_correct(false, false, &text));
        assert!(
            !should_auto_correct(true, false, &text),
            "ok, not a rejection"
        );
        assert!(
            !should_auto_correct(false, true, &text),
            "a policy refusal, not this gate"
        );
        assert!(
            !should_auto_correct(
                false,
                false,
                "no sheet called \"Foo\". This workbook has: …"
            ),
            "a different rejection shape entirely"
        );
    }

    #[test]
    fn query_table_corrects_with_tables_scoped_to_the_named_sheet() {
        let args = json!({ "table": "'Sales'!Z1:AA9", "workbook": "book.xlsx" });
        let (tool, correction_args) = correction(&args);
        assert_eq!(tool, "tables");
        assert_eq!(correction_args["sheet"], json!("Sales"));
        assert_eq!(correction_args["workbook"], json!("book.xlsx"));
    }

    #[test]
    fn a_cell_tool_corrects_with_tables_on_the_same_sheet() {
        let args = json!({ "citation": "Sales!Z1:AA9" });
        let (tool, correction_args) = correction(&args);
        assert_eq!(tool, "tables");
        assert_eq!(correction_args["sheet"], json!("Sales"));
    }

    #[test]
    fn a_citation_naming_no_sheet_corrects_unscoped() {
        let args = json!({ "citation": "not a range" });
        let (tool, correction_args) = correction(&args);
        assert_eq!(tool, "tables");
        assert!(correction_args.get("sheet").is_none());
    }
}
