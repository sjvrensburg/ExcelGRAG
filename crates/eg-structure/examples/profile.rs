//! Profile a workbook's columns and report what they hold.
//!
//! This is the measurement behind storing profiles: how many columns a real
//! workbook has, how many of them read as categories a question would name, and
//! what the whole thing costs to build.
//!
//! Usage: `cargo run --release --example profile -- <workbook> [--show-values]
//! [--limit N]`
//!
//! **Values are withheld by default.** A profile is the one thing derived from
//! a workbook that is its data rather than its structure — distinct values,
//! sums, minima — and example output ends up in commit messages and READMEs.
//! Counts, types and A1 ranges are always shown; `--show-values` adds the rest,
//! for a workbook whose contents may be seen.

use std::time::Instant;

use eg_ingest::{load_with, LoadOptions};
use eg_structure::{detect_regions, profile_table, read_table, ColumnProfile, ProfileOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: profile <workbook> [--show-values] [--limit N]");
        std::process::exit(2);
    };
    let rest: Vec<String> = args.collect();
    let show_values = rest.iter().any(|a| a == "--show-values");
    let limit = rest
        .iter()
        .position(|a| a == "--limit")
        .and_then(|i| rest.get(i + 1))
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(15);

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

    let started = Instant::now();
    let opts = ProfileOptions::default();
    let mut columns: Vec<ColumnProfile> = Vec::new();
    let mut tables = 0usize;
    for sheet in &loaded.workbook.sheets {
        for region in detect_regions(sheet) {
            if let Some(table) = read_table(sheet, &region) {
                tables += 1;
                columns.extend(profile_table(sheet, &table, &opts));
            }
        }
    }
    let elapsed = started.elapsed();

    let categorical: Vec<&ColumnProfile> = columns.iter().filter(|c| c.is_categorical()).collect();
    let numeric: Vec<&ColumnProfile> = columns.iter().filter(|c| c.numeric.is_some()).collect();
    let with_errors: Vec<&ColumnProfile> = columns.iter().filter(|c| c.errors > 0).collect();

    println!("workbook:      {}", loaded.workbook.path);
    println!("  sheets:      {}", loaded.workbook.sheets.len());
    println!("  tables:      {tables}");
    println!("  columns:     {}", columns.len());
    println!("  profiled in: {:.2}s", elapsed.as_secs_f64());

    println!("\nkinds");
    for kind in [
        eg_structure::ColumnKind::Number,
        eg_structure::ColumnKind::Text,
        eg_structure::ColumnKind::Bool,
        eg_structure::ColumnKind::Error,
        eg_structure::ColumnKind::Mixed,
        eg_structure::ColumnKind::Empty,
    ] {
        let n = columns.iter().filter(|c| c.kind == kind).count();
        if n > 0 {
            println!("  {:<8} {n}", kind.as_str());
        }
    }

    println!(
        "\ncategorical columns: {} of {} — the ones a question names by value",
        categorical.len(),
        columns.len()
    );
    for column in categorical.iter().take(limit) {
        let values = column.distinct.as_ref().map_or(0, Vec::len);
        println!(
            "  {:<34} {:>10} rows, {values:>3} value(s)   {}",
            truncate(&column.header, 34),
            column.populated,
            column.range.to_a1()
        );
        if show_values {
            if let Some(list) = &column.distinct {
                for value in list.iter().take(6) {
                    println!("      {:>9} x {}", value.count, truncate(&value.value, 48));
                }
                if list.len() > 6 {
                    println!("      … {} more", list.len() - 6);
                }
            }
        }
    }
    if categorical.len() > limit {
        println!("  … {} more", categorical.len() - limit);
    }

    println!("\nnumeric columns: {}", numeric.len());
    if show_values {
        for column in numeric.iter().take(limit) {
            let n = column.numeric.expect("filtered on it");
            println!(
                "  {:<34} sum {:>18.2}  min {:>14.2}  max {:>14.2}",
                truncate(&column.header, 34),
                n.sum,
                n.min,
                n.max
            );
        }
    } else {
        println!("  (sums and ranges withheld; pass --show-values)");
    }

    if !with_errors.is_empty() {
        println!("\ncolumns holding error values: {}", with_errors.len());
        for column in with_errors.iter().take(limit) {
            println!(
                "  {:<34} {:>6} of {:>10}   {}",
                truncate(&column.header, 34),
                column.errors,
                column.populated,
                column.range.to_a1()
            );
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    format!("{}…", s.chars().take(n - 1).collect::<String>())
}
