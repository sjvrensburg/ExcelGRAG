//! What calamine hands us for one sheet, before ingest touches it.
//!
//! Usage: `raw_cells <workbook> <sheet> [row] [--show-values]`
//!
//! The question this answers is "did the reader lose this cell, or did we?".
//! Ingest builds a sheet from two independent ranges — values and formulas —
//! so a cell can be missing because the file does not have it, because the
//! reader skipped the record, or because we dropped it afterwards. This shows
//! the first of the three, which is the one no other example can see.
//!
//! It found `BrtFmlaError`: a formula cell whose cached value is an error was
//! skipped by the reader, so the coordinate appeared in the formula range and
//! nowhere in the value range. From ingest's side that is indistinguishable
//! from a blank.
//!
//! ```sh
//! cargo run --release -p eg-ingest --example raw_cells -- private/book.xlsb Sheet1
//! cargo run --release -p eg-ingest --example raw_cells -- private/book.xlsb Sheet1 144
//! ```
//!
//! Prints the range's extent, and per row the populated columns and their value
//! *kinds*. Values only with `--show-values`, as everywhere else in this repo.

use calamine::{open_workbook_auto, Data, Reader};
use eg_model::col_to_letters;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(sheet)) = (args.next(), args.next()) else {
        eprintln!("usage: raw_cells <workbook> <sheet> [row] [--show-values]");
        std::process::exit(2);
    };
    let mut row: Option<usize> = None;
    let mut show_values = false;
    for arg in args {
        match arg.as_str() {
            "--show-values" => show_values = true,
            other => match other.parse::<usize>() {
                // A row is given in A1 terms, so subtract the one Excel adds.
                Ok(n) if n > 0 => row = Some(n - 1),
                _ => {
                    eprintln!("expected a 1-based row number or --show-values, got {other:?}");
                    std::process::exit(2);
                }
            },
        }
    }

    let mut workbook = match open_workbook_auto(&path) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("could not open {path}: {e}");
            std::process::exit(1);
        }
    };
    let values = match workbook.worksheet_range(&sheet) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no sheet {sheet:?}: {e}");
            std::process::exit(1);
        }
    };
    let (row0, col0) = values.start().unwrap_or((0, 0));
    println!(
        "{sheet}: {} x {}, origin {}{}",
        values.height(),
        values.width(),
        col_to_letters(col0),
        row0 + 1
    );

    let mut shown = 0usize;
    let mut counted = 0usize;
    for (r, c, value) in values.used_cells() {
        let (r, c) = (r + row0 as usize, c + col0 as usize);
        if row.is_some_and(|want| want != r) {
            continue;
        }
        counted += 1;
        if row.is_none() || shown >= 200 {
            continue;
        }
        shown += 1;
        let detail = if show_values {
            format!("{value:?}")
        } else {
            kind_of(value).to_string()
        };
        println!("  {}{:<8} {detail}", col_to_letters(c as u32), r + 1);
    }

    match row {
        Some(want) => println!("row {} holds {counted} populated cells", want + 1),
        None => println!("{counted} populated cells"),
    }
}

/// The kind of a value, for naming a cell without disclosing it.
fn kind_of(value: &Data) -> &'static str {
    match value {
        Data::Empty => "empty",
        Data::String(_) => "text",
        Data::Float(_) | Data::Int(_) => "number",
        Data::Bool(_) => "bool",
        Data::Error(_) => "error",
        Data::DateTime(_) | Data::DateTimeIso(_) | Data::DurationIso(_) => "date",
    }
}
