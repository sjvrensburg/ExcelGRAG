//! A chat turn the model drives itself.
//!
//! `chat::run_turn` is the fixed pipeline — condense, find→expand→render,
//! phrase — and it answers "where is the provision" well. This is the other
//! kind of turn: the configured model is handed the `eg-mcp` tools through
//! `eg-agent`'s harness and chooses its own way through the workbook. Every
//! step is broadcast to every tab as it lands (`WsEvent::AgentStep`), the
//! camera follows each citation the model reads (`WsEvent::Navigate`, the
//! same open-then-select an agent's `gui_show` uses), and the finished turn
//! is committed to the shared session like any other, carrying the trail of
//! tool calls it made — arguments and verdicts, never results.
//!
//! The privacy dial applies to the harness by the same rule as to the
//! pipeline: `off` means no investigation at all (the turn falls back to
//! the pipeline); `passage` runs every tool with values redacted to kinds,
//! so the model can search, trace and check without a cell's content
//! reaching it; `values` runs the tools as the engine was opened. The same
//! lock discipline holds: the engine lock is taken per tool call inside the
//! harness and never across a model call.

use std::sync::Arc;

use eg_agent::{Event, Harness, Policy};
use serde_json::Value;

use crate::app::App;
use crate::chat::{self, ChatTurn, TurnSource};
use crate::dto::{self, AgentStepDto, ChatTurnDto, TrailStepDto, WsEvent};

/// Longest tool-result text a live step carries to the browser. The model
/// sees the whole result; a tab needs the shape of it, not two thousand
/// rows.
const LIVE_RESULT_CHARS: usize = 4000;

/// Run one investigation and commit it as a turn of `session_id`.
pub async fn run_investigation(
    app: &Arc<App>,
    session_id: &str,
    source: TurnSource,
    message: &str,
) -> Result<ChatTurnDto, String> {
    let llm = app
        .llm()
        .ok_or("no chat model is configured — set one in the Chat model panel first")?;
    if !llm.privacy.allows_llm() {
        return Err(
            "the chat model's privacy tier is `off`; an investigation needs a model".into(),
        );
    }
    let (base_url, api_key, model_name) = llm.connection();
    let model = eg_agent::openai_compatible(base_url, api_key, model_name)?;
    let harness =
        Harness::new(model, Policy::default()).with_redacted_values(!llm.privacy.allows_values());

    // Where the camera looks: the session's sticky workbook, else the
    // corpus's only one. The graph is loaded once per turn (a file read
    // and a parse, the same `GET /api/graph` does per request) and used
    // for every citation the model touches.
    let workbook = {
        let sessions = app.sessions();
        sessions.get(session_id).and_then(|s| s.workbook.clone())
    };
    let graph = {
        let app = Arc::clone(app);
        tokio::task::spawn_blocking(move || {
            let state = app.engine();
            let (hash, _) = state.resolve(workbook.as_deref()).ok()?;
            let stored = state.corpus.get(&hash).ok().flatten()?;
            Some((hash, stored.graph))
        })
        .await
        .unwrap_or(None)
    };

    let engine = app.engine_handle();
    let mut sink = |event: Event| {
        if let Event::ToolCall { name, args, .. } = &event {
            if let Some((hash, graph)) = &graph {
                for citation in cited_ranges(name, args) {
                    if let Some(node) = dto::node_for_citation(graph, &citation) {
                        app.send(WsEvent::Navigate {
                            session_id: session_id.to_string(),
                            node,
                            workbook: Some(hash.clone()),
                        });
                    }
                }
            }
        }
        app.send(WsEvent::AgentStep {
            session_id: session_id.to_string(),
            step: step_dto(event),
        });
    };
    let outcome = harness
        .ask(engine, message, &mut sink)
        .await
        .map_err(|e| format!("the investigation failed: {e}"))?;

    let trail: Vec<TrailStepDto> = outcome
        .calls
        .iter()
        .map(|c| TrailStepDto {
            turn: c.turn,
            name: c.name.clone(),
            args: c.args.clone(),
            ok: c.ok,
            refused: c.refused,
        })
        .collect();
    let answer = outcome.answer.clone().unwrap_or_else(|| {
        format!(
            "(no answer: the investigation ran out of turns after {} tool call(s))",
            outcome.calls.len()
        )
    });
    // What the turn cites: the ranges the model asked tools about, and the
    // ranges its reply names that some tool result actually carried — a
    // range the model wrote that no tool ever returned is not a citation.
    let mut citations: Vec<String> = Vec::new();
    for call in &outcome.calls {
        if !call.ok {
            continue;
        }
        for citation in cited_ranges(&call.name, &call.args) {
            if !citations.contains(&citation) {
                citations.push(citation);
            }
        }
    }
    for range in ranges_in(&answer) {
        let carried = outcome
            .calls
            .iter()
            .any(|c| c.ok && c.result.contains(&range));
        if carried && !citations.contains(&range) {
            citations.push(range);
        }
    }
    // The camera ends on the first thing the reply cites.
    if let (Some((hash, graph)), Some(first)) = (&graph, citations.first()) {
        if let Some(node) = dto::node_for_citation(graph, first) {
            app.send(WsEvent::Navigate {
                session_id: session_id.to_string(),
                node,
                workbook: Some(hash.clone()),
            });
        }
    }
    let evidence = format!(
        "investigation: {} model call(s), {} tool call(s) — {}",
        outcome.turns,
        outcome.calls.len(),
        summarise(&outcome.calls)
    );
    let turn = ChatTurn {
        evidence,
        citations,
        answer,
        trail,
        ..ChatTurn::new(source, message)
    };
    let sticky = graph.as_ref().map(|(hash, _)| (Some(hash.clone()), None));
    chat::commit_turn(app, session_id, turn, sticky)
}

/// The ranges a tool call names, by the argument each tool takes them in.
fn cited_ranges(tool: &str, args: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |v: Option<&Value>| {
        if let Some(s) = v.and_then(Value::as_str) {
            out.push(s.to_string());
        }
    };
    match tool {
        "read_cells" | "precedents" | "dependents" | "recompute" | "graph" => {
            push(args.get("citation"))
        }
        "query_table" => push(args.get("table")),
        "what_if" => {
            for change in args
                .get("changes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                push(change.get("citation"));
            }
        }
        _ => {}
    }
    out
}

/// Every `Sheet!A1:B2`-shaped token in a reply, quoted sheet names
/// included, in order of appearance. A syntactic scan, not a parse — what
/// it finds is then checked against what the tools returned.
fn ranges_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // A sheet name: quoted, or a run of word characters; then `!`.
        let start = i;
        let end_of_sheet = if bytes[i] == b'\'' {
            match text[i + 1..].find('\'') {
                Some(n) => i + 1 + n + 1,
                None => break,
            }
        } else {
            let mut j = i;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            j
        };
        if end_of_sheet > start && bytes.get(end_of_sheet) == Some(&b'!') {
            let mut j = end_of_sheet + 1;
            let ref_start = j;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'$' || bytes[j] == b':')
            {
                j += 1;
            }
            let candidate = &text[start..j];
            if j > ref_start && eg_model::parse_a1(candidate).is_ok() {
                out.push(candidate.trim_end_matches(':').to_string());
            }
            i = j.max(start + 1);
        } else {
            i = end_of_sheet.max(start + 1);
        }
    }
    out
}

/// `search, context, read_cells ×2` — the tools in order, runs collapsed.
fn summarise(calls: &[eg_agent::CallRecord]) -> String {
    let mut parts: Vec<(String, usize)> = Vec::new();
    for call in calls {
        match parts.last_mut() {
            Some((name, n)) if *name == call.name => *n += 1,
            _ => parts.push((call.name.clone(), 1)),
        }
    }
    parts
        .iter()
        .map(|(name, n)| {
            if *n > 1 {
                format!("{name} ×{n}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn step_dto(event: Event) -> AgentStepDto {
    match event {
        Event::ModelCall { turn } => AgentStepDto::ModelCall { turn },
        Event::ModelReasoning { turn, text } => AgentStepDto::ModelReasoning {
            turn,
            text: clip(&text, LIVE_RESULT_CHARS),
        },
        Event::ModelText { turn, text } => AgentStepDto::ModelText { turn, text },
        Event::ToolCall { turn, name, args } => AgentStepDto::ToolCall { turn, name, args },
        Event::ToolResult {
            turn,
            name,
            ok,
            refused,
            text,
        } => AgentStepDto::ToolResult {
            turn,
            name,
            ok,
            refused,
            text: clip(&text, LIVE_RESULT_CHARS),
        },
        Event::UnknownTool { turn, name } => AgentStepDto::UnknownTool { turn, name },
        Event::Ungrounded { turn, reason } => AgentStepDto::SentBack { turn, reason },
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} more bytes)", &text[..end], text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_tool_that_takes_a_range_is_followed() {
        assert_eq!(
            cited_ranges("read_cells", &json!({"citation": "Debtors!H2:H2001"})),
            ["Debtors!H2:H2001"]
        );
        assert_eq!(
            cited_ranges("query_table", &json!({"table": "'Debtors'!B2:M2001"})),
            ["'Debtors'!B2:M2001"]
        );
        assert_eq!(
            cited_ranges(
                "what_if",
                &json!({"changes": [{"citation": "Rates!B11", "value": 0.2}, {"citation": "Rates!B4", "value": 0.1}]})
            ),
            ["Rates!B11", "Rates!B4"]
        );
        assert!(cited_ranges("search", &json!({"query": "Rates!B11"})).is_empty());
    }

    #[test]
    fn ranges_in_a_reply_are_found_whole() {
        let text = "the \"Discount Rate\" column (Debtors!H2:H2001) reads Rates!$A$4:$B$7 and \
                    'Q3 Sales'!B2; not a range: Debtors, x!y, 12!34.";
        assert_eq!(
            ranges_in(text),
            ["Debtors!H2:H2001", "Rates!$A$4:$B$7", "'Q3 Sales'!B2"]
        );
    }

    #[test]
    fn a_trail_summary_collapses_runs() {
        let calls: Vec<eg_agent::CallRecord> = ["search", "context", "read_cells", "read_cells"]
            .iter()
            .map(|name| eg_agent::CallRecord {
                turn: 1,
                name: name.to_string(),
                args: json!({}),
                ok: true,
                refused: false,
                result: String::new(),
            })
            .collect();
        assert_eq!(summarise(&calls), "search, context, read_cells ×2");
    }
}
