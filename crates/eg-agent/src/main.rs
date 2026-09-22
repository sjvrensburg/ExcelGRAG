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
use serde::{Deserialize, Serialize};

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
    /// With `--score`, save each question's mark to this file, for a later
    /// run to `--compare` against.
    #[arg(long)]
    out: Option<PathBuf>,
    /// With `--score`, diff this run's marks against a file an earlier
    /// `--score --out` saved — e.g. before and after a change to the
    /// tool-validation gate, to see which questions it moved.
    #[arg(long)]
    compare: Option<PathBuf>,
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

/// One question's outcome, saved by `--out` and read back by `--compare`.
///
/// Deliberately smaller than [`eg_agent::harness::CallRecord`]'s own trail:
/// this is a score to diff against a later run, not a replay log, and a
/// tool's arguments or full result text are not what "did this question's
/// mark change" needs.
#[derive(Clone, Serialize, Deserialize)]
struct ScoreRow {
    ask: String,
    mark: String,
    turns: usize,
    calls: usize,
    /// Whether the run's own trail shows the `invalid_ref` auto-correction
    /// firing — a rejected structural/schema-gate call immediately followed
    /// by the `tables` call it triggers. What this exists to measure: how
    /// much of a before/after difference the gate (and its auto-correction)
    /// accounts for, versus everything else that also changed between runs.
    auto_corrected: bool,
}

/// Whether `calls` shows the `invalid_ref` correction firing: a call the
/// structural/schema gate rejected, immediately followed by the `tables`
/// call `eg_agent::invalid_ref::correction` runs in response. Matches
/// `harness.rs`'s own ordering — the correction is pushed to `calls` right
/// after the call it corrects, never elsewhere.
fn was_auto_corrected(calls: &[eg_agent::harness::CallRecord]) -> bool {
    calls.windows(2).any(|pair| {
        let [rejected, correction] = pair else {
            return false;
        };
        eg_agent::invalid_ref::gate(rejected.ok, rejected.refused, &rejected.result).is_some()
            && correction.ok
    })
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
    let model = eg_agent::openai_compatible(&args.base_url, Some(&api_key), &args.model)
        .map_err(|e| anyhow!(e))?;
    let policy = Policy {
        max_turns: args.max_turns,
        max_scans: args.max_scans,
        ..Policy::default()
    };
    let mut harness = Harness::new(model, policy).with_redacted_values(args.redact_values);
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
        let mut rows: Vec<ScoreRow> = Vec::new();
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
                    rows.push(ScoreRow {
                        ask: q.ask.clone(),
                        mark: format!("MISS (failed: {e})"),
                        turns: 0,
                        calls: 0,
                        auto_corrected: false,
                    });
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
            rows.push(ScoreRow {
                ask: q.ask.clone(),
                mark,
                turns: outcome.turns,
                calls: outcome.calls.len(),
                auto_corrected: was_auto_corrected(&outcome.calls),
            });
        }
        println!("\n{hits}/{} answered and grounded", questions.len());
        for row in &rows {
            let note = if row.auto_corrected {
                " (auto-corrected)"
            } else {
                ""
            };
            println!(
                "  {:<12} {:>2} turns {:>2} calls  {}{note}",
                row.mark, row.turns, row.calls, row.ask
            );
        }
        if let Some(path) = &args.out {
            let json = serde_json::to_string_pretty(&rows)?;
            std::fs::write(path, json)
                .with_context(|| format!("could not write {}", path.display()))?;
            println!("\nsaved to {}", path.display());
        }
        if let Some(path) = &args.compare {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let baseline: Vec<ScoreRow> = serde_json::from_str(&text)
                .with_context(|| format!("{} is not a --score --out file", path.display()))?;
            print_comparison(&baseline, &rows);
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
        Event::ModelReasoning { text, .. } => {
            println!("  thinking: {}", first_lines(text, 2, verbose))
        }
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

/// A short mark, stripped of its parenthetical detail — `"HIT  (0.85)"` and
/// `"HIT  (85%)"` are the same outcome for a before/after diff even though
/// the answer file's wants let a question be answered either way, and a
/// `MISS`'s known-gap note or run-failure reason would otherwise make two
/// identical misses look like a change.
fn mark_kind(mark: &str) -> &str {
    mark.split_whitespace().next().unwrap_or(mark)
}

/// Prints a before/after diff of two `--score` runs over the same question
/// file: which questions moved, and how many of the misses that flipped to
/// hits did so with the `invalid_ref` auto-correction visible in the trail
/// — the number that shows what the gate specifically closed, as opposed to
/// whatever else differed between the two runs (a different model, a
/// different corpus).
fn print_comparison(baseline: &[ScoreRow], current: &[ScoreRow]) {
    use std::collections::HashMap;
    let before: HashMap<&str, &ScoreRow> = baseline.iter().map(|r| (r.ask.as_str(), r)).collect();

    println!("\n=== compare ===");
    let mut moved = 0;
    let mut gate_closed = 0;
    for row in current {
        let Some(prior) = before.get(row.ask.as_str()) else {
            println!("  (new question, no baseline) {}", row.ask);
            continue;
        };
        if mark_kind(&prior.mark) == mark_kind(&row.mark) {
            continue;
        }
        moved += 1;
        let flipped_to_hit = mark_kind(&prior.mark) != "HIT" && mark_kind(&row.mark) == "HIT";
        if flipped_to_hit && row.auto_corrected {
            gate_closed += 1;
        }
        let note = if flipped_to_hit && row.auto_corrected {
            "  (closed by the auto-correction)"
        } else {
            ""
        };
        println!(
            "  {} -> {}{note}  {}",
            mark_kind(&prior.mark),
            mark_kind(&row.mark),
            row.ask
        );
    }
    if moved == 0 {
        println!("  no question's mark changed");
    } else {
        println!(
            "\n{moved} question(s) moved, {gate_closed} of them a miss that the auto-correction \
             turned into a hit"
        );
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
