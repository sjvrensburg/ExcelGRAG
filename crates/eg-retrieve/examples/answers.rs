//! Score a corpus against questions whose right answer is already known.
//!
//! Every other layer of this project has a check that fails loudly: the reader
//! is diffed against a second reader, the graph's edges are re-derived from the
//! cells, and every formula is recomputed against what Excel cached. The layer
//! the whole thing exists for — a question in words, and the part of a workbook
//! that answers it — had none. This is that check.
//!
//! Usage: `answers <corpus> <questions.json> [--seeds N] [--budget N]
//! [--lexical-only] [--verbose]`
//!
//! The question file is a list of `{ask, want, why}`. `want` names the nodes a
//! good answer must reach, by citation (`'Work Doc'!AR2:AR115004`) or by label
//! (`PV of expected receipts`); a question is satisfied when *any* of them is
//! found, because several nodes are often equally right.
//!
//! ```json
//! [
//!   {
//!     "ask": "what rate are receipts discounted at",
//!     "want": ["RATES!BS4:BS12"],
//!     "why": "the rate table the PV column looks into"
//!   }
//! ]
//! ```
//!
//! Two numbers, because they answer different questions:
//!
//! - **Search** — where the wanted node lands in the ranking. Reported as hit@1,
//!   hit@5, and mean reciprocal rank, which is the one to watch: it moves when a
//!   right answer slips from second to third, and hit@5 does not.
//! - **Context** — whether the passage `eg ask` renders actually cites the
//!   wanted node. This is the one that matters, because the passage is the
//!   product; a node ranked first and then squeezed out of the passage by the
//!   budget has not answered anything.
//!
//! Prints the questions it was given and the labels of what it found, so a
//! question file about a confidential workbook makes this output confidential
//! too. The committed one is not.

use std::collections::BTreeMap;
use std::time::Instant;

use eg_graph::store::Corpus;
use eg_index::{Hit, SearchOptions};
use eg_retrieve::{expand, find, render, ExpandOptions, RenderOptions};
use serde::Deserialize;

/// One question, and what a good answer has to reach.
#[derive(Debug, Deserialize)]
struct Question {
    ask: String,
    want: Vec<String>,
    /// Why that is the right answer. Not scored — it is here so that a question
    /// nobody can justify does not quietly become the standard.
    #[serde(default)]
    #[allow(dead_code)]
    why: String,
}

/// How one question came out.
struct Scored {
    ask: String,
    /// 1-based position of the first wanted node in the ranking.
    rank: Option<usize>,
    /// Whether the rendered passage cites a wanted node.
    in_context: bool,
    /// What came back first, so a miss can be read rather than guessed at.
    top: Option<String>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(dir), Some(file)) = (args.next(), args.next()) else {
        eprintln!("usage: answers <corpus> <questions.json> [--seeds N] [--budget N] [--lexical-only] [--verbose]");
        std::process::exit(2);
    };
    let rest: Vec<String> = args.collect();
    let flag = |name: &str, default: usize| -> usize {
        rest.iter()
            .position(|a| a == name)
            .and_then(|i| rest.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    // The defaults `eg ask` uses. Scoring anything else measures a tool nobody
    // runs.
    let seeds = flag("--seeds", 8);
    let budget = flag("--budget", 40);
    let chars = flag("--chars", 4000);
    let lexical_only = rest.iter().any(|a| a == "--lexical-only");
    let verbose = rest.iter().any(|a| a == "--verbose");

    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("could not read {file}: {e}");
            std::process::exit(1);
        }
    };
    let questions: Vec<Question> = match serde_json::from_str(&text) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("{file} is not a list of questions: {e}");
            std::process::exit(1);
        }
    };
    let corpus = match Corpus::open(&dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not open the corpus: {e}");
            std::process::exit(1);
        }
    };

    let started = Instant::now();
    let mut scored = Vec::new();
    for question in &questions {
        scored.push(score(
            &dir,
            &corpus,
            question,
            seeds,
            budget,
            chars,
            lexical_only,
        ));
    }
    let elapsed = started.elapsed();

    println!("corpus at {dir}: {} workbook(s)", corpus.len());
    println!(
        "  {} question(s), {} seeds, budget {budget}, {} search{}",
        questions.len(),
        seeds,
        if lexical_only { "by word" } else { "hybrid" },
        if lexical_only {
            ""
        } else {
            " (word + meaning)"
        }
    );

    let n = scored.len().max(1) as f64;
    let hits_at = |k: usize| -> f64 {
        scored
            .iter()
            .filter(|s| s.rank.is_some_and(|r| r <= k))
            .count() as f64
            / n
    };
    let mrr: f64 = scored
        .iter()
        .map(|s| s.rank.map_or(0.0, |r| 1.0 / r as f64))
        .sum::<f64>()
        / n;
    let in_context = scored.iter().filter(|s| s.in_context).count();

    println!("\nsearch");
    println!("  hit@1                 {:>6.1}%", hits_at(1) * 100.0);
    println!("  hit@5                 {:>6.1}%", hits_at(5) * 100.0);
    println!("  hit@{seeds:<17} {:>6.1}%", hits_at(seeds) * 100.0);
    println!("  mean reciprocal rank  {mrr:>6.3}");

    println!("\ncontext");
    println!(
        "  passage cites it      {:>6.1}%   ({in_context} of {})",
        in_context as f64 / n * 100.0,
        scored.len()
    );
    println!(
        "\n{:.2}s for {} question(s)",
        elapsed.as_secs_f64(),
        n as u64
    );

    // A question that is answered needs no explaining; the ones that are not
    // are the entire point of running this.
    let missed: Vec<&Scored> = scored.iter().filter(|s| !s.in_context).collect();
    if !missed.is_empty() {
        println!("\nnot in the passage:");
        for s in &missed {
            println!(
                "  {:?}\n    ranked {}, first hit was {}",
                s.ask,
                match s.rank {
                    Some(r) => format!("#{r}"),
                    None => format!("nowhere in {seeds}"),
                },
                s.top.as_deref().unwrap_or("nothing")
            );
        }
    }
    if verbose {
        println!("\nevery question:");
        for s in &scored {
            println!(
                "  {:<4} {:<8} {:?}",
                if s.in_context { "ok" } else { "MISS" },
                match s.rank {
                    Some(r) => format!("#{r}"),
                    None => "—".to_string(),
                },
                s.ask
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn score(
    dir: &str,
    corpus: &Corpus,
    question: &Question,
    seeds: usize,
    budget: usize,
    chars: usize,
    lexical_only: bool,
) -> Scored {
    let options = SearchOptions {
        limit: seeds.max(1),
        ..Default::default()
    };
    let hits = find(dir, &question.ask, &options, lexical_only).unwrap_or_default();
    let rank = hits
        .iter()
        .position(|h| wanted(h, &question.want))
        .map(|i| i + 1);
    let top = hits.first().map(describe);

    let in_context = match expand(
        corpus,
        &hits,
        &ExpandOptions {
            budget: budget.max(1),
            ..Default::default()
        },
    ) {
        Ok(found) => {
            let rendered = render(
                &found,
                &RenderOptions {
                    max_chars: chars.max(200),
                    ..Default::default()
                },
            );
            // Against the citations rather than the text: a passage that happens
            // to contain the words of a label has not cited the node, and an
            // agent may only cite what the citation list holds.
            let labels: BTreeMap<&str, ()> = found
                .workbooks
                .iter()
                .flat_map(|w| w.nodes.iter())
                .map(|n| (n.label.as_str(), ()))
                .collect();
            question.want.iter().any(|want| {
                rendered.citations.iter().any(|c| matches(c, want))
                    || labels.keys().any(|l| matches(l, want))
            })
        }
        Err(_) => false,
    };

    Scored {
        ask: question.ask.clone(),
        rank,
        in_context,
        top,
    }
}

/// Whether a hit is one of the nodes the question wanted.
fn wanted(hit: &Hit, want: &[String]) -> bool {
    want.iter()
        .any(|w| hit.a1.as_deref().is_some_and(|a1| matches(a1, w)) || matches(&hit.label, w))
}

/// Case-insensitive equality, so a question file can spell a sheet the way the
/// tab does rather than the way the index stored it.
fn matches(found: &str, want: &str) -> bool {
    found.eq_ignore_ascii_case(want)
}

fn describe(hit: &Hit) -> String {
    match &hit.a1 {
        Some(a1) => format!("{} {:?} at {a1}", hit.kind.as_str(), hit.label),
        None => format!("{} {:?}", hit.kind.as_str(), hit.label),
    }
}
