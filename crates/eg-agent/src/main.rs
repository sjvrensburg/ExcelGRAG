//! `eg-agent`: drive the harness from a shell.
//!
//! One question, or a whole answer file scored the way `eg-retrieve`'s
//! scorer does — except that here the mark is whether the *agent's final
//! reply* names an answer, after it chose its own tools, not whether
//! retrieval ranked the node. Every step is printed as it lands, because a
//! wrong answer is diagnosed from the trail, not from the reply.
//!
//! The model is any OpenAI-chat-completions-compatible endpoint, which is
//! what a local `llama-server`, Ollama, and every hosted API speak. The key,
//! when one is needed, is named by environment variable — never carried on
//! the command line, the same rule `eg gui` keeps.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use eg_agent::{Event, Harness, Policy};
use rig_core::client::CompletionClient;
use rig_core::providers::openai::CompletionsClient;
use serde::Deserialize;

#[derive(Parser)]
#[command(
    name = "eg-agent",
    about = "Let a model explore an ExcelGRAG corpus with the eg-mcp tools, one step at a time"
)]
struct Args {
    /// The corpus directory (`eg index` writes it).
    corpus: PathBuf,
    /// The question. Omit it with `--score` to run an answer file instead.
    question: Vec<String>,
    /// An OpenAI-chat-completions-compatible base URL.
    #[arg(long, default_value = "http://127.0.0.1:8080/v1")]
    base_url: String,
    /// The model name the endpoint expects. A local llama-server ignores it.
    #[arg(long, default_value = "local")]
    model: String,
    /// The environment variable holding the API key, if the endpoint wants
    /// one.
    #[arg(long)]
    api_key_env: Option<String>,
    /// Score every question in this file (the `answers.json` format).
    #[arg(long)]
    score: Option<PathBuf>,
    /// With `--score`, only the first N questions.
    #[arg(long)]
    limit: Option<usize>,
    /// Most model calls per question.
    #[arg(long, default_value_t = Policy::default().max_turns)]
    max_turns: usize,
    /// Most full-scan tool calls (`dependents`, `find_value`) per question.
    #[arg(long, default_value_t = Policy::default().max_scans)]
    max_scans: usize,
    /// Show the model every value as its kind, never the value itself.
    #[arg(long)]
    redact_values: bool,
    /// Print tool results in full rather than their first lines.
    #[arg(long)]
    verbose: bool,
    /// A JSON object merged into every request body, for endpoint-specific
    /// switches — e.g. '{"chat_template_kwargs":{"terse":false}}' to a
    /// llama-server whose template takes that flag.
    #[arg(long)]
    extra_params: Option<String>,
}

#[derive(Deserialize)]
struct Question {
    ask: String,
    want: Vec<String>,
    #[serde(default)]
    known_gap: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let corpus = args
        .corpus
        .to_str()
        .ok_or_else(|| anyhow!("the corpus path is not UTF-8"))?;
    if !args.corpus.join("manifest.json").is_file() {
        return Err(anyhow!(
            "{corpus} is not a corpus: no manifest.json. Run `eg index {corpus} <workbook>` first."
        ));
    }
    let state = eg_mcp::State::open(corpus, args.redact_values).map_err(|e| anyhow!(e))?;
    let engine = Arc::new(Mutex::new(state));

    let api_key = match &args.api_key_env {
        Some(var) => std::env::var(var)
            .with_context(|| format!("the environment variable {var} is not set"))?,
        // A local server wants a bearer header or none; any string satisfies
        // llama-server, and the value is never a secret.
        None => "none".to_string(),
    };
    let client = CompletionsClient::builder()
        .api_key(api_key)
        .base_url(&args.base_url)
        .build()
        .map_err(|e| anyhow!("could not build the model client: {e}"))?;
    let model = client.completion_model(&args.model);
    let policy = Policy {
        max_turns: args.max_turns,
        max_scans: args.max_scans,
        ..Policy::default()
    };
    let mut harness = Harness::new(model, policy);
    if let Some(raw) = &args.extra_params {
        let params: serde_json::Value =
            serde_json::from_str(raw).context("--extra-params is not JSON")?;
        if !params.is_object() {
            return Err(anyhow!("--extra-params must be a JSON object"));
        }
        harness = harness.with_extra_params(params);
    }
    let verbose = args.verbose;

    if let Some(path) = &args.score {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let mut questions: Vec<Question> = serde_json::from_str(&text)?;
        if let Some(n) = args.limit {
            questions.truncate(n);
        }
        let mut hits = 0;
        let mut rows = Vec::new();
        for (i, q) in questions.iter().enumerate() {
            println!("\n=== [{}/{}] {}", i + 1, questions.len(), q.ask);
            let mut sink = |e: Event| print_event(&e, verbose);
            // A question the model could not finish — a timed-out call, a
            // provider error — is that question's miss, not the end of the
            // file: the other questions are the point of running it.
            let outcome = match harness.ask(Arc::clone(&engine), &q.ask, &mut sink).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    println!("--- MISS (run failed: {e})");
                    rows.push((q.ask.clone(), format!("MISS (failed: {e})"), 0, 0));
                    continue;
                }
            };
            let answer = outcome.answer.clone().unwrap_or_default();
            // A hit is *grounded*: the reply names an answer, and a tool the
            // model actually called returned that name. The reply alone is
            // not enough — the question usually contains the answer's own
            // words, and a model that never called a tool echoes them back.
            // Case-blind, and blind to the thousands separators a model puts
            // into a figure a tool printed bare — `41,789,046.97` is
            // `41789046.97`, and the agent answer file wants the number.
            let named = |text: &str, w: &str| normalise(text).contains(&normalise(w));
            // The wants are one answer in its several spellings — `0.85`,
            // `85%` — so the reply may use one and the tool another.
            let carried = q.want.iter().any(|w| {
                outcome
                    .calls
                    .iter()
                    .any(|c| c.ok && !c.refused && named(&c.result, w))
            });
            let hit = q.want.iter().find(|w| carried && named(&answer, w));
            let echoed = hit.is_none() && q.want.iter().any(|w| named(&answer, w));
            let mark = match (hit, echoed, &q.known_gap) {
                (Some(w), _, _) => {
                    hits += 1;
                    format!("HIT  ({w})")
                }
                (None, true, _) => "ECHO (named, but no tool result carried it)".to_string(),
                (None, false, Some(gap)) => format!("MISS (known gap: {gap})"),
                (None, false, None) => "MISS".to_string(),
            };
            println!(
                "--- {mark}; {} turn(s), {} tool call(s), {} tokens",
                outcome.turns,
                outcome.calls.len(),
                outcome.usage.total_tokens
            );
            println!("{}", indent(&answer));
            rows.push((q.ask.clone(), mark, outcome.turns, outcome.calls.len()));
        }
        println!("\n{hits}/{} answered and grounded", questions.len());
        for (ask, mark, turns, calls) in rows {
            println!("  {mark:<12} {turns:>2} turns {calls:>2} calls  {ask}");
        }
        return Ok(());
    }

    let question = args.question.join(" ");
    if question.trim().is_empty() {
        return Err(anyhow!("give a question, or --score <answers.json>"));
    }
    let mut sink = |e: Event| print_event(&e, verbose);
    let outcome = harness.ask(engine, &question, &mut sink).await?;
    match outcome.answer {
        Some(answer) => println!("\n{answer}"),
        None => println!(
            "\n(no answer: the turn budget of {} ran out after {} tool call(s))",
            args.max_turns,
            outcome.calls.len()
        ),
    }
    println!(
        "\n{} turn(s), {} tool call(s), {} tokens",
        outcome.turns,
        outcome.calls.len(),
        outcome.usage.total_tokens
    );
    Ok(())
}

fn print_event(event: &Event, verbose: bool) {
    match event {
        Event::ModelCall { turn } => println!("→ model call #{turn}"),
        Event::ModelText { text, .. } => println!("  model: {}", first_lines(text, 3, verbose)),
        Event::ToolCall { name, args, .. } => println!("  → {name} {args}"),
        Event::ToolResult {
            name,
            ok,
            refused,
            text,
            ..
        } => {
            let verdict = match (refused, ok) {
                (true, _) => "refused by policy",
                (false, true) => "ok",
                (false, false) => "tool said no",
            };
            println!("  ← {name}: {verdict}");
            println!("{}", indent(&first_lines(text, 12, verbose)));
        }
        Event::UnknownTool { name, .. } => println!("  ✗ no such tool: {name}"),
        Event::Ungrounded { reason, .. } => {
            println!("  ✗ sent back: {}", first_lines(reason, 1, false))
        }
    }
}

fn normalise(text: &str) -> String {
    text.to_lowercase().replace(',', "")
}

fn first_lines(text: &str, n: usize, all: bool) -> String {
    if all {
        return text.to_string();
    }
    let total = text.lines().count();
    let mut out: Vec<&str> = text.lines().take(n).collect();
    let more = total.saturating_sub(n);
    let tail;
    if more > 0 {
        tail = format!("… ({more} more line(s))");
        out.push(&tail);
    }
    out.join("\n")
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("      {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
