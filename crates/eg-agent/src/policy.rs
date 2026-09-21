//! What the model may do, and how much of it.
//!
//! Nothing in `eg` mutates a workbook — even `what_if` is an overlay — so
//! approval here is never about safety. It is about **cost**: `dependents`
//! and `find_value` each scan every formula or every cell of a workbook,
//! which on the reference file is tens of millions of cells, and a model in
//! a loop will happily fire one per turn. The budget counts them, and past
//! it a call is refused with a reason the model reads as the tool's result,
//! so it can change course rather than the run simply failing.
//!
//! arXiv 2609.20804 found that a weak model does better when a budget's
//! refusal is a reminder it reads and can act on, rather than the run simply
//! terminating — [`Policy::admit`] already works this way and was evaluated
//! against the paper and left unchanged on purpose.

use std::collections::BTreeMap;

use crate::elide::ElisionConfig;

/// The two tools that cost a full scan of a workbook. Everything else is a
/// lookup, a bounded walk, or a formula's own text.
pub const SCAN_TOOLS: &[&str] = &["dependents", "find_value"];

#[derive(Clone, Debug)]
pub struct Policy {
    /// Most model calls in one run. Every tool round trip is a turn, so this
    /// bounds the investigation, not the answer.
    pub max_turns: usize,
    /// Most full-scan tool calls ([`SCAN_TOOLS`]) in one run.
    pub max_scans: usize,
    /// Most calls of any one tool with the same arguments. A model that
    /// repeats a call verbatim has stopped reading the result; refusing the
    /// repeat says so.
    pub max_identical_calls: usize,
    /// Most tokens the model may generate per call. A tool call is a line
    /// and an answer is a paragraph; a small model under a forced tool
    /// choice was seen generating twenty thousand tokens for a one-word
    /// question, and without this cap it would still be going. Not too low
    /// either: a reasoning model's thinking counts against it, and one that
    /// runs out mid-thought answers with nothing.
    pub max_output_tokens: u64,
    /// How long one model call may take before the run gives up on it.
    /// Generous, because a local server shared with other work queues the
    /// call behind whatever it is already generating, and a reasoning
    /// model can spend minutes thinking before its first token.
    pub model_timeout: std::time::Duration,
    /// How many times a reply given before any tool has run is sent back.
    /// Zero accepts such a reply as the answer.
    pub max_ungrounded_retries: usize,
    /// How the history sent on each model call is trimmed once it grows
    /// past a soft budget. Budget-adjacent like the rest of this struct, not
    /// a safety gate — see [`crate::elide`].
    pub elision: ElisionConfig,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            max_turns: 12,
            max_scans: 2,
            max_identical_calls: 1,
            max_output_tokens: 4096,
            model_timeout: std::time::Duration::from_secs(600),
            max_ungrounded_retries: 2,
            elision: ElisionConfig::default(),
        }
    }
}

/// What the policy has already let through, so a budget is a running count.
#[derive(Default, Debug)]
pub struct Ledger {
    scans: usize,
    seen: BTreeMap<String, usize>,
}

impl Policy {
    /// Allow a call, or refuse it with the sentence the model gets back.
    pub fn admit(
        &self,
        ledger: &mut Ledger,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<(), String> {
        let key = format!("{name}{}", canonical(args));
        let count = ledger.seen.entry(key).or_insert(0);
        if *count >= self.max_identical_calls {
            return Err(format!(
                "`{name}` was already called with exactly these arguments in this run; \
                 the result has not changed. Read the earlier result, or call it \
                 with different arguments."
            ));
        }
        if SCAN_TOOLS.contains(&name) {
            if ledger.scans >= self.max_scans {
                return Err(format!(
                    "`{name}` scans every cell or formula of the workbook and this run \
                     has used its budget of {} such scans. Narrow the question with \
                     `search`, `context` or `precedents` instead, or answer with what \
                     is already known.",
                    self.max_scans
                ));
            }
            ledger.scans += 1;
        }
        *count += 1;
        Ok(())
    }
}

/// A JSON value with every object's keys sorted, so the same arguments in
/// another order are the same call. `serde_json`'s own `Display` follows
/// insertion order once any crate in the build turns on `preserve_order`.
fn canonical(value: &serde_json::Value) -> String {
    fn walk(value: &serde_json::Value, out: &mut String) {
        match value {
            serde_json::Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_unstable();
                out.push('{');
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::Value::String((*key).clone()).to_string());
                    out.push(':');
                    walk(&map[*key], out);
                }
                out.push('}');
            }
            serde_json::Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    walk(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    walk(value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_repeat_key_ignores_argument_order() {
        assert_eq!(
            canonical(&json!({"b": [1, {"y": 2, "x": 1}], "a": "s"})),
            canonical(&json!({"a": "s", "b": [1, {"x": 1, "y": 2}]}))
        );
        assert_ne!(canonical(&json!({"a": 1})), canonical(&json!({"a": 2})));
    }

    #[test]
    fn scans_are_budgeted_and_repeats_refused() {
        let policy = Policy {
            max_scans: 1,
            ..Policy::default()
        };
        let mut ledger = Ledger::default();
        assert!(policy
            .admit(&mut ledger, "search", &json!({"query": "x"}))
            .is_ok());
        assert!(policy
            .admit(&mut ledger, "search", &json!({"query": "x"}))
            .is_err());
        assert!(policy
            .admit(&mut ledger, "search", &json!({"query": "y"}))
            .is_ok());
        assert!(policy
            .admit(&mut ledger, "find_value", &json!({"value": 1}))
            .is_ok());
        let refused = policy
            .admit(&mut ledger, "dependents", &json!({"citation": "A!B1"}))
            .unwrap_err();
        assert!(refused.contains("budget"), "{refused}");
    }
}
