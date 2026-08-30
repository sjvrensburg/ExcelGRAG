//! The verbs that work on a workbook directly, below the graph: cells,
//! provenance, and whether the arithmetic still holds.
//!
//! All three open the file, which on a large workbook is seconds and gigabytes.
//! That is the price of asking about cells rather than about structure, and it
//! is why the server holds a workbook open while these commands do not.

use std::time::Instant;

use eg_eval::{cells_in, check as check_formulas, dependents_of, precedents_of, Outcome, Target};
use eg_ingest::{load_with, LoadOptions};
use eg_model::{parse_a1, CellValue, RangeRef, Workbook};

/// Open a workbook, saying how long it took, because on a real file the wait
/// wants explaining.
fn open(path: &str) -> Result<Workbook, String> {
    let started = Instant::now();
    let loaded = load_with(
        path,
        &LoadOptions {
            max_cells: None,
            ..Default::default()
        },
    )
    .map_err(|e| format!("could not load {path}: {e}"))?;
    eprintln!(
        "{path}: {} sheets, {} cells in {:.1}s",
        loaded.workbook.sheets.len(),
        loaded.workbook.total_cells(),
        started.elapsed().as_secs_f64()
    );
    Ok(loaded.workbook)
}

/// Turn a citation into a range on one of the workbook's sheets.
fn locate(workbook: &Workbook, citation: &str) -> Result<RangeRef, String> {
    let parsed = parse_a1(citation).map_err(|e| format!("{citation:?} is not an A1 range: {e}"))?;
    let Some(name) = &parsed.sheet_name else {
        return Err(format!(
            "{citation:?} names no sheet. A citation needs one, e.g. \"Sheet1!B2\"."
        ));
    };
    let id = workbook.sheet_id_by_name(name).ok_or_else(|| {
        let names: Vec<&str> = workbook.sheets.iter().map(|s| s.name.as_str()).collect();
        format!(
            "no sheet called {name:?}. This workbook has: {}",
            names.join(", ")
        )
    })?;
    Ok(parsed.resolve(id))
}

/// A value, or the shape of one when the caller asked not to see it.
fn show(value: &CellValue, redact: bool) -> String {
    if redact {
        format!("<{}>", value.kind().as_str())
    } else {
        match value {
            CellValue::Text(text) => format!("{text:?}"),
            other => other.to_display(),
        }
    }
}

pub fn cells(path: &str, citation: &str, limit: usize, redact: bool) -> Result<(), String> {
    let workbook = open(path)?;
    let range = locate(&workbook, citation)?;
    let (cells, capped) = cells_in(&workbook, range, limit);

    println!(
        "{} — {} populated cell(s){}",
        workbook.cite_range(range),
        cells.len(),
        if capped { ", capped by --limit" } else { "" }
    );
    for fact in &cells {
        print!("  {:<28}", fact.a1);
        if let Some(formula) = &fact.formula {
            print!(" ={formula}");
        }
        println!("  {}", show(&fact.value, redact));
    }
    if cells.is_empty() {
        println!("  nothing populated in that range");
    }
    Ok(())
}

pub fn trace(path: &str, citation: &str, dependents: bool, limit: usize) -> Result<(), String> {
    let workbook = open(path)?;
    let range = locate(&workbook, citation)?;
    let at = Instant::now();

    if dependents {
        let (refs, report) = dependents_of(&workbook, range, limit);
        println!(
            "{} is read by {} formula(s), of {} scanned in {:.1}s",
            workbook.cite_range(range),
            report.matches,
            report.formulas_scanned,
            at.elapsed().as_secs_f64()
        );
        for reference in &refs {
            println!("  {:<28} {}", workbook.cite(reference.from), reference.text);
        }
        if refs.is_empty() {
            println!("  nothing in this workbook reads it");
        } else if report.capped {
            println!("  … more, raise --limit");
        }
        return Ok(());
    }

    println!("{} reads", workbook.cite_range(range));
    let mut printed = 0;
    for (cell, contents) in workbook
        .sheet(range.sheet)
        .into_iter()
        .flat_map(|sheet| sheet.iter_range(range))
    {
        if contents.formula.is_none() {
            continue;
        }
        for reference in precedents_of(&workbook, cell) {
            if printed >= limit {
                println!("  … more, raise --limit");
                return Ok(());
            }
            let target = match &reference.target {
                Target::Cells(range) => workbook.cite_range(*range),
                Target::UnknownSheet(name) => format!("#REF! — no sheet called {name:?}"),
                Target::ExternalWorkbook(token) => {
                    format!("another workbook, written as [{token}]")
                }
            };
            println!(
                "  {:<28} {} → {target}",
                workbook.cite(cell),
                reference.text
            );
            printed += 1;
        }
    }
    if printed == 0 {
        println!("  nothing — no formulas in that range");
    }
    Ok(())
}

pub fn check(path: &str, scope: Option<&str>, limit: usize, redact: bool) -> Result<(), String> {
    let workbook = open(path)?;
    let range = match scope {
        Some(citation) => Some(locate(&workbook, citation)?),
        None => None,
    };

    let at = Instant::now();
    let (differed, report) = check_formulas(&workbook, range, limit);
    let elapsed = at.elapsed().as_secs_f64();
    let share = |n: u64| {
        if report.formulas == 0 {
            0.0
        } else {
            100.0 * n as f64 / report.formulas as f64
        }
    };

    println!("{} formulas in {elapsed:.1}s", report.formulas);
    println!(
        "  agreed      {:>10} ({:.1}%)",
        report.agreed,
        share(report.agreed)
    );
    println!(
        "  differed    {:>10} ({:.1}%)",
        report.differed,
        share(report.differed)
    );
    println!(
        "  unsupported {:>10} ({:.1}%)",
        report.unsupported,
        share(report.unsupported)
    );

    if !report.reasons.is_empty() {
        println!("\nnot recomputed, commonest first");
        for (reason, count) in report.reasons.iter().take(limit.max(1)) {
            println!("  {count:>10}  {reason}");
        }
    }

    println!(
        "\ndisagreements ({} of {})",
        differed.len(),
        report.differed
    );
    for result in &differed {
        println!("  {:<28} ={}", result.a1, result.formula);
        match &result.outcome {
            Outcome::Differs { computed, stored } => println!(
                "    computed {}, stored {}",
                show(computed, redact),
                show(stored, redact)
            ),
            other => println!("    {}", other.as_str()),
        }
    }
    if differed.is_empty() {
        println!("  none — every formula this can evaluate agrees with its stored value");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_model::{Cell, Sheet, SheetId};

    fn workbook() -> Workbook {
        let mut sheet = Sheet::new(SheetId(0), "Q3 Sales");
        sheet.set(0, 0, Cell::literal(CellValue::Number(1.0)));
        Workbook {
            sheets: vec![sheet, Sheet::new(SheetId(1), "Rates")],
            ..Default::default()
        }
    }

    #[test]
    fn a_citation_resolves_against_the_sheet_it_names() {
        let workbook = workbook();
        let range = locate(&workbook, "'Q3 Sales'!B2:D40").expect("a range");
        assert_eq!(range.sheet, SheetId(0));
        assert_eq!(range.to_a1(), "B2:D40");
        // Excel matches sheet names without regard to case, and so does this.
        assert_eq!(
            locate(&workbook, "rates!A1").expect("a range").sheet,
            SheetId(1)
        );
    }

    #[test]
    fn a_citation_without_a_sheet_is_refused_rather_than_guessed() {
        // Guessing the first sheet would answer a different question from the
        // one asked, and look right doing it.
        let error = locate(&workbook(), "B2:D40").expect_err("no sheet named");
        assert!(error.contains("names no sheet"), "{error}");
    }

    #[test]
    fn a_wrong_sheet_name_says_what_the_workbook_does_have() {
        let error = locate(&workbook(), "Missing!A1").expect_err("no such sheet");
        assert!(
            error.contains("Q3 Sales") && error.contains("Rates"),
            "{error}"
        );
    }

    #[test]
    fn redaction_keeps_the_shape_and_drops_the_data() {
        let value = CellValue::Number(295.869);
        assert_eq!(show(&value, false), "295.869");
        assert_eq!(show(&value, true), "<number>");
        assert_eq!(show(&CellValue::Text("debt".into()), false), "\"debt\"");
        assert_eq!(show(&CellValue::Text("debt".into()), true), "<text>");
    }
}
