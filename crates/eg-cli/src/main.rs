//! `eg` — the front door.
//!
//! Everything this workspace can do is reachable through the examples in each
//! crate, which is fine for developing a library and poor for using one. This
//! is the same capabilities behind one binary and nine verbs, in the order a
//! question actually travels:
//!
//! ```sh
//! eg index corpus/ book.xlsb     # read the workbook, store its graph, index it
//! eg ask corpus/ bad debt        # a question, answered as a cited passage
//! eg cells book.xlsb 'LOOKUP!AE53:AG89'   # the cells behind a citation
//! eg check book.xlsb             # do the formulas still agree with their values
//! eg what-if book.xlsb 'RATES!B4=0.15'    # and what moves if one changes
//! eg serve corpus/               # the same, to an agent over MCP
//! ```
//!
//! The verbs wrap library calls only. The diagnostics — `raw_cells`,
//! `why_unpopulated`, the format probes — stay as examples in the crate they
//! belong to, because they exist to develop this code rather than to use it.
//!
//! Cell values are shown. That is the opposite of the examples, which redact by
//! default because their output ends up in commit messages and READMEs; a
//! person who types `eg cells` is asking to see the cells. `--redact-values`
//! turns them back into kinds, and matches `eg serve`, so the two front doors
//! agree with each other.

mod corpus;
mod workbook;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "eg",
    version,
    about = "Ask a spreadsheet a question, and check the answer against its cells.",
    long_about = None,
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read workbooks into a corpus and index them for search.
    Index {
        /// The corpus directory. Created if it does not exist.
        dir: String,
        /// Workbooks to add. With none, the existing corpus is re-indexed.
        workbooks: Vec<String>,
        /// Re-index workbooks the corpus already holds.
        #[arg(long)]
        reindex: bool,
        /// Skip the embedding model; index by word only.
        #[arg(long)]
        lexical_only: bool,
        /// Do not profile the columns.
        ///
        /// A profile records what a column holds — its distinct values where
        /// there are few of them, and the range and total where it is numeric.
        /// That is the workbook's data, unlike everything else the corpus
        /// stores, and it is written to its own `profiles/` directory so it can
        /// be withheld. `--redact-values` keeps the counts and types and drops
        /// what came out of the cells.
        #[arg(long)]
        no_profiles: bool,
        #[command(flatten)]
        privacy: Privacy,
    },

    /// Ask a question and get the context around the answer, as a passage.
    Ask {
        dir: String,
        #[arg(required = true)]
        query: Vec<String>,
        /// How many search hits to expand from.
        #[arg(long, default_value_t = 5)]
        seeds: usize,
        /// Dependency hops from a seed.
        #[arg(long, default_value_t = 2)]
        hops: usize,
        /// Most nodes per workbook.
        #[arg(long, default_value_t = 40)]
        budget: usize,
        /// Contained children to show per node.
        #[arg(long, default_value_t = 0)]
        children: usize,
        /// Ceiling on the passage, in characters.
        #[arg(long, default_value_t = 8000)]
        chars: usize,
        /// Restrict to one workbook, by content hash (or a prefix of it).
        #[arg(long)]
        workbook: Option<String>,
        /// Restrict to one sheet, by exact name.
        #[arg(long)]
        sheet: Option<String>,
        #[arg(long)]
        lexical_only: bool,
    },

    /// Search the corpus and list what matched, without expanding it.
    Search {
        dir: String,
        #[arg(required = true)]
        query: Vec<String>,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Restrict to one workbook, by content hash (or a prefix of it).
        #[arg(long)]
        workbook: Option<String>,
        /// Restrict to one sheet, by exact name.
        #[arg(long)]
        sheet: Option<String>,
        #[arg(long)]
        lexical_only: bool,
    },

    /// List the workbooks in a corpus.
    Workbooks { dir: String },

    /// Read the cells of a range: their formulas and their values.
    Cells {
        workbook: String,
        /// A citation, sheet and all, e.g. "'Q3 Sales'!B2:D40".
        citation: String,
        #[arg(long, default_value_t = 40)]
        limit: usize,
        #[command(flatten)]
        privacy: Privacy,
    },

    /// Follow a citation to the cells behind it.
    Trace {
        workbook: String,
        citation: String,
        /// Find what reads the range, rather than what it reads. Expensive:
        /// nothing records who reads a cell, so every formula is scanned.
        #[arg(long)]
        dependents: bool,
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },

    /// Recompute formulas and say whether they agree with their stored values.
    ///
    /// Exits 2 if any formula disagreed, so CI can gate on it without parsing
    /// stdout; exits 1 on a tool error (e.g. the workbook could not be read).
    Check {
        workbook: String,
        /// Confine the sweep to one range, e.g. "'Q3 Sales'!A1:Z999".
        #[arg(long)]
        scope: Option<String>,
        /// How many disagreements to list.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[command(flatten)]
        privacy: Privacy,
    },

    /// Change a cell and report every cell that moves because of it.
    #[command(name = "what-if", alias = "whatif")]
    WhatIf {
        workbook: String,
        /// One or more `Sheet!A1=value` changes. A bare word is text, a
        /// number is a number, and nothing at all empties the cell.
        #[arg(required = true)]
        changes: Vec<String>,
        /// Levels of the dependency chain to follow. Each one is a full scan
        /// of the workbook's formulas.
        #[arg(long, default_value_t = 8)]
        levels: usize,
        /// Ceiling on how many cells the change may reach before the walk
        /// gives up. The counts stay exact for what it did walk.
        #[arg(long, default_value_t = 500_000)]
        max_cells: usize,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[command(flatten)]
        privacy: Privacy,
    },

    /// Serve a corpus to an agent over MCP, on stdin and stdout.
    Serve {
        dir: String,
        #[command(flatten)]
        privacy: Privacy,
    },
}

#[derive(Args, Clone, Copy)]
struct Privacy {
    /// Print the kind of each value rather than the value: `<number>`,
    /// `<text>`. For a workbook whose contents should not leave the machine.
    #[arg(long)]
    redact_values: bool,
}

fn main() {
    let cli = Cli::parse();
    // Set only by `Check`, when the sweep found a disagreement — CLAUDE.md
    // calls that a regression, and a caller (CI, most of all) cannot gate on
    // it from stdout alone.
    let mut disagreements = false;
    let result = match cli.command {
        Command::Index {
            dir,
            workbooks,
            reindex,
            lexical_only,
            no_profiles,
            privacy,
        } => corpus::index(
            &dir,
            &workbooks,
            reindex,
            lexical_only,
            !no_profiles,
            privacy.redact_values,
        ),
        Command::Ask {
            dir,
            query,
            seeds,
            hops,
            budget,
            children,
            chars,
            workbook,
            sheet,
            lexical_only,
        } => corpus::ask(
            &dir,
            &query.join(" "),
            corpus::AskOptions {
                seeds,
                hops,
                budget,
                children,
                chars,
                workbook,
                sheet,
                lexical_only,
            },
        ),
        Command::Search {
            dir,
            query,
            limit,
            workbook,
            sheet,
            lexical_only,
        } => corpus::search(&dir, &query.join(" "), limit, workbook, sheet, lexical_only),
        Command::Workbooks { dir } => corpus::workbooks(&dir),
        Command::Cells {
            workbook,
            citation,
            limit,
            privacy,
        } => workbook::cells(&workbook, &citation, limit, privacy.redact_values),
        Command::Trace {
            workbook,
            citation,
            dependents,
            limit,
        } => workbook::trace(&workbook, &citation, dependents, limit),
        Command::Check {
            workbook,
            scope,
            limit,
            privacy,
        } => workbook::check(&workbook, scope.as_deref(), limit, privacy.redact_values)
            .map(|found_disagreements| disagreements = found_disagreements),
        Command::WhatIf {
            workbook,
            changes,
            levels,
            max_cells,
            limit,
            privacy,
        } => workbook::whatif(
            &workbook,
            &changes,
            levels,
            max_cells,
            limit,
            privacy.redact_values,
        ),
        Command::Serve { dir, privacy } => corpus::serve(&dir, privacy.redact_values),
    };

    if let Err(message) = result {
        eprintln!("eg: {message}");
        std::process::exit(1);
    }
    if disagreements {
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_argument_definitions_are_coherent() {
        // clap's own audit: duplicate flags, defaults that do not parse,
        // required arguments after optional ones. It panics on a mistake, which
        // is the whole point of running it here rather than at a user's prompt.
        Cli::command().debug_assert();
    }

    #[test]
    fn every_verb_is_reachable() {
        let names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for verb in [
            "index",
            "ask",
            "search",
            "workbooks",
            "cells",
            "trace",
            "check",
            "what-if",
            "serve",
        ] {
            assert!(names.contains(&verb.to_string()), "{verb} went missing");
        }
    }

    #[test]
    fn a_question_is_taken_as_words_rather_than_as_one_argument() {
        // `eg ask corpus bad debt provision` — the query is the rest of the
        // line, so it needs no quoting.
        let cli = Cli::try_parse_from(["eg", "ask", "corpus", "bad", "debt", "provision"])
            .expect("parses");
        match cli.command {
            Command::Ask { dir, query, .. } => {
                assert_eq!(dir, "corpus");
                assert_eq!(query.join(" "), "bad debt provision");
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn a_question_with_nothing_to_ask_is_refused() {
        assert!(Cli::try_parse_from(["eg", "ask", "corpus"]).is_err());
        assert!(Cli::try_parse_from(["eg", "cells", "book.xlsb"]).is_err());
    }

    #[test]
    fn values_are_shown_unless_the_caller_asks_otherwise() {
        let shown = Cli::try_parse_from(["eg", "cells", "b.xlsb", "S!A1"]).expect("parses");
        let redacted = Cli::try_parse_from(["eg", "cells", "b.xlsb", "S!A1", "--redact-values"])
            .expect("parses");
        match (shown.command, redacted.command) {
            (Command::Cells { privacy: a, .. }, Command::Cells { privacy: b, .. }) => {
                assert!(
                    !a.redact_values,
                    "a person who types `eg cells` wants the cells"
                );
                assert!(b.redact_values);
            }
            _ => panic!("wrong command"),
        }
    }
}
