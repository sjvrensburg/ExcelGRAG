//! The verbs that work on a workbook directly, below the graph: cells,
//! provenance, whether the arithmetic still holds, and what moves if a number
//! changes.
//!
//! All three open the file, which on a large workbook is seconds and gigabytes.
//! That is the price of asking about cells rather than about structure, and it
//! is why the server holds a workbook open while these commands do not.

use std::time::Instant;

use eg_eval::whatif::{what_if, Blocked, Change, WhatIfOptions};
use eg_eval::{cells_in, check as check_formulas, dependents_of, precedents_of, Outcome, Target};
use eg_ingest::{load_with, LoadOptions};
use eg_model::{parse_a1, redact_formula_literals, CellValue, RangeRef, Workbook};

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

/// A formula's text, with its own literals hidden the same way a value is.
///
/// A formula's literals are the workbook's data as much as any cell's value
/// is — `=IF(A2="Smith, John",B2*0.15,0)` names a person and a rate — so
/// printing it unredacted while the value column shows `<number>` would leak
/// exactly what `--redact-values` exists to withhold.
fn show_formula(formula: &str, redact: bool) -> String {
    if redact {
        redact_formula_literals(formula)
    } else {
        formula.to_string()
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
            print!(" ={}", show_formula(formula, redact));
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

/// Sweep a workbook's formulas, printing the report. Returns whether any
/// disagreed — CLAUDE.md calls any disagreement a regression, and a caller
/// (CI, most of all) cannot gate on that from stdout alone, so `main` turns
/// this into a non-zero exit rather than the same 0 a clean sweep gets.
pub fn check(path: &str, scope: Option<&str>, limit: usize, redact: bool) -> Result<bool, String> {
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
        println!(
            "  {:<28} ={}",
            result.a1,
            show_formula(&result.formula, redact)
        );
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
    Ok(report.differed > 0)
}

/// One `Sheet!A1=value` assignment, as a change to a single cell.
///
/// The value is read the way a spreadsheet reads what you type: a number if it
/// parses as one, TRUE or FALSE as a boolean, nothing at all as an empty cell,
/// and anything else as text. Quotes force text, so `A1="12"` is the string.
fn assignment(workbook: &Workbook, text: &str) -> Result<Change, String> {
    let (citation, value) = split_assignment(text)
        .ok_or_else(|| format!("{text:?} is not a change. Write one as \"Sheet1!B2=0.15\"."))?;
    let range = locate(workbook, citation.trim())?;
    if range.cell_count() != 1 {
        return Err(format!(
            "{citation:?} is {} cells. A change names one.",
            range.cell_count()
        ));
    }
    Ok(Change::new(range.top_left(), parse_literal(value.trim())))
}

/// Splits `Sheet!A1=value` on the assignment `=`, scanning past a leading
/// quoted sheet name first — `'It''s a sheet with = in it'!A1=5` must not
/// split on the `=` inside the quotes, the same way `parse_a1` reads it.
/// Falls back to the first `=` in the whole text when there is no leading
/// quote, or the quote never closes (a citation error `locate` will report).
fn split_assignment(text: &str) -> Option<(&str, &str)> {
    let mut after_quote = 0;
    if let Some(rest) = text.strip_prefix('\'') {
        let mut i = 0;
        while let Some(off) = rest[i..].find('\'') {
            i += off + 1;
            if rest[i..].starts_with('\'') {
                i += 1; // a doubled quote is an escaped literal quote
                continue;
            }
            after_quote = 1 + i;
            break;
        }
        // else: unterminated quote — let the plain search below run
    }
    let eq = text[after_quote..].find('=')? + after_quote;
    Some((&text[..eq], &text[eq + 1..]))
}

fn parse_literal(text: &str) -> CellValue {
    if let Some(quoted) = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return CellValue::Text(quoted.to_string());
    }
    if text.is_empty() {
        return CellValue::Empty;
    }
    if let Ok(number) = text.parse::<f64>() {
        return CellValue::Number(number);
    }
    match text.to_ascii_uppercase().as_str() {
        "TRUE" => CellValue::Bool(true),
        "FALSE" => CellValue::Bool(false),
        _ => CellValue::Text(text.to_string()),
    }
}

pub fn whatif(
    path: &str,
    changes: &[String],
    levels: usize,
    max_cells: usize,
    limit: usize,
    redact: bool,
) -> Result<(), String> {
    let workbook = open(path)?;
    let changes: Vec<Change> = changes
        .iter()
        .map(|text| assignment(&workbook, text))
        .collect::<Result<_, _>>()?;

    let at = Instant::now();
    let impact = what_if(
        &workbook,
        &changes,
        &WhatIfOptions {
            max_levels: levels,
            max_cells,
            limit,
        },
    );
    let elapsed = at.elapsed().as_secs_f64();
    let report = &impact.report;

    for applied in &impact.changes {
        println!(
            "{:<28} {} → {}",
            applied.a1,
            show(&applied.before, redact),
            show(&applied.after, redact)
        );
        if let Some(formula) = &applied.replaced_formula {
            println!("  replacing ={}", show_formula(formula, redact));
        }
    }

    println!(
        "\n{} cell(s) downstream over {} level(s), {} scan(s) of {} formulas in {elapsed:.1}s",
        report.affected, report.levels, report.scans, report.formulas_scanned
    );
    println!("  moved       {:>10}", report.moved);
    println!("  unchanged   {:>10}", report.unchanged);
    println!("  blocked     {:>10}", report.blocked);
    if let Some(stopped) = report.stopped {
        println!(
            "  stopped at {} — the change reaches further than this",
            stopped.as_str()
        );
    }

    if !impact.moved.is_empty() {
        println!("\nmoved ({} of {})", impact.moved.len(), report.moved);
        for moved in &impact.moved {
            println!(
                "  {:<28} ={}",
                moved.a1,
                show_formula(&moved.formula, redact)
            );
            println!(
                "    {} → {}   (level {}){}",
                show(&moved.before, redact),
                show(&moved.after, redact),
                moved.level,
                if moved.was_stale {
                    "  — and it already disagreed with its stored value"
                } else {
                    ""
                }
            );
        }
        if report.moved_not_listed > 0 {
            println!("  … {} more, raise --limit", report.moved_not_listed);
        }
    }

    if !impact.unanswered.is_empty() {
        // These are the ones a caller must not read as "unchanged": the change
        // reaches them and this cannot say where it leaves them.
        println!(
            "\nno answer ({} of {})",
            impact.unanswered.len(),
            report.blocked
        );
        for blocked in &impact.unanswered {
            let why = match &blocked.reason {
                Blocked::Formula(reason) => reason.to_string(),
                Blocked::Upstream(cause) => format!("reads {cause}, which has no answer"),
                Blocked::Cycle => "circular reference".to_string(),
            };
            println!("  {:<28} {why}", blocked.a1);
        }
    }

    if report.affected == 0 {
        println!("\nnothing in this workbook reads it.");
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
    fn a_change_is_read_the_way_a_sheet_reads_what_you_type() {
        let wb = workbook();
        let change = assignment(&wb, "'Q3 Sales'!A1=0.15").expect("a change");
        assert_eq!(change.cell.sheet, SheetId(0));
        assert_eq!(change.value, CellValue::Number(0.15));

        assert_eq!(parse_literal("42"), CellValue::Number(42.0));
        assert_eq!(parse_literal("TRUE"), CellValue::Bool(true));
        assert_eq!(parse_literal(""), CellValue::Empty);
        assert_eq!(parse_literal("overdue"), CellValue::Text("overdue".into()));
        // Quotes are how you say the digits are a label, not a number.
        assert_eq!(parse_literal("\"12\""), CellValue::Text("12".into()));
    }

    #[test]
    fn a_quoted_sheet_name_holding_an_equals_sign_is_not_split_on() {
        // L7: a naive `split_once('=')` breaks the moment the sheet name
        // itself contains one, even though `parse_a1` reads a quoted sheet
        // name fine.
        let mut wb = workbook();
        wb.sheets.push(Sheet::new(SheetId(2), "Cost = Price"));
        let change = assignment(&wb, "'Cost = Price'!A1=5").expect("a change");
        assert_eq!(change.cell.sheet, SheetId(2));
        assert_eq!(change.value, CellValue::Number(5.0));

        // And a doubled quote inside the name (Excel's escape for a literal
        // quote) must not be mistaken for the closing quote.
        let mut wb = workbook();
        wb.sheets.push(Sheet::new(SheetId(2), "It's = Fine"));
        let change = assignment(&wb, "'It''s = Fine'!B2=7").expect("a change");
        assert_eq!(change.cell.sheet, SheetId(2));
        assert_eq!(change.value, CellValue::Number(7.0));
    }

    #[test]
    fn a_change_names_one_cell_and_says_so_when_it_does_not() {
        let wb = workbook();
        let error = assignment(&wb, "'Q3 Sales'!A1:B9=1").expect_err("a range");
        assert!(error.contains("names one"), "{error}");
        let error = assignment(&wb, "'Q3 Sales'!A1").expect_err("no value");
        assert!(error.contains("not a change"), "{error}");
    }

    #[test]
    fn redaction_keeps_the_shape_and_drops_the_data() {
        let value = CellValue::Number(295.869);
        assert_eq!(show(&value, false), "295.869");
        assert_eq!(show(&value, true), "<number>");
        assert_eq!(show(&CellValue::Text("debt".into()), false), "\"debt\"");
        assert_eq!(show(&CellValue::Text("debt".into()), true), "<text>");
    }

    #[test]
    fn formula_redaction_drops_literals_the_way_read_cells_and_check_print_them() {
        // The exact site `cells`/`check`/`whatif` all call before splicing a
        // formula into their output: a redacted formula must carry no
        // literal a redacted value wouldn't have shown either.
        let formula = "IF(A2=\"Smith, John\",B2*0.15,0)";
        assert_eq!(show_formula(formula, false), formula);
        assert_eq!(
            show_formula(formula, true),
            "IF(A2=<text>,B2*<number>,<number>)"
        );
        // References survive redaction — only literals are the workbook's
        // data; a range is where to look, not what is there.
        assert_eq!(
            show_formula("VLOOKUP(A1,Rates!A1:B9,2,FALSE)", true),
            "VLOOKUP(A1,Rates!A1:B9,<number>,FALSE)"
        );
    }
}
