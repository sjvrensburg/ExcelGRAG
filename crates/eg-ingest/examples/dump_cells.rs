//! Dump a sheet's formulas and cached values, in the schema `sheet-oracle`
//! writes.
//!
//! Usage: `dump_cells <workbook> [options]`
//!
//!   --sheet <name|index>   which sheet (default: the first)
//!   --list-sheets          print the sheet names and stop
//!   --range <A1:BZ2000>    only this range
//!   --limit <n>            stop after n cells, default 5000
//!   --all-cells            include cells with no formula
//!   --no-values            kinds only, no cell data
//!   --leading-equals       write formulas as `=A1*2` rather than `A1*2`
//!   --format json|csv      default json
//!   --out <path>           default stdout
//!   --compact              JSON on one line
//!   --no-hash              skip the file digest
//!
//! This is one half of a differential test. The other half is
//! [`sheet-oracle`], which asks SheetJS the same question, and the two dumps
//! are diffed by `sheet-oracle compare`. A spreadsheet reader that decodes a
//! formula wrongly does not crash and does not warn — a wrong formula looks
//! exactly like a right one — so the only cheap way to find such a defect is
//! to ask a second implementation and look at where the answers differ.
//!
//! ```sh
//! cargo run --release -p eg-ingest --example dump_cells -- \
//!   private/book.xlsb --sheet 'Work Doc' --range A2:BZ200 --out ours.json
//! ```
//!
//! Values are the workbook's data: `--no-values` keeps the formulas and the
//! value kinds and drops the rest, which is what to use on a confidential file
//! when the question is about formulas.
//!
//! [`sheet-oracle`]: https://github.com/sjvrensburg/sheet-oracle

use std::io::Read;
use std::path::Path;

use eg_ingest::{load_with, LoadOptions};
use eg_model::{parse_a1, CellValue, RangeRef, Sheet, SheetId, Workbook};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match Options::parse(&args) {
        Ok(Some(options)) => options,
        Ok(None) => return,
        Err(message) => {
            eprintln!("dump_cells: {message}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    if let Err(message) = run(&options) {
        eprintln!("dump_cells: {message}");
        std::process::exit(1);
    }
}

const USAGE: &str = "usage: dump_cells <workbook> [--sheet name|index] [--range A1:B99] \
[--limit n] [--all-cells] [--no-values] [--leading-equals] [--format json|csv] [--out path] \
[--compact] [--no-hash] [--list-sheets]";

struct Options {
    file: String,
    sheet: Option<String>,
    range: Option<String>,
    limit: usize,
    formulas_only: bool,
    values: bool,
    leading_equals: bool,
    format: Format,
    out: Option<String>,
    pretty: bool,
    hash: bool,
    list_sheets: bool,
}

#[derive(PartialEq)]
enum Format {
    Json,
    Csv,
}

impl Options {
    /// `Ok(None)` means the argument list asked for help and nothing is to be
    /// done.
    fn parse(args: &[String]) -> Result<Option<Self>, String> {
        let mut options = Options {
            file: String::new(),
            sheet: None,
            range: None,
            limit: 5000,
            formulas_only: true,
            values: true,
            leading_equals: false,
            format: Format::Json,
            out: None,
            pretty: true,
            hash: true,
            list_sheets: false,
        };
        let mut file = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut wants = |flag: &str| -> Result<String, String> {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} wants a value"))
            };
            match arg.as_str() {
                "-h" | "--help" => {
                    println!("{USAGE}");
                    return Ok(None);
                }
                "--sheet" => options.sheet = Some(wants("--sheet")?),
                "--range" => options.range = Some(wants("--range")?.to_uppercase()),
                "--limit" => {
                    let value = wants("--limit")?;
                    options.limit = value
                        .parse()
                        .map_err(|_| format!("--limit wants a number, got {value:?}"))?;
                }
                "--all-cells" => options.formulas_only = false,
                "--no-values" => options.values = false,
                "--leading-equals" => options.leading_equals = true,
                "--format" => {
                    options.format = match wants("--format")?.as_str() {
                        "json" => Format::Json,
                        "csv" => Format::Csv,
                        other => return Err(format!("--format wants json or csv, got {other:?}")),
                    }
                }
                "--out" => options.out = Some(wants("--out")?),
                "--compact" => options.pretty = false,
                "--no-hash" => options.hash = false,
                "--list-sheets" => options.list_sheets = true,
                other if other.starts_with("--") => return Err(format!("unknown option {other}")),
                other => {
                    if file.is_some() {
                        return Err(format!("unexpected argument {other}"));
                    }
                    file = Some(other.to_string());
                }
            }
        }
        options.file = file.ok_or("no workbook given")?;
        Ok(Some(options))
    }
}

fn run(options: &Options) -> Result<(), String> {
    let loaded = load_with(
        &options.file,
        &LoadOptions {
            max_cells: None,
            ..Default::default()
        },
    )
    .map_err(|e| format!("could not load {}: {e}", options.file))?;
    let workbook = &loaded.workbook;

    if options.list_sheets {
        for sheet in &workbook.sheets {
            println!("{}\t{}", sheet.id.0, sheet.name);
        }
        return Ok(());
    }

    let sheet = resolve_sheet(workbook, options.sheet.as_deref())?;
    let range = match &options.range {
        Some(text) => Some(resolve_range(text, sheet.id)?),
        None => None,
    };

    let (cells, counts, truncated) = extract(sheet, range, options);

    let text = match options.format {
        Format::Json => {
            let payload = payload(options, workbook, sheet, &cells, &counts, truncated)?;
            let mut out = if options.pretty {
                serde_json::to_string_pretty(&payload)
            } else {
                serde_json::to_string(&payload)
            }
            .map_err(|e| format!("could not serialise: {e}"))?;
            out.push('\n');
            out
        }
        Format::Csv => csv(&cells, options.values),
    };

    match &options.out {
        Some(path) => {
            std::fs::write(path, &text).map_err(|e| format!("could not write {path}: {e}"))?;
            eprintln!(
                "{} cells ({} with a formula) → {path}{}",
                cells.len(),
                counts.with_formula,
                if truncated {
                    " — truncated, raise --limit"
                } else {
                    ""
                }
            );
        }
        None => print!("{text}"),
    }
    Ok(())
}

#[derive(Default)]
struct Counts {
    scanned: u64,
    with_formula: u64,
    emitted: u64,
}

/// One cell, as the schema names it.
struct Record {
    reference: String,
    formula: Option<String>,
    kind: &'static str,
    value: Value,
}

fn extract(
    sheet: &Sheet,
    range: Option<RangeRef>,
    options: &Options,
) -> (Vec<Record>, Counts, bool) {
    let mut counts = Counts::default();
    let mut cells = Vec::new();
    let mut truncated = false;

    // Both iterators are row-major, which is what makes two dumps line up.
    let iter: Box<dyn Iterator<Item = _>> = match range {
        Some(range) => Box::new(sheet.iter_range(range)),
        None => Box::new(sheet.iter()),
    };

    for (at, cell) in iter {
        counts.scanned += 1;
        let has_formula = cell.formula.is_some();
        if has_formula {
            counts.with_formula += 1;
        }
        if options.formulas_only && !has_formula {
            continue;
        }
        if cells.len() >= options.limit {
            truncated = true;
            break;
        }
        cells.push(Record {
            reference: at.to_a1(),
            formula: cell.formula.as_ref().map(|f| {
                if options.leading_equals {
                    format!("={f}")
                } else {
                    f.clone()
                }
            }),
            kind: cell.value.kind().as_str(),
            value: if options.values {
                json_value(&cell.value)
            } else {
                Value::Null
            },
        });
        counts.emitted += 1;
    }
    (cells, counts, truncated)
}

/// A cell's value in the schema's terms.
///
/// An error is its text rather than a code, because that is the only spelling
/// two readers can be expected to agree on, and a blank is `null`.
fn json_value(value: &CellValue) -> Value {
    match value {
        CellValue::Empty => Value::Null,
        CellValue::Number(n) => json!(n),
        CellValue::Text(s) => json!(s),
        CellValue::Bool(b) => json!(b),
        CellValue::Error(e) => json!(e.as_str()),
    }
}

fn payload(
    options: &Options,
    workbook: &Workbook,
    sheet: &Sheet,
    cells: &[Record],
    counts: &Counts,
    truncated: bool,
) -> Result<Value, String> {
    let mut rows = Vec::with_capacity(cells.len());
    for cell in cells {
        let mut row = Map::new();
        row.insert("ref".into(), json!(cell.reference));
        if let Some(formula) = &cell.formula {
            row.insert("formula".into(), json!(formula));
        }
        row.insert("kind".into(), json!(cell.kind));
        if options.values {
            row.insert("value".into(), cell.value.clone());
        }
        rows.push(Value::Object(row));
    }

    let digest = if options.hash {
        json!({ "algorithm": "sha256", "value": sha256(Path::new(&options.file))? })
    } else {
        Value::Null
    };

    Ok(json!({
        "tool": "eg-ingest",
        "schema": 1,
        "reader": concat!("eg-ingest ", env!("CARGO_PKG_VERSION")),
        "file": std::fs::canonicalize(&options.file)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| options.file.clone()),
        "digest": digest,
        "sheet": sheet.name,
        "sheetIndex": sheet.id.0,
        "sheetRef": sheet.used_range().map(|r| r.to_a1()),
        "options": {
            "range": options.range,
            "limit": options.limit,
            "formulasOnly": options.formulas_only,
            "leadingEquals": options.leading_equals,
            "values": options.values,
        },
        "counts": {
            "scanned": counts.scanned,
            "withFormula": counts.with_formula,
            "emitted": counts.emitted,
        },
        "truncated": truncated,
        // Reported because a workbook whose values were never recalculated
        // will disagree with any evaluator, and this is where that shows.
        "workbookCells": workbook.total_cells(),
        "cells": rows,
    }))
}

fn csv(cells: &[Record], values: bool) -> String {
    let mut out = String::new();
    out.push_str(if values {
        "ref,formula,kind,value\n"
    } else {
        "ref,formula,kind\n"
    });
    for cell in cells {
        out.push_str(&csv_field(&cell.reference));
        out.push(',');
        out.push_str(&csv_field(cell.formula.as_deref().unwrap_or("")));
        out.push(',');
        out.push_str(&csv_field(cell.kind));
        if values {
            out.push(',');
            out.push_str(&csv_field(&match &cell.value {
                Value::Null => String::new(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            }));
        }
        out.push('\n');
    }
    out
}

/// RFC 4180 quoting: a field is quoted when it holds a comma, a quote or a
/// newline, and an interior quote is doubled. Formulas contain all three.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// The digest the other side of the comparison computes, so a diff can refuse
/// two dumps of different files. `eg-ingest`'s own content hash is blake3 and
/// is a different thing for a different purpose.
fn sha256(path: &Path) -> Result<String, String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn resolve_sheet<'a>(workbook: &'a Workbook, wanted: Option<&str>) -> Result<&'a Sheet, String> {
    let names = || {
        workbook
            .sheets
            .iter()
            .map(|s| format!("{:?}", s.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match wanted {
        None => workbook
            .sheets
            .first()
            .ok_or("the workbook has no sheets".into()),
        Some(text) => {
            if let Ok(index) = text.parse::<usize>() {
                return workbook.sheets.get(index).ok_or_else(|| {
                    format!(
                        "sheet index {index} is past the last sheet ({} sheets)",
                        workbook.sheets.len()
                    )
                });
            }
            workbook
                .sheet_by_name(text)
                .ok_or_else(|| format!("no sheet called {text:?}. This file has: {}", names()))
        }
    }
}

fn resolve_range(text: &str, sheet: SheetId) -> Result<RangeRef, String> {
    let parsed = parse_a1(text).map_err(|e| format!("{text:?} is not an A1 range: {e}"))?;
    if parsed.sheet_name.is_some() {
        return Err(format!(
            "{text:?} names a sheet; --range is sheet-local, use --sheet for the sheet"
        ));
    }
    Ok(parsed.resolve(sheet))
}
