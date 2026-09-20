//! The loop: ask the run what to do, do it, feed it back.
//!
//! Rig's [`AgentRun`] is sans-IO — it never calls a model or a tool itself.
//! It hands back a step, the harness performs it, and the run advances. That
//! is what makes every model call and every tool call visible to the host as
//! an [`Event`] before it lands, and what lets the [`Policy`] sit between the
//! model's intent and the tool's execution without a hook system in the way.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use rig_agent::agent::{
    AgentRun, AgentRunStep, InvalidToolCallAction, ModelTurn, ModelTurnOutcome, RetryRequest,
};
use rig_agent::completion::PromptError;
use rig_core::completion::message::{ReasoningContent, ToolChoice, ToolResultContent, UserContent};
use rig_core::completion::{AssistantContent, CompletionModel, ToolDefinition, Usage};
use serde::Serialize;
use serde_json::{json, Value};

use crate::policy::{Ledger, Policy};
use crate::preamble::PREAMBLE;
use crate::tools;

/// One thing the harness did, reported to the host as it happens.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A model call is being made; `turn` is one-based.
    ModelCall { turn: usize },
    /// What the model was thinking before it acted — a reasoning model's
    /// `reasoning_content`, reported once the call returns. It is the why
    /// behind a tool call, and worth the host showing; it is not an answer
    /// and never grounds one.
    ModelReasoning { turn: usize, text: String },
    /// The model replied with text (possibly beside tool calls).
    ModelText { turn: usize, text: String },
    /// The model asked for a tool. Reported before the policy decides.
    ToolCall {
        turn: usize,
        name: String,
        args: Value,
    },
    /// A tool answered. `refused` is the policy's refusal, `ok` the tool's
    /// own verdict; the text is what the model gets back either way.
    ToolResult {
        turn: usize,
        name: String,
        ok: bool,
        refused: bool,
        text: String,
    },
    /// The model named a tool that does not exist; it was told so.
    UnknownTool { turn: usize, name: String },
    /// The model's reply could not stand — given before any tool had run,
    /// or empty — and was sent back with `reason`.
    Ungrounded { turn: usize, reason: String },
}

/// How a run ended.
#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    /// The model's final reply, or `None` when the turn budget ran out
    /// first — which is an outcome, not an error: the trail is still there.
    pub answer: Option<String>,
    /// Model calls made.
    pub turns: usize,
    /// Every tool call the model made, in order, with the policy's and the
    /// tool's verdicts — the trail a host can replay.
    pub calls: Vec<CallRecord>,
    pub usage: Usage,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallRecord {
    pub turn: usize,
    pub name: String,
    pub args: Value,
    pub ok: bool,
    pub refused: bool,
    /// What the model was given back — the evidence an answer can be
    /// checked against.
    pub result: String,
}

/// A model, a policy, and the tool table; reusable across questions.
pub struct Harness<M: CompletionModel + Clone> {
    model: M,
    policy: Policy,
    tools: Vec<ToolDefinition>,
    names: BTreeSet<String>,
    preamble: String,
    /// Provider-specific fields merged into every request body — a local
    /// server's chat-template switches, say. `None` sends nothing extra.
    extra_params: Option<Value>,
    /// Show the model values as their kinds, whatever the engine was opened
    /// with. See [`tools::execute`].
    redact_values: bool,
}

impl<M: CompletionModel + Clone> Harness<M> {
    pub fn new(model: M, policy: Policy) -> Self {
        Harness {
            model,
            policy,
            tools: tools::definitions(),
            names: tools::names().map(str::to_string).collect(),
            preamble: PREAMBLE.to_string(),
            extra_params: None,
            redact_values: false,
        }
    }

    /// Never let a cell value reach the model: every tool result is
    /// redacted to kinds for this harness, as under `--redact-values`.
    pub fn with_redacted_values(mut self, redact: bool) -> Self {
        self.redact_values = redact;
        self
    }

    /// Merge these fields into every request body. What they mean is the
    /// endpoint's business; the harness passes them through untouched.
    pub fn with_extra_params(mut self, params: Value) -> Self {
        self.extra_params = Some(params);
        self
    }

    /// Replace the preamble, for a host with more to say about its corpus.
    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = preamble.into();
        self
    }

    /// Run one question to its end, reporting each step to `sink` as it
    /// happens.
    ///
    /// The engine lock is taken only inside [`tools::execute`], per call,
    /// never across a model call: a slow model must not serialise every
    /// other reader of the corpus behind it.
    pub async fn ask(
        &self,
        engine: Arc<Mutex<eg_mcp::State>>,
        question: &str,
        sink: &mut (dyn FnMut(Event) + Send),
    ) -> Result<Outcome, PromptError> {
        let mut run = AgentRun::new(question)
            .max_turns(self.policy.max_turns)
            .max_invalid_tool_call_retries(2);
        let mut ledger = Ledger::default();
        let mut calls: Vec<CallRecord> = Vec::new();
        let mut turn = 0;
        let mut ungrounded_retries = 0;

        // The model is told what workbooks the corpus holds before it asks
        // anything. Every tool takes a workbook argument and a model that
        // does not know the names invents one — `"default"` was seen — and
        // then reasons from the refusal as if it were a finding.
        let preamble = match tools::execute(
            Arc::clone(&engine),
            "workbooks".into(),
            json!({}),
            self.redact_values,
        )
        .await
        {
            Ok(Ok(listing)) => format!("{}\n\nThe corpus holds:\n{listing}", self.preamble),
            _ => self.preamble.clone(),
        };

        loop {
            let step = match run.next_step() {
                Ok(step) => step,
                Err(PromptError::MaxTurnsError { .. }) => {
                    return Ok(Outcome {
                        answer: None,
                        turns: turn,
                        calls,
                        usage: run.usage(),
                    });
                }
                Err(e) => return Err(e),
            };
            match step {
                AgentRunStep::CallModel {
                    prompt,
                    history,
                    turn: t,
                } => {
                    turn = t;
                    sink(Event::ModelCall { turn });
                    // The first turn must be a tool call. A small model
                    // asked about a workbook it cannot see will otherwise
                    // answer from the question's own words — and was seen
                    // claiming a `find_value` scan it never made. Only the
                    // first: forcing every turn is the documented footgun
                    // where the model can never stop to answer.
                    let request = self
                        .model
                        .completion_request(prompt)
                        .messages(history)
                        .preamble(preamble.clone())
                        .tools(self.tools.clone())
                        .temperature(0.0)
                        .max_tokens(self.policy.max_output_tokens)
                        .additional_params_opt(self.extra_params.clone());
                    let request = if turn == 1 {
                        request.tool_choice(ToolChoice::Required)
                    } else {
                        request
                    };
                    let response = tokio::time::timeout(self.policy.model_timeout, request.send())
                        .await
                        .map_err(|_| PromptError::PromptCancelled {
                            chat_history: run.full_history(),
                            reason: format!(
                                "the model did not answer within {:?}",
                                self.policy.model_timeout
                            ),
                        })??;
                    let reasoning: String = response
                        .choice
                        .iter()
                        .filter_map(|c| match c {
                            AssistantContent::Reasoning(r) => Some(r),
                            _ => None,
                        })
                        .flat_map(|r| r.content.iter())
                        .filter_map(|part| match part {
                            ReasoningContent::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !reasoning.trim().is_empty() {
                        sink(Event::ModelReasoning {
                            turn,
                            text: reasoning,
                        });
                    }
                    let text: String = response
                        .choice
                        .iter()
                        .filter_map(|c| match c {
                            AssistantContent::Text(t) => Some(t.text()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.trim().is_empty() {
                        sink(Event::ModelText {
                            turn,
                            text: text.clone(),
                        });
                    }
                    let has_tool_calls = response
                        .choice
                        .iter()
                        .any(|c| matches!(c, AssistantContent::ToolCall(_)));
                    let mut outcome = run.model_response(ModelTurn::new(
                        response.message_id.clone(),
                        response.choice.clone(),
                        response.usage,
                        self.names.clone(),
                        self.names.clone(),
                    ))?;
                    // An answer before any tool has run is not an answer:
                    // the model cannot see the workbook, so whatever it
                    // said came from the question's own words — and a small
                    // model was seen describing tool results it never had.
                    // `tool_choice: required` is best-effort on a local
                    // server (a lazy grammar that only binds once the model
                    // starts a call), so the rule is enforced here: the
                    // turn is rolled back with the reason, a bounded number
                    // of times, and the retry spends the same turn budget.
                    //
                    // An *empty* final reply is sent back the same way. A
                    // reasoning model's thinking arrives as reasoning content,
                    // not text, and a model that spent its whole output budget
                    // thinking hands back nothing — seen with a 35B model about
                    // to list three hundred accounts. The retry is a fresh
                    // budget, and the feedback asks for the short form.
                    let empty_reply = !has_tool_calls && text.trim().is_empty();
                    let feedback = if !has_tool_calls && calls.is_empty() {
                        Some(
                            "You have not called any tool, so you know nothing about \
                             this workbook yet; nothing in that reply can be checked. \
                             Call `search` with the question's words first, then \
                             answer from what it returns.",
                        )
                    } else if empty_reply {
                        Some(
                            "Your reply was empty. Answer in a few plain sentences \
                             from what the tools returned; summarise rather than list.",
                        )
                    } else {
                        None
                    };
                    if let Some(feedback) = feedback {
                        if ungrounded_retries < self.policy.max_ungrounded_retries
                            && matches!(outcome, ModelTurnOutcome::Continue { .. })
                        {
                            ungrounded_retries += 1;
                            sink(Event::Ungrounded {
                                turn,
                                reason: feedback.to_string(),
                            });
                            run.retry_model_turn(RetryRequest::Feedback(feedback.to_string()))?;
                            continue;
                        }
                    }
                    // A misnamed tool is a result the model can read, not a
                    // failed run: it gets told what exists and tries again,
                    // up to the retry budget set above.
                    while let ModelTurnOutcome::NeedsResolution(context) = outcome {
                        sink(Event::UnknownTool {
                            turn,
                            name: context.tool_name.clone(),
                        });
                        let mut available: Vec<&str> =
                            self.names.iter().map(String::as_str).collect();
                        available.sort_unstable();
                        outcome =
                            run.resolve_invalid_tool_call(InvalidToolCallAction::skip(format!(
                                "there is no tool called `{}`. The tools are: {}.",
                                context.tool_name,
                                available.join(", ")
                            )))?;
                    }
                }
                AgentRunStep::CallTools { calls: pending } => {
                    let mut results = Vec::with_capacity(pending.len());
                    for call in pending {
                        let id = call.tool_call.id.clone();
                        let provider = call.tool_call.provider.clone();
                        let name = call.tool_call.function.name.clone();
                        if let Some(result) = call.preresolved_result {
                            results.push(result);
                            continue;
                        }
                        let args = call.tool_call.function.arguments.clone();
                        sink(Event::ToolCall {
                            turn,
                            name: name.clone(),
                            args: args.clone(),
                        });
                        let (ok, refused, text) = match self.policy.admit(&mut ledger, &name, &args)
                        {
                            Err(reason) => (false, true, reason),
                            Ok(()) => match tools::execute(
                                Arc::clone(&engine),
                                name.clone(),
                                args.clone(),
                                self.redact_values,
                            )
                            .await
                            {
                                Ok(Ok(text)) => (true, false, text),
                                Ok(Err(message)) => (false, false, message),
                                Err(harness) => (false, false, harness),
                            },
                        };
                        sink(Event::ToolResult {
                            turn,
                            name: name.clone(),
                            ok,
                            refused,
                            text: text.clone(),
                        });
                        calls.push(CallRecord {
                            turn,
                            name: name.clone(),
                            args,
                            ok,
                            refused,
                            result: text.clone(),
                        });
                        results.push(UserContent::tool_result_for(
                            id,
                            provider,
                            name,
                            vec![ToolResultContent::text(text)],
                        ));
                    }
                    run.tool_results(results)?;
                }
                AgentRunStep::Done(response) => {
                    return Ok(Outcome {
                        answer: Some(response.output),
                        turns: turn,
                        calls,
                        usage: response.usage,
                    });
                }
            }
        }
    }
}
