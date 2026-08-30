//! Recompute a formula from the cells under it, and say whether the workbook
//! agrees.
//!
//! Usage: `recompute <workbook> [<A1>] [options]`
//!
//!   --check          sweep every formula instead of printing one cell
//!   --limit <n>      cap the list, default 20
//!   --show-values    print the numbers rather than their kinds
//!
//! ```sh
//! cargo run --release -p eg-eval --example recompute -- private/book.xlsb 'Sheet1!D7'
//! cargo run --release -p eg-eval --example recompute -- private/book.xlsb --check
//! ```
//!
//! Without `--show-values` this prints verdicts and value *kinds*, never the
//! values: a verdict is about the workbook, a value is the workbook's data, and
//! this is pointed at `private/` by design. Which cell disagrees is enough to
//! know where to look.
//!
//! A sweep is one pass over every formula, each recomputed from its precedents'
//! *stored* values — no chain is followed, so a disagreement is about that one
//! formula.

use std::time::Instant;

use eg_eval::{check, recompute, Outcome, Recomputed};
use eg_ingest::{load_with, LoadOptions};
use eg_model::{parse_a1, CellValue, RangeRef, Workbook};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: recompute <workbook> [<A1>] [--check] [--limit n] [--show-values]");
        std::process::exit(2);
    };

    let mut target: Option<String> = None;
    let mut sweep = false;
    let mut limit = 20usize;
    let mut show_values = false;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => sweep = true,
            "--show-values" => show_values = true,
            "--limit" => match args.next().as_deref().and_then(|n| n.parse().ok()) {
                Some(n) => limit = n,
                None => {
                    eprintln!("--limit wants a number");
                    std::process::exit(2);
                }
            },
            other if other.starts_with("--") => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
            other => target = Some(other.to_string()),
        }
    }
    if target.is_none() && !sweep {
        eprintln!("give an A1 to recompute, or --check to sweep the workbook");
        std::process::exit(2);
    }

    let loading = Instant::now();
    let loaded = match load_with(
        &path,
        &LoadOptions {
            max_cells: None,
            ..Default::default()
        },
    ) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not load {path}: {e}");
            std::process::exit(1);
        }
    };
    let load_time = loading.elapsed();
    let workbook = &loaded.workbook;

    let scope = target.as_deref().map(|a1| resolve(workbook, a1));

    println!("{path}");
    println!(
        "  loaded {} sheets, {} cells in {:.2}s",
        workbook.sheets.len(),
        workbook.total_cells(),
        load_time.as_secs_f64()
    );

    let at = Instant::now();
    if sweep {
        let (differed, report) = check(workbook, scope, limit);
        let elapsed = at.elapsed().as_secs_f64();
        let share = |n: u64| {
            if report.formulas == 0 {
                0.0
            } else {
                100.0 * n as f64 / report.formulas as f64
            }
        };
        println!(
            "\n  {} formulas in {:.2}s ({:.0} per second)",
            report.formulas,
            elapsed,
            report.formulas as f64 / elapsed.max(f64::EPSILON)
        );
        println!(
            "    agreed      {:>10} ({:.1}%)",
            report.agreed,
            share(report.agreed)
        );
        println!(
            "    differed    {:>10} ({:.1}%)",
            report.differed,
            share(report.differed)
        );
        println!(
            "    unsupported {:>10} ({:.1}%)",
            report.unsupported,
            share(report.unsupported)
        );

        if !report.reasons.is_empty() {
            println!("\n  not recomputed, commonest first");
            for (reason, count) in report.reasons.iter().take(limit) {
                println!("    {count:>10}  {reason}");
            }
        }

        println!(
            "\n  disagreements ({} of {})",
            differed.len(),
            report.differed
        );
        for result in &differed {
            println!("    {:<28} ={}", result.a1, result.formula);
            println!("      {}", verdict(result, show_values));
        }
        if differed.is_empty() {
            println!("    none");
        }
        return;
    }

    // One cell, or every formula cell in a range.
    let scope = scope.expect("checked above");
    let Some(sheet) = workbook.sheet(scope.sheet) else {
        eprintln!("no such sheet");
        std::process::exit(1);
    };
    let mut printed = 0usize;
    for (cell, _) in sheet.iter_range(scope) {
        let Some(result) = recompute(workbook, cell) else {
            continue;
        };
        if printed >= limit {
            println!("    … more, raise --limit");
            break;
        }
        printed += 1;
        println!("\n  {}", result.a1);
        println!("    ={}", result.formula);
        println!("    {}", verdict(&result, show_values));
        for input in &result.inputs {
            let detail = match (&input.value, show_values) {
                (Some(value), true) => format!("{value:?}"),
                (Some(value), false) => format!("<{}>", value.kind().as_str()),
                (None, _) => format!("{} cells", input.cells),
            };
            println!("      {:<28} {:<12} {detail}", input.a1, input.text);
        }
    }
    if printed == 0 {
        println!(
            "\n  nothing to recompute — no formulas in {}",
            workbook.cite_range(scope)
        );
    }
    println!("\n  ({:.2}ms)", at.elapsed().as_secs_f64() * 1000.0);
}

fn verdict(result: &Recomputed, show_values: bool) -> String {
    match &result.outcome {
        Outcome::Agrees(value) => format!("agrees {}", render(value, show_values)),
        Outcome::Differs { computed, stored } => format!(
            "differs: computed {}, stored {}{}",
            render(computed, show_values),
            render(stored, show_values),
            if show_values {
                ""
            } else {
                " — --show-values to see them"
            }
        ),
        Outcome::Unsupported(reason) => format!("not recomputed: {reason}"),
    }
}

fn render(value: &CellValue, show_values: bool) -> String {
    if show_values {
        value.to_display()
    } else {
        format!("<{}>", value.kind().as_str())
    }
}

fn resolve(workbook: &Workbook, a1: &str) -> RangeRef {
    let parsed = match parse_a1(a1) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{a1:?} is not an A1 reference: {e}");
            std::process::exit(2);
        }
    };
    let Some(name) = &parsed.sheet_name else {
        eprintln!("{a1:?} names no sheet — a citation needs one, e.g. 'Sheet1!B2'");
        std::process::exit(2);
    };
    match workbook.sheet_id_by_name(name) {
        Some(id) => parsed.resolve(id),
        None => {
            eprintln!("{} has no sheet called {name:?}", workbook.path);
            std::process::exit(1);
        }
    }
}
