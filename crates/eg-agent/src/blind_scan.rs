//! Turns a `search` call's "blind on a number" banner into a `find_value`
//! scan, instead of leaving it to a model that may not take the hint.
//!
//! `search`'s own evidence line already says, in words, when a question
//! carried a number the index cannot hold
//! (`eg_retrieve::search::Search::unindexable`) — a column's values are kept
//! in the profile only where there were few enough to list, so a figure out
//! of a large numeric column is indexed nowhere and `search` comes back
//! `Blind` on it. The evidence line names the fix (`find_value`, `eg
//! where`), but a model reading it is not guaranteed to act on it — a 4B
//! model handed exactly this sentence in a live run answered from the guess
//! anyway. This module recognises the same banner `search` renders and
//! performs the scan it names, before the model gets a turn to skip it.
//!
//! Detection works on rendered text, not the `Search` value that built it:
//! the harness only ever sees what `eg_mcp::tools::call` returns, a
//! `String`. Matching the exact banner text `eg-retrieve` renders, rather
//! than reimplementing the verdict logic that produces it, keeps the two
//! from silently drifting apart — reword the banner there and this stops
//! firing, loudly, the next time the scorer runs a numeric question.

use serde_json::{json, Value};

/// The literal wording `eg_retrieve::search::Search::warning` and
/// `::evidence` use for this case. See `crates/eg-retrieve/src/search.rs`.
const BLIND_BANNER: &str = "BLIND MATCH";
const SCAN_HINT: &str = "scan the cells (`find_value`, `eg where`)";

/// Most numbers one `search` call's banner triggers a scan for. A query
/// naming a dozen figures is not the case this exists for, and each scan
/// still costs `find_value`'s own budget (`Policy::max_scans`) on top.
pub const MAX_AUTO_SCANS: usize = 3;

/// Whether a `search` or `context` tool's result is a genuinely blind answer
/// on a number this corpus could not index — the case `find_value` exists
/// for. `context` renders the identical banner through the same
/// `Search::warning`/`::evidence` path `search` does (`tools.rs`'s `context`
/// pushes `found.warning()` verbatim), so a model that reached for `context`
/// instead of `search` on a numeric question gets the same automatic scan.
pub fn should_auto_scan(tool_name: &str, ok: bool, text: &str) -> bool {
    (tool_name == "search" || tool_name == "context")
        && ok
        && text.contains(BLIND_BANNER)
        && text.contains(SCAN_HINT)
}

/// The numeric words of a query, in the same terms `eg-retrieve`'s own
/// coverage probe splits one (`term_hits`: split at non-alphanumeric
/// boundaries), kept only where the word parses as a number — the same
/// test `Search::unindexable` filters on. Deduplicated in first-seen order
/// and capped at [`MAX_AUTO_SCANS`].
pub fn numeric_words(query: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for word in query.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let value = if let Ok(i) = word.parse::<i64>() {
            json!(i)
        } else if let Ok(f) = word.parse::<f64>() {
            // Rust's float parser accepts "nan"/"inf"/"infinity" (any case)
            // as words, not just digits, and `json!(f)` turns a non-finite
            // float into `Value::Null` rather than an error — a cell can
            // hold neither, and `find_value` refuses `null` outright. Skip
            // here rather than spend a scan slot on a call guaranteed to
            // fail.
            if !f.is_finite() {
                continue;
            }
            json!(f)
        } else {
            continue;
        };
        if !seen.insert(word.to_string()) {
            continue;
        }
        out.push(value);
        if out.len() >= MAX_AUTO_SCANS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_blind_result_with_a_numeric_scan_hint() {
        let text = "BLIND MATCH: none of \"1612\" appears in what this corpus indexes, \
                     so nothing below was found on the question. Treat it as a guess; \
                     `workbooks` says what is actually indexed. A number can be in a \
                     cell and in no index — scan the cells (`find_value`, `eg where`).";
        assert!(should_auto_scan("search", true, text));
        assert!(
            should_auto_scan("context", true, text),
            "same banner, same fix"
        );
        assert!(!should_auto_scan("query_table", true, text), "wrong tool");
        assert!(!should_auto_scan("search", false, text), "not ok");
        assert!(
            !should_auto_scan("search", true, "NOTHING MATCHED."),
            "no blind banner"
        );
        assert!(
            !should_auto_scan(
                "search",
                true,
                "BLIND MATCH: the best result carries none of \"debt\"."
            ),
            "blind, but no numeric word to scan for"
        );
    }

    #[test]
    fn extracts_numeric_words_deduplicated_and_capped() {
        // Split the same way `eg-retrieve`'s own coverage probe does — at
        // non-alphanumeric boundaries, so "42.5" is the two words "42" and
        // "5" here just as it is there, and a decimal figure is scanned as
        // both.
        let words = numeric_words("what about 1612 and 1612 and 42.5 and 8 and bad debt");
        assert_eq!(words, vec![json!(1612), json!(42), json!(5)]);
    }

    #[test]
    fn ignores_a_query_with_no_number() {
        assert!(numeric_words("bad debt provision").is_empty());
    }

    #[test]
    fn ignores_words_rusts_float_parser_accepts_but_a_cell_cannot_hold() {
        // "nan", "inf" and "infinity" all parse as `f64`, and `json!(f64)`
        // turns a non-finite float into `Value::Null` (serde_json has no
        // JSON representation for either) — silently, not an `Err`. Left
        // unfiltered, a query containing one of these words would hand
        // `find_value` a `{"value": null}` scan that always refuses
        // ("an empty cell is not a value"), spending one of the run's scan
        // slots on a word that was never a number to look for.
        let words = numeric_words("what is the value at infinity and nan and -Infinity cell");
        assert!(words.is_empty(), "{words:?}");
    }
}
