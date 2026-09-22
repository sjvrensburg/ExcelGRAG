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
use serde_json::{json, Value};

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

fn grid(id: u16, name: &str, rows: &[&str]) -> eg_model::Sheet {
    let mut sheet = eg_model::Sheet::new(eg_model::SheetId(id), name);
    for (r, line) in rows.iter().enumerate() {
        for (c, token) in line.split('|').enumerate() {
            let token = token.trim();
            if token.is_empty() || token == "." {
                continue;
            }
            let cell = match token.strip_prefix('=') {
                Some(formula) => eg_model::Cell {
                    value: eg_model::CellValue::Number(0.0),
                    formula: Some(formula.to_string()),
                    format: Default::default(),
                },
                None => match token.parse::<f64>() {
                    Ok(n) => eg_model::Cell::literal(eg_model::CellValue::Number(n)),
                    Err(_) => eg_model::Cell::literal(eg_model::CellValue::Text(token.to_string())),
                },
            };
            sheet.set(r as u32, c as u16, cell);
        }
    }
    sheet
}

/// A real file for `find_value` to open when the auto-scan runs — the
/// synthetic sheets above only ever back the lexical index, and `search`
/// never opens the workbook itself, but a scan does. The demo fixture is
/// committed and unrelated in content; only its existence on disk matters
/// here.
fn demo_fixture_path() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/demo/impairment.xlsx")
        .canonicalize()
        .expect("the committed demo fixture exists")
        .to_string_lossy()
        .into_owned()
}

/// The debtors book from `eg-retrieve`'s own retrieval-floor fixture
/// (`crates/eg-retrieve/tests/answers.rs`), which is already known to answer
/// `search "colour of the invoice paper"` with a genuine `Blind` verdict —
/// none of its words are in the index, and the query is short enough that
/// nothing else in the corpus is found on the side. Reused rather than
/// invented afresh: engineering a corpus that is blind on a real BM25
/// ranking (as opposed to the `Nothing` verdict a totally empty hit list
/// gives) is fiddly to get right, and this one is already proven to.
fn debtors_workbook() -> eg_model::Workbook {
    eg_model::Workbook {
        path: demo_fixture_path(),
        format: Some(eg_model::WorkbookFormat::Xlsx),
        content_hash: "hash-answers".into(),
        sheets: vec![
            grid(
                0,
                "Work Doc",
                &[
                    "Customer | Debt Type | Total Debt | Discount Rate | PV of expected receipts | Impairment provision",
                    "North | Retail | 1200 | =VLOOKUP(B2,Rates!A:B,2,FALSE) | =C2/(1+D2) | =C2-E2",
                    "South | Business | 3400 | =VLOOKUP(B3,Rates!A:B,2,FALSE) | =C3/(1+D3) | =C3-E3",
                    "East | Retail | 900 | =VLOOKUP(B4,Rates!A:B,2,FALSE) | =C4/(1+D4) | =C4-E4",
                ],
            ),
            grid(
                1,
                "Rates",
                &[
                    "Debt Type | Discount Rate",
                    "Retail | 0.08",
                    "Business | 0.11",
                    "Wholesale | 0.15",
                ],
            ),
        ],
        defined_names: Vec::new(),
        external_links: Vec::new(),
    }
}

fn debtors_engine() -> (Arc<Mutex<eg_mcp::State>>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().to_str().expect("utf-8 path");
    let wb = debtors_workbook();
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

/// The blind query itself: none of "colour", "invoice" or "paper" is
/// anywhere in the corpus, and "1050" — a number nobody wrote down — parses
/// as a word `search` would otherwise have to shrug at. Genuinely `Blind`,
/// not merely `Nothing`: the raw lexical query still matches something (a
/// column that happens to share a frame word with it), which is what gives
/// `search` hits to rank at all. See [`debtors_workbook`].
const BLIND_QUERY: &str = "colour of the invoice paper 1050";

#[tokio::test]
async fn a_blind_result_on_a_number_is_scanned_automatically() {
    let (engine, _dir) = debtors_engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call(
            "c1",
            "search",
            json!({ "query": BLIND_QUERY, "lexical_only": true }),
        )],
        vec![AssistantContent::text(
            "1050 is not a value in this workbook.",
        )],
    ]);
    let harness = Harness::new(model.clone(), Policy::default());
    let mut tool_calls: Vec<(String, Value)> = Vec::new();
    let outcome = harness
        .ask(engine, BLIND_QUERY, &mut |e: Event| {
            if let Event::ToolCall { name, args, .. } = e {
                tool_calls.push((name, args));
            }
        })
        .await
        .expect("the run completes");

    // The model was never asked for a second tool call — the scan happened
    // inside the turn that got the blind result, not as a retry that cost a
    // model call.
    assert_eq!(outcome.turns, 2);
    let names: Vec<&str> = outcome.calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["search", "find_value"]);
    assert!(outcome.calls.iter().all(|c| c.ok && !c.refused));
    assert_eq!(outcome.calls[1].args, json!({ "value": 1050 }));
    // Reported to the host as its own step, same as a call the model made.
    assert_eq!(
        tool_calls,
        vec![
            (
                "search".to_string(),
                json!({ "query": BLIND_QUERY, "lexical_only": true })
            ),
            ("find_value".to_string(), json!({ "value": 1050 })),
        ]
    );

    // And the model was told, in the same turn's result, without a round
    // trip to ask for it.
    let requests = model.requests.lock().unwrap();
    let second = serde_json::to_string(&requests[1].chat_history).unwrap();
    assert!(second.contains("Scanned automatically"), "{second}");
    assert!(second.contains("not in this workbook"), "{second}");
}

#[tokio::test]
async fn an_auto_scan_respects_the_scan_budget() {
    let (engine, _dir) = debtors_engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call(
            "c1",
            "search",
            json!({ "query": BLIND_QUERY, "lexical_only": true }),
        )],
        vec![AssistantContent::text("done")],
    ]);
    // `find_value` is one of `SCAN_TOOLS`; a budget of zero refuses it
    // before it runs, exactly as it would a model-issued call.
    let policy = Policy {
        max_scans: 0,
        ..Policy::default()
    };
    let harness = Harness::new(model, policy);
    let outcome = harness
        .ask(engine, BLIND_QUERY, &mut |_| {})
        .await
        .expect("the run completes");

    let names: Vec<&str> = outcome.calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["search"], "no scan when the budget is spent");
}

/// A range far outside anything on the demo fixture's `Debtors` sheet —
/// syntactically fine, semantically nowhere, the shape a hallucinated
/// coordinate takes. See `demo_fixture_path`: the real file on disk is
/// unrelated in content to the synthetic `debtors_workbook`, but its real
/// sheet names are what `resolve_range` checks against, and `Debtors` is
/// one of them.
const HALLUCINATED_RANGE: &str = "Debtors!ZZ9000:AAA9010";

#[tokio::test]
async fn a_structural_gate_rejection_is_corrected_automatically() {
    let (engine, _dir) = debtors_engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call(
            "c1",
            "read_cells",
            json!({ "citation": HALLUCINATED_RANGE }),
        )],
        vec![AssistantContent::text("done")],
    ]);
    let harness = Harness::new(model.clone(), Policy::default());
    let outcome = harness
        .ask(engine, "what is in that range?", &mut |_| {})
        .await
        .expect("the run completes");

    // No second model call spent on the correction — it happens inside the
    // turn that got the rejection, exactly as `auto_scan` does for a blind
    // search result.
    assert_eq!(outcome.turns, 2);
    let names: Vec<&str> = outcome.calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["read_cells", "tables"]);
    assert!(!outcome.calls[0].ok && !outcome.calls[0].refused);
    assert!(outcome.calls[1].ok && !outcome.calls[1].refused);
    assert_eq!(outcome.calls[1].args, json!({ "sheet": "Debtors" }));

    let requests = model.requests.lock().unwrap();
    let second = serde_json::to_string(&requests[1].chat_history).unwrap();
    assert!(second.contains("Looked automatically"), "{second}");
    assert!(
        second.contains(eg_mcp::tools::STRUCTURAL_GATE_PREFIX),
        "{second}"
    );
}

#[tokio::test]
async fn an_auto_correction_respects_the_repeat_call_dedupe() {
    let (engine, _dir) = debtors_engine();
    let model = Scripted::new(vec![
        vec![AssistantContent::tool_call(
            "c1",
            "tables",
            json!({ "sheet": "Debtors" }),
        )],
        vec![AssistantContent::tool_call(
            "c2",
            "read_cells",
            json!({ "citation": HALLUCINATED_RANGE }),
        )],
        vec![AssistantContent::text("done")],
    ]);
    let harness = Harness::new(model, Policy::default());
    let outcome = harness
        .ask(engine, "what is in that range?", &mut |_| {})
        .await
        .expect("the run completes");

    // The model already called `tables --sheet Debtors` itself; the
    // correction is the identical call, so it is refused rather than
    // repeated, the same as a model-issued repeat would be.
    let names: Vec<&str> = outcome.calls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["tables", "read_cells"]);
}
