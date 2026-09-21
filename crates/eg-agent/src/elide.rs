//! Rule-based, non-recoverable context elision.
//!
//! arXiv 2609.20804 found that simple rule-based elision staged before an
//! expensive model call beats fancier context management, and that a
//! *recoverable* elision buffer — keeping the original text around in case
//! the model asks for it back — earns no accuracy gain in their experiments.
//! This module is deliberately the plainer of the two: once a bulky tool
//! result is elided it is gone, replaced by a short stub the model can read
//! as a fact ("this was here, and is no longer needed verbatim") rather than
//! a promise it can retrieve the original.
//!
//! History is rewritten just before a request is built
//! ([`crate::harness::Harness::ask`]), never inside [`rig_agent::agent::AgentRun`]
//! itself — the run's own history stays intact; only the copy sent to the
//! model on this one call is trimmed.

use rig_core::completion::message::{Message, ToolResultContent, UserContent};

/// How aggressively to elide.
#[derive(Clone, Debug)]
pub struct ElisionConfig {
    /// Once the history's approximate token count exceeds this, the oldest
    /// eligible tool results are stubbed out until it no longer does (or
    /// there is nothing left eligible to stub).
    pub soft_token_budget: usize,
    /// The most recent this many tool-result-bearing messages are never
    /// elided, however large the history — the model always sees what it
    /// just did.
    pub keep_recent: usize,
}

impl Default for ElisionConfig {
    fn default() -> Self {
        ElisionConfig {
            soft_token_budget: 6_000,
            keep_recent: 2,
        }
    }
}

/// Token count for a piece of text, by the same encoding used to budget
/// output tokens elsewhere in the stack. Approximate for a non-OpenAI model,
/// but consistent — the budget only needs to trend the same way the actual
/// context window does, not match it exactly.
pub fn count_tokens(text: &str) -> usize {
    tiktoken_rs::cl100k_base_singleton()
        .encode_ordinary(text)
        .len()
}

/// Elide the oldest, non-protected tool results from `history` until it fits
/// `cfg.soft_token_budget`, or nothing eligible remains. Returns a rewritten
/// copy; `history` itself is untouched.
pub fn elide_history(history: &[Message], cfg: &ElisionConfig) -> Vec<Message> {
    let mut total: usize = history.iter().map(message_tokens).sum();
    if total <= cfg.soft_token_budget {
        return history.to_vec();
    }

    let tool_result_indices: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| has_tool_result(m))
        .map(|(i, _)| i)
        .collect();
    let protect_from = tool_result_indices.len().saturating_sub(cfg.keep_recent);
    let protected: std::collections::HashSet<usize> = tool_result_indices[protect_from..]
        .iter()
        .copied()
        .collect();

    let mut out = history.to_vec();
    for (i, message) in out.iter_mut().enumerate() {
        if total <= cfg.soft_token_budget {
            break;
        }
        if protected.contains(&i) {
            continue;
        }
        let Message::User { content } = message else {
            continue;
        };
        for item in content.iter_mut() {
            let UserContent::ToolResult(result) = item else {
                continue;
            };
            let original_chars: usize = result
                .content
                .iter()
                .filter_map(ToolResultContent::as_text)
                .map(str::len)
                .sum();
            if original_chars == 0 {
                continue;
            }
            let before_tokens: usize = result
                .content
                .iter()
                .filter_map(ToolResultContent::as_text)
                .map(count_tokens)
                .sum();
            let stub = format!(
                "[elided: earlier `{}` result, {original_chars} chars, superseded by more recent calls]",
                result.name
            );
            let after_tokens = count_tokens(&stub);
            result.content = vec![ToolResultContent::text(stub)];
            total = total.saturating_sub(before_tokens.saturating_sub(after_tokens));
        }
    }
    out
}

fn has_tool_result(message: &Message) -> bool {
    matches!(
        message,
        Message::User { content } if content.iter().any(|c| matches!(c, UserContent::ToolResult(_)))
    )
}

fn message_tokens(message: &Message) -> usize {
    count_tokens(&message_text(message))
}

fn message_text(message: &Message) -> String {
    match message {
        Message::System { content } => content.clone(),
        Message::User { content } => content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text().to_string()),
                UserContent::ToolResult(r) => Some(
                    r.content
                        .iter()
                        .filter_map(ToolResultContent::as_text)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                UserContent::Image(_)
                | UserContent::Audio(_)
                | UserContent::Video(_)
                | UserContent::Document(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Message::Assistant { content, .. } => content
            .iter()
            .filter_map(|c| match c {
                rig_core::completion::AssistantContent::Text(t) => Some(t.text().to_string()),
                rig_core::completion::AssistantContent::ToolCall(tc) => {
                    Some(tc.function.arguments.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::{ToolCallId, ToolResult};

    fn user_text(text: &str) -> Message {
        Message::User {
            content: vec![UserContent::text(text)],
        }
    }

    fn tool_result(name: &str, text: &str) -> Message {
        Message::User {
            content: vec![UserContent::ToolResult(ToolResult {
                call: ToolCallId::mint(),
                provider: None,
                name: name.to_string(),
                content: vec![ToolResultContent::text(text)],
            })],
        }
    }

    #[test]
    fn a_short_history_is_untouched() {
        let history = vec![user_text("hi"), tool_result("search", "a few rows")];
        let out = elide_history(&history, &ElisionConfig::default());
        assert_eq!(out.len(), history.len());
        assert!(matches!(&out[1], Message::User { content }
            if matches!(&content[0], UserContent::ToolResult(r) if r.content[0].as_text() == Some("a few rows"))));
    }

    #[test]
    fn the_oldest_bulky_result_is_elided_first() {
        let big = "x".repeat(40_000);
        let history = vec![
            tool_result("query_table", &big),
            tool_result("query_table", &big),
            tool_result("query_table", "the latest, small result"),
        ];
        let cfg = ElisionConfig {
            soft_token_budget: 5_000,
            keep_recent: 1,
        };
        let out = elide_history(&history, &cfg);

        let text_of = |m: &Message| -> String {
            let Message::User { content } = m else {
                panic!("expected a user message")
            };
            let UserContent::ToolResult(r) = &content[0] else {
                panic!("expected a tool result")
            };
            r.content[0].as_text().unwrap().to_string()
        };

        assert!(
            text_of(&out[0]).starts_with("[elided:"),
            "{}",
            text_of(&out[0])
        );
        // The most recent `keep_recent = 1` result is kept verbatim.
        assert_eq!(text_of(&out[2]), "the latest, small result");
    }

    #[test]
    fn tokens_are_counted_not_just_guessed_at_zero() {
        assert!(count_tokens("the quick brown fox") > 0);
        assert_eq!(count_tokens(""), 0);
    }
}
