//! Follow a citation down to the cells behind it.
//!
//! Usage: `trace <workbook> <A1> [options]`
//!
//!   --dependents     find what reads the range, not what it reads
//!   --limit <n>      cap the list, default 20
//!   --show-values    print cell values
//!
//! The A1 is a citation of the kind the retrieval layer hands back, sheet name
//! and all: `trace book.xlsb "'BP136-6-WORK DOC'!AQ2"`.
//!
//! ```sh
//! cargo run --release -p eg-eval --example trace -- private/book.xlsb 'Sheet1!B2'
//! cargo run --release -p eg-eval --example trace -- private/book.xlsb 'Rates!B2:B99' --dependents
//! ```
//!
//! Prints addresses and formulas. **Not values**, unless `--show-values` says
//! so: a formula is structure, a value is the workbook's data, and this is
//! pointed at `private/` by design.
//!
//! The two directions do not cost the same, and the timing line says which you
//! paid for. What a cell reads is in its own text. What reads a cell is written
//! down nowhere, so finding it means scanning every formula in the file.

use std::time::Instant;

use eg_eval::{cells_in, dependents_of, precedents_of};
use eg_ingest::{load_with, LoadOptions};
use eg_model::{parse_a1, RangeRef};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(a1)) = (args.next(), args.next()) else {
        eprintln!("usage: trace <workbook> <A1> [--dependents] [--limit n] [--show-values]");
        std::process::exit(2);
    };

    let mut dependents = false;
    let mut limit = 20usize;
    let mut show_values = false;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dependents" => dependents = true,
            "--show-values" => show_values = true,
            "--limit" => match args.next().as_deref().and_then(|n| n.parse().ok()) {
                Some(n) => limit = n,
                None => {
                    eprintln!("--limit wants a number");
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
    }

    let parsed = match parse_a1(&a1) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{a1:?} is not an A1 reference: {e}");
            std::process::exit(2);
        }
    };

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

    // A citation names its sheet. Without one there is no way to know which of
    // twenty-five sheets was meant, and guessing the first would be wrong
    // quietly.
    let Some(name) = &parsed.sheet_name else {
        eprintln!("{a1:?} names no sheet — a citation needs one, e.g. 'Sheet1!B2'");
        std::process::exit(2);
    };
    let Some(sheet) = workbook
        .sheets
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
    else {
        eprintln!("{path} has no sheet called {name:?}");
        std::process::exit(1);
    };
    let range: RangeRef = parsed.resolve(sheet.id);

    println!("{path}");
    println!(
        "  loaded {} sheets, {} cells in {:.2}s",
        workbook.sheets.len(),
        workbook.total_cells(),
        load_time.as_secs_f64()
    );
    println!("  {}", workbook.cite_range(range));

    let (cells, capped) = cells_in(workbook, range, limit);
    println!(
        "\n  cells ({}{})",
        cells.len(),
        if capped { "+" } else { "" }
    );
    for fact in &cells {
        let detail = match (&fact.formula, show_values) {
            (Some(f), _) => format!("={f}"),
            (None, true) => format!("{:?}", fact.value),
            (None, false) => format!("<{}>", fact.kind.as_str()),
        };
        println!("    {:<28} {detail}", fact.a1);
    }
    if cells.is_empty() {
        println!("    none populated");
    }

    if dependents {
        let at = Instant::now();
        let (refs, report) = dependents_of(workbook, range, limit);
        println!(
            "\n  read by ({} of {}, {} formula(s) scanned in {:.2}s)",
            refs.len(),
            report.matches,
            report.formulas_scanned,
            at.elapsed().as_secs_f64()
        );
        for reference in &refs {
            println!(
                "    {:<28} {}",
                workbook.cite(reference.from),
                reference.text
            );
        }
        if refs.is_empty() {
            println!("    nothing in this workbook reads it");
        }
        return;
    }

    // Forward: what each formula cell in the range reads. Cheap, because it is
    // in the text of those cells.
    let at = Instant::now();
    let mut printed = 0usize;
    println!("\n  reads");
    for fact in &cells {
        for reference in precedents_of(workbook, fact.cell) {
            if printed >= limit {
                println!("    … more, raise --limit");
                break;
            }
            let target = reference.target.cite(workbook);
            println!("    {:<28} {} → {target}", fact.a1, reference.text);
            printed += 1;
        }
    }
    if printed == 0 {
        println!("    nothing — no formulas in this range");
    }
    println!("\n  ({:.2}ms)", at.elapsed().as_secs_f64() * 1000.0);
}
