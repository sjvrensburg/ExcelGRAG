//! Audit a workbook and report *statistics only* — never cell contents.
//!
//! Intended for confidential files that cannot be committed as fixtures. It
//! prints counts, coverage percentages and A1 addresses, so its output is safe
//! to paste into an issue or a chat. Sheet names are redacted to indices by
//! default; pass `--show-names` to include them.
//!
//! Usage:
//!     cargo run --release -p eg-ingest --example audit -- private/book.xlsb

use std::collections::BTreeMap;
use std::time::Instant;

use eg_ingest::{load_with, LoadOptions};
use eg_model::{Sheet, ValueKind};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let mut show_names = false;
    for arg in args.by_ref() {
        match arg.as_str() {
            "--show-names" => show_names = true,
            other => path = Some(other.to_string()),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: audit [--show-names] <workbook>");
        std::process::exit(2);
    };

    let opts = LoadOptions {
        max_cells: None,
        ..Default::default()
    };

    let started = Instant::now();
    let loaded = match load_with(&path, &opts) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to load: {e}");
            std::process::exit(1);
        }
    };
    let elapsed = started.elapsed();
    let wb = &loaded.workbook;

    println!("# Workbook audit");
    println!();
    println!("- format: {:?}", wb.format);
    println!("- load time: {:.2}s", elapsed.as_secs_f64());
    println!("- sheets: {}", wb.sheets.len());
    println!("- populated cells: {}", wb.total_cells());
    println!("- defined names: {}", wb.defined_names.len());
    println!("- writable: {}", wb.is_writable());
    if !loaded.capabilities.limitations().is_empty() {
        println!("- format limitations:");
        for note in loaded.capabilities.limitations() {
            println!("    - {note}");
        }
    }
    if !loaded.warnings.is_empty() {
        println!("- warnings: {}", loaded.warnings.len());
        for w in loaded.warnings.iter().take(10) {
            println!("    - {w}");
        }
    }

    let mut total_formulas = 0usize;
    let mut total_gaps = 0usize;
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();

    println!();
    println!("## Sheets");
    println!();
    for (i, sheet) in wb.sheets.iter().enumerate() {
        let label = if show_names {
            format!("{:?}", sheet.name)
        } else {
            format!("#{i}")
        };
        let formulas = sheet.iter().filter(|(_, c)| c.is_formula()).count();
        total_formulas += formulas;

        for (_, cell) in sheet.iter() {
            *kinds
                .entry(match cell.value.kind() {
                    ValueKind::Empty => "empty",
                    ValueKind::Number => "number",
                    ValueKind::Text => "text",
                    ValueKind::Bool => "bool",
                    ValueKind::Error => "error",
                })
                .or_default() += 1;
        }

        let gaps = shared_formula_gaps(sheet);
        total_gaps += gaps.len();

        println!(
            "- {label}: {} cells, {formulas} formulas, used={}, merges={}, tables={}, visible={}",
            sheet.len(),
            sheet
                .used_range()
                .map(|r| r.to_a1())
                .unwrap_or_else(|| "-".into()),
            sheet.merges.len(),
            sheet.tables.len(),
            sheet.visibility.is_visible(),
        );
        if !gaps.is_empty() {
            println!(
                "    - suspected dropped shared formulas: {} (e.g. {})",
                gaps.len(),
                gaps.iter()
                    .take(5)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    println!();
    println!("## Totals");
    println!();
    println!("- formulas: {total_formulas}");
    println!("- value kinds: {kinds:?}");
    println!("- suspected dropped shared formulas: {total_gaps}");
    if total_gaps > 0 {
        let denom = total_formulas + total_gaps;
        println!(
            "- estimated formula coverage: {:.1}%",
            100.0 * total_formulas as f64 / denom.max(1) as f64
        );
        println!();
        println!(
            "  A gap is a numeric cell with no formula that sits directly below a\n  \
             formula cell in the same column, with a numeric cell above it too.\n  \
             In the binary formats calamine discards `PtgExp` tokens, which is how\n  \
             Excel encodes shared formulas, so those cells lose their formula while\n  \
             keeping their cached value. This estimate is a heuristic, not a proof."
        );
    }
}

/// Find cells that look like they lost a shared formula.
///
/// In XLSB and XLS, Excel commonly stores a run of identical column formulas as
/// one master plus `PtgExp` back-references. calamine ignores `PtgExp`, so those
/// cells arrive with a cached value but no formula text. The signature is a
/// column where a formula cell is followed by numeric cells that have none.
fn shared_formula_gaps(sheet: &Sheet) -> Vec<String> {
    let Some(used) = sheet.used_range() else {
        return Vec::new();
    };

    let mut gaps = Vec::new();
    for col in used.left..=used.right {
        let mut seen_formula_above = false;
        for row in used.top..=used.bottom {
            match sheet.get(row, col) {
                Some(cell) if cell.is_formula() => seen_formula_above = true,
                Some(cell) if seen_formula_above && cell.value.kind() == ValueKind::Number => {
                    gaps.push(eg_model::CellRef::new(sheet.id, row, col).to_a1());
                }
                // A blank or text cell ends the run; the next formula starts a new one.
                _ => seen_formula_above = false,
            }
        }
    }
    gaps
}
