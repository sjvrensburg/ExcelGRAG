//! The loop, driven by a model that is a script.
//!
//! A test may not depend on a model server, so the model here is a list of
//! turns: what it "says" on each call, tool calls and all. That is enough to
//! prove the harness's own contract — every tool call reaches the engine,
//! every result reaches the model, the policy's refusals are results the
//! model reads, a misnamed tool is corrected rather than fatal, and the
//! trail records all of it in order. What a real model does with the tools
//! is measured by `eg-agent --score`, not asserted here.

use std::sync::{Arc, Mutex};

use eg_agent::{Event, Harness, Policy};
use eg_model::{CellValue, Workbook};
use rig_core::completion::message::AssistantContent;
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, Usage,
};
use rig_core::streaming::StreamingCompletionResponse;
use serde_json::json;

/// A model whose every reply is written down in advance. Each call pops the
/// next turn; it records what it was asked, so a test can check that tool
/// results made it into the history.
#[derive(Clone)]
struct Scripted {
    turns: Arc<Mutex<Vec<Vec<AssistantContent>>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Scripted {
    fn new(turns: Vec<Vec<AssistantContent>>) -> Self {
        let mut turns = turns;
        turns.reverse();
        Scripted {
            turns: Arc::new(Mutex::new(turns)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl CompletionModel for Scripted {
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        self.requests.lock().unwrap().push(request);
        let choice = self
            .turns
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| CompletionError::ResponseError("script exhausted".into()))?;
        Ok(CompletionResponse::new(
            choice,
            Usage::default(),
            "scripted",
        ))
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        Err(CompletionError::ResponseError(
            "scripted model does not stream".into(),
        ))
    }
}

fn workbook() -> Workbook {
    let mut sheet = eg_model::Sheet::new(eg_model::SheetId(0), "Sales");
    let rows = ["Region Revenue", "North 10", "South 20", "East 30"];
    for (r, line) in rows.iter().enumerate() {
        for (c, tok) in line.split_whitespace().enumerate() {
            let value = match tok.parse::<f64>() {
                Ok(n) => CellValue::Number(n),
                Err(_) => CellValue::Text(tok.to_string()),
            };
            sheet.set(r as u32, c as u16, eg_model::Cell::literal(value));
        }
    }
    Workbook {
        path: "sales.xlsx".into(),
        format: Some(eg_model::WorkbookFormat::Xlsx),
        content_hash: "hash-sales".into(),
        sheets: vec![sheet],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

fn engine() -> (Arc<Mutex<eg_mcp::State>>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().to_str().expect("utf-8 path");
    let wb = workbook();
    let built = eg_graph::build(&wb);
    let mut corpus = eg_graph::store::Corpus::open(path).expect("corpus opens");
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
    let mut text = eg_index::TextIndex::open(path).expect("text index opens");
    text.index_built(&built, &wb.content_hash, &wb.path)
        .expect("indexed");
    drop(text);
    let state = eg_mcp::State::open(path, false).expect("state opens");
    (Arc::new(Mutex::new(state)), dir)
}

#[tokio::test]
async fn tool_results_reach_the_model_and_the_trail() {
    let (engine, _dir) = engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call("c1", "workbooks", json!({}))],
        vec![AssistantContent::tool_call(
            "c2",
            "search",
            json!({ "query": "revenue", "lexical_only": true }),
        )],
        vec![AssistantContent::text(
            "Revenue is a column on the Sales sheet.",
        )],
    ]);
    let harness = Harness::new(model.clone(), Policy::default());
    let mut events = Vec::new();
    let outcome = harness
        .ask(engine, "where is revenue?", &mut |e: Event| events.push(e))
        .await
        .expect("the run completes");

    assert_eq!(
        outcome.answer.as_deref(),
        Some("Revenue is a column on the Sales sheet.")
    );
    assert_eq!(outcome.turns, 3);
    let names: Vec<&str> = outcome.calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["workbooks", "search"]);
    assert!(outcome.calls.iter().all(|c| c.ok && !c.refused));

    // The tool's text landed in the model's history before its next call.
    let requests = model.requests.lock().unwrap();
    let third = serde_json::to_string(&requests[2].chat_history).unwrap();
    assert!(third.contains("sales.xlsx"), "workbooks result in history");
    assert!(third.contains("Revenue"), "search result in history");
    // And every call carried the whole tool table.
    assert_eq!(requests[0].tools.len(), eg_mcp::tools::TOOLS.len());

    let results: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::ToolResult { .. }))
        .collect();
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn a_refusal_is_a_result_the_model_reads() {
    let (engine, _dir) = engine();
    let same = json!({ "query": "revenue", "lexical_only": true });
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call("c1", "search", same.clone())],
        vec![AssistantContent::tool_call("c2", "search", same)],
        vec![AssistantContent::text("done")],
    ]);
    let harness = Harness::new(model.clone(), Policy::default());
    let outcome = harness
        .ask(engine, "revenue?", &mut |_| {})
        .await
        .expect("the run completes");
    assert_eq!(outcome.calls.len(), 2);
    assert!(outcome.calls[0].ok && !outcome.calls[0].refused);
    assert!(!outcome.calls[1].ok && outcome.calls[1].refused);
    let requests = model.requests.lock().unwrap();
    let third = serde_json::to_string(&requests[2].chat_history).unwrap();
    assert!(third.contains("already called"), "{third}");
}

#[tokio::test]
async fn a_progress_recap_reaches_the_model_from_the_second_call_on() {
    let (engine, _dir) = engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call(
            "c1",
            "search",
            json!({ "query": "revenue", "lexical_only": true }),
        )],
        vec![AssistantContent::text("done")],
    ]);
    let harness = Harness::new(model.clone(), Policy::default());
    harness
        .ask(engine, "where is revenue?", &mut |_| {})
        .await
        .expect("the run completes");

    let requests = model.requests.lock().unwrap();
    // Turn 1 has made no calls yet, so its preamble carries no recap.
    let first = serde_json::to_string(&requests[0]).unwrap();
    assert!(!first.contains("Progress so far"), "{first}");
    // Turn 2 is told what turn 1 already found.
    let second = serde_json::to_string(&requests[1]).unwrap();
    assert!(second.contains("Progress so far"), "{second}");
    assert!(second.contains("search"), "{second}");
}

#[tokio::test]
async fn an_old_result_is_elided_once_it_no_longer_fits_the_budget() {
    let (engine, _dir) = engine();
    // The freshest tool result is always the next call's `prompt`, not part
    // of `history` — so eliding anything requires at least two prior tool
    // calls: the first ages into `history` once the second has run.
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call(
            "c1",
            "search",
            json!({ "query": "revenue", "lexical_only": true }),
        )],
        vec![AssistantContent::tool_call("c2", "tables", json!({}))],
        vec![AssistantContent::text("done")],
    ]);
    // A budget of one token is smaller than any real tool result, so the
    // oldest tool-result-bearing message in history is a candidate the
    // moment there is one; `keep_recent: 0` protects nothing further.
    let policy = Policy {
        elision: eg_agent::elide::ElisionConfig {
            soft_token_budget: 1,
            keep_recent: 0,
        },
        ..Policy::default()
    };
    let harness = Harness::new(model.clone(), policy);
    harness
        .ask(engine, "where is revenue?", &mut |_| {})
        .await
        .expect("the run completes");

    let requests = model.requests.lock().unwrap();
    let third: serde_json::Value = serde_json::to_value(&requests[2].chat_history).unwrap();
    let messages = third.as_array().expect("chat_history is an array");
    let first_result_text = messages
        .iter()
        .flat_map(|m| {
            m.get("content")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
        })
        .find(|item| item.get("call").and_then(|c| c.as_str()) == Some("c1"))
        .and_then(|item| item.get("content").and_then(|c| c.as_array()))
        .and_then(|c| c.first())
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .expect("the first search call's tool result is present");
    assert!(
        first_result_text.starts_with("[elided:"),
        "{first_result_text}"
    );
}

#[tokio::test]
async fn a_short_run_under_budget_is_never_elided() {
    let (engine, _dir) = engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call(
            "c1",
            "search",
            json!({ "query": "revenue", "lexical_only": true }),
        )],
        vec![AssistantContent::text(
            "Revenue is a column on the Sales sheet.",
        )],
    ]);
    let harness = Harness::new(model.clone(), Policy::default());
    harness
        .ask(engine, "where is revenue?", &mut |_| {})
        .await
        .expect("the run completes");

    let requests = model.requests.lock().unwrap();
    let second = serde_json::to_string(&requests[1].chat_history).unwrap();
    assert!(!second.contains("[elided:"), "{second}");
}

#[tokio::test]
async fn a_misnamed_tool_is_corrected_not_fatal() {
    let (engine, _dir) = engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call(
            "c1",
            "grep",
            json!({ "q": "x" }),
        )],
        vec![AssistantContent::text("there is no grep")],
    ]);
    // Answering after the correction is still answering with no evidence;
    // that rule is tested on its own, so it is off here.
    let policy = Policy {
        max_ungrounded_retries: 0,
        ..Policy::default()
    };
    let harness = Harness::new(model.clone(), policy);
    let mut unknown = Vec::new();
    let outcome = harness
        .ask(engine, "?", &mut |e: Event| {
            if let Event::UnknownTool { name, .. } = e {
                unknown.push(name);
            }
        })
        .await
        .expect("the run completes");
    assert_eq!(unknown, ["grep"]);
    assert_eq!(outcome.answer.as_deref(), Some("there is no grep"));
    assert!(
        outcome.calls.is_empty(),
        "a skipped call never reached a tool"
    );
    let requests = model.requests.lock().unwrap();
    let second = serde_json::to_string(&requests[1].chat_history).unwrap();
    assert!(second.contains("no tool called `grep`"), "{second}");
}

#[tokio::test]
async fn running_out_of_turns_is_an_outcome() {
    let (engine, _dir) = engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call("c1", "workbooks", json!({}))],
        vec![AssistantContent::tool_call("c2", "tables", json!({}))],
    ]);
    let policy = Policy {
        max_turns: 2,
        ..Policy::default()
    };
    let outcome = Harness::new(model, policy)
        .ask(engine, "?", &mut |_| {})
        .await
        .expect("a budget is not an error");
    assert!(outcome.answer.is_none());
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.calls.len(), 2);
}

#[tokio::test]
async fn an_answer_before_any_tool_is_sent_back() {
    let (engine, _dir) = engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::text(
            "Revenue is obviously on the first sheet.",
        )],
        vec![AssistantContent::tool_call(
            "c1",
            "search",
            json!({ "query": "revenue", "lexical_only": true }),
        )],
        vec![AssistantContent::text(
            "Revenue is a column on the Sales sheet.",
        )],
    ]);
    let harness = Harness::new(model.clone(), Policy::default());
    let mut sent_back = 0;
    let outcome = harness
        .ask(engine, "where is revenue?", &mut |e: Event| {
            if matches!(e, Event::Ungrounded { .. }) {
                sent_back += 1;
            }
        })
        .await
        .expect("the run completes");
    assert_eq!(sent_back, 1);
    assert_eq!(
        outcome.answer.as_deref(),
        Some("Revenue is a column on the Sales sheet.")
    );
    assert_eq!(outcome.calls.len(), 1);
    let requests = model.requests.lock().unwrap();
    // The second call carries the rejected reply and the correction.
    let second = serde_json::to_string(&requests[1].chat_history).unwrap();
    assert!(second.contains("obviously"), "{second}");
    assert!(second.contains("not called any tool"), "{second}");
    // And every call told the model which workbooks the corpus holds.
    let first = serde_json::to_string(&requests[0]).unwrap();
    assert!(first.contains("sales.xlsx"), "{first}");
}

#[tokio::test]
async fn an_ungrounded_answer_is_accepted_once_the_retries_are_spent() {
    let (engine, _dir) = engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::text("first guess")],
        vec![AssistantContent::text("second guess")],
    ]);
    let policy = Policy {
        max_ungrounded_retries: 1,
        ..Policy::default()
    };
    let outcome = Harness::new(model, policy)
        .ask(engine, "?", &mut |_| {})
        .await
        .expect("the run completes");
    assert_eq!(outcome.answer.as_deref(), Some("second guess"));
    assert!(outcome.calls.is_empty());
}

#[tokio::test]
async fn an_empty_final_reply_is_sent_back() {
    let (engine, _dir) = engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call(
            "c1",
            "search",
            json!({ "query": "revenue", "lexical_only": true }),
        )],
        // A reasoning model that spent its whole budget thinking.
        vec![AssistantContent::text("")],
        vec![AssistantContent::text(
            "Revenue is a column on the Sales sheet.",
        )],
    ]);
    let harness = Harness::new(model.clone(), Policy::default());
    let mut sent_back = 0;
    let outcome = harness
        .ask(engine, "where is revenue?", &mut |e: Event| {
            if matches!(e, Event::Ungrounded { .. }) {
                sent_back += 1;
            }
        })
        .await
        .expect("the run completes");
    assert_eq!(sent_back, 1);
    assert_eq!(
        outcome.answer.as_deref(),
        Some("Revenue is a column on the Sales sheet.")
    );
    let requests = model.requests.lock().unwrap();
    let third = serde_json::to_string(&requests[2].chat_history).unwrap();
    assert!(third.contains("reply was empty"), "{third}");
}
