//! An auto-generated recap of what the run has already done.
//!
//! arXiv 2609.20804 found that weak models — the regime a sidecar local
//! model actually runs in — stall or quit early without an explicit
//! persistent plan/progress scaffold: nothing in [`crate::preamble`] tells a
//! model what it has already *learned*, only what order to try tools in.
//! Reinjecting a short recap of the trail on every call keeps a small model
//! oriented, without touching [`rig_agent::agent::AgentRun`]'s own history —
//! it is spliced into the preamble for one request at a time.

use crate::harness::CallRecord;

/// Most recent calls kept in the recap. Older calls are dropped rather than
/// summarised further, so the recap itself never becomes the thing that
/// needs eliding.
const MAX_LINES: usize = 6;

/// Characters of a call's result kept in its recap line.
const RESULT_PREVIEW_CHARS: usize = 160;

/// A recap of the most recent tool calls, ready to append to a preamble.
pub struct ProgressSummary {
    lines: Vec<String>,
}

impl ProgressSummary {
    /// Build a recap from the trail so far. Empty before any call has been
    /// made, so a first-turn request is unaffected.
    pub fn build(calls: &[CallRecord]) -> Self {
        let skip = calls.len().saturating_sub(MAX_LINES);
        let lines = calls.iter().skip(skip).map(line_for).collect();
        ProgressSummary { lines }
    }

    /// Render as a block to append to the preamble, or an empty string
    /// before any call has been made.
    pub fn render(&self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        format!("\n\nProgress so far:\n{}", self.lines.join("\n"))
    }
}

fn line_for(call: &CallRecord) -> String {
    let verdict = if call.refused {
        "refused"
    } else if call.ok {
        "ok"
    } else {
        "declined"
    };
    let preview = truncate(&call.result, RESULT_PREVIEW_CHARS);
    format!("- {}({}) [{verdict}]: {preview}", call.name, call.args)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, ok: bool, refused: bool, result: &str) -> CallRecord {
        CallRecord {
            turn: 1,
            name: name.to_string(),
            args: json!({ "query": "x" }),
            ok,
            refused,
            result: result.to_string(),
        }
    }

    #[test]
    fn no_calls_renders_nothing() {
        assert_eq!(ProgressSummary::build(&[]).render(), "");
    }

    #[test]
    fn a_call_is_named_with_its_verdict_and_a_result_preview() {
        let calls = vec![call("search", true, false, "12 rows")];
        let rendered = ProgressSummary::build(&calls).render();
        assert!(rendered.contains("Progress so far:"));
        assert!(rendered.contains("search"));
        assert!(rendered.contains("[ok]"));
        assert!(rendered.contains("12 rows"));
    }

    #[test]
    fn a_refusal_is_marked_as_refused_not_ok() {
        let calls = vec![call("search", false, true, "already called")];
        let rendered = ProgressSummary::build(&calls).render();
        assert!(rendered.contains("[refused]"));
    }

    #[test]
    fn only_the_most_recent_calls_are_kept() {
        let calls: Vec<CallRecord> = (0..10)
            .map(|i| call(&format!("tool{i}"), true, false, "r"))
            .collect();
        let rendered = ProgressSummary::build(&calls).render();
        assert!(!rendered.contains("tool0"), "{rendered}");
        assert!(rendered.contains("tool9"), "{rendered}");
    }

    #[test]
    fn a_long_result_is_truncated() {
        let long = "x".repeat(1000);
        let calls = vec![call("read_cells", true, false, &long)];
        let rendered = ProgressSummary::build(&calls).render();
        assert!(rendered.len() < long.len());
        assert!(rendered.contains('…'));
    }
}
