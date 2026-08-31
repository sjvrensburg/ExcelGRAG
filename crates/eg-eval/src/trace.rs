//! Which cell fed which.
//!
//! The graph answers "what does this table depend on" by lifting every
//! reference to the region containing it, and that lift is lossy on purpose:
//! 6.79 million formulas on the reference workbook, most naming two or three
//! cells each, is a graph larger than the workbook. What it cannot answer is
//! "why is *this* number wrong", which needs the cell.
//!
//! So the cell layer is not stored. It is recovered on demand from the
//! workbook, which is what the node ranges in the graph are for. The trade is
//! deliberate and it has a price — the workbook has to be read — and the two
//! directions do not cost the same:
//!
//! - **Precedents**, the cells a formula reads, are in that formula's own text.
//!   Answering needs the one cell.
//! - **Dependents**, the cells that read a given cell, are not written down
//!   anywhere. Answering needs every formula in the workbook scanned, because
//!   any of them might name it.
//!
//! That asymmetry is why they are separate functions with separate costs rather
//! than one `trace` that hides which of the two you asked for.
//!
//! Nothing here evaluates anything. Recomputing a formula is the rest of P6;
//! knowing which cells it stands on comes first, because an evaluator that
//! cannot say where a number came from is a second opinion rather than an
//! explanation.

use eg_model::formula::scan_references_into;
use eg_model::{CellRef, CellValue, RangeRef, ReferenceSpan, Sheet, SheetId, ValueKind, Workbook};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// One cell, as a fact about the workbook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellFact {
    pub cell: CellRef,
    /// A fully-qualified citation, so a caller can hand it back unchanged.
    pub a1: String,
    pub kind: ValueKind,
    /// The formula as written, for a cell that has one.
    pub formula: Option<String>,
    /// The cell's value.
    ///
    /// This is the workbook's actual data, and the only thing in this crate
    /// that is. Callers rendering for anyone but the workbook's owner should
    /// treat it accordingly — the examples in this repo redact it by default.
    pub value: CellValue,
}

/// Where a reference points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// Cells of this workbook.
    Cells(RangeRef),
    /// The same cells on every sheet a 3-D reference spans (`Jan:Dec!B2`), in
    /// tab order. A separate variant rather than a `Cells` per sheet because
    /// one reference was written, and the callers that count references — the
    /// what-if's levels, the dependents report — count it once.
    Spanned(Vec<RangeRef>),
    /// A sheet the workbook does not have — a `#REF!` break. For a 3-D
    /// reference, either end going missing breaks the whole span.
    UnknownSheet(String),
    /// Another workbook, named by the token the formula wrote. Not resolved,
    /// because no reader available to us maps that token to a path.
    ExternalWorkbook(String),
}

impl Target {
    /// The ranges named, in tab order; empty for a target this workbook does
    /// not contain.
    ///
    /// The way to read a target, and the reason it is a slice rather than an
    /// `Option<RangeRef>`: a caller that matched `Cells` alone handled the
    /// ordinary reference and silently dropped the 3-D one, which is how a
    /// what-if came to report the sheets of a `Jan:Dec!B2` as unaffected.
    pub fn ranges(&self) -> &[RangeRef] {
        match self {
            Target::Cells(range) => std::slice::from_ref(range),
            Target::Spanned(ranges) => ranges,
            Target::UnknownSheet(_) | Target::ExternalWorkbook(_) => &[],
        }
    }

    /// This target, written for a reader.
    ///
    /// One rendering, because the CLI, the MCP server and the examples each
    /// had their own identical copy — three places to remember when a variant
    /// is added, which is two more than anyone remembers.
    pub fn cite(&self, workbook: &Workbook) -> String {
        match self {
            Target::Cells(range) => workbook.cite_range(*range),
            // First and last, not all twelve: the span is contiguous in tab
            // order, so its ends and its size say the whole of it.
            Target::Spanned(ranges) => match (ranges.first(), ranges.last()) {
                (Some(first), Some(last)) => format!(
                    "{} … {} ({} sheets)",
                    workbook.cite_range(*first),
                    workbook.cite_range(*last),
                    ranges.len()
                ),
                _ => "no sheets".to_string(),
            },
            Target::UnknownSheet(name) => format!("#REF! — no sheet called {name:?}"),
            Target::ExternalWorkbook(token) => format!("another workbook, written as [{token}]"),
        }
    }
}

/// One reference, from the cell that wrote it to what it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    /// The cell whose formula contains it.
    pub from: CellRef,
    /// The reference exactly as written, e.g. `'Q3 Sales'!$B$2:$B$99`.
    pub text: String,
    pub target: Target,
}

/// What a scan looked at, so a caller can tell a small answer from a capped one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    /// Formula cells examined.
    pub formulas_scanned: u64,
    /// References read out of them.
    pub references_scanned: u64,
    /// Results found, including any past the cap.
    pub matches: u64,
    /// Whether the cap stopped the result list short. The counts above are
    /// exact regardless; only the list is capped.
    pub capped: bool,
}

/// The cells of a range, in row-major order.
///
/// Only the populated ones: an empty cell is absent from the sheet, and
/// inventing a row of blanks would make a sparse region look dense.
pub fn cells_in(workbook: &Workbook, range: RangeRef, limit: usize) -> (Vec<CellFact>, bool) {
    let Some(sheet) = workbook.sheet(range.sheet) else {
        return (Vec::new(), false);
    };
    // `iter_range` probes one `BTreeMap` range per row of `range`'s height,
    // which is fine for an ordinary citation but not for a whole-column
    // reference (`Sheet1!A:A`, 1,048,576 rows) now that those parse — a
    // sheet's used range is never larger than what is actually there to find,
    // so clipping to it first bounds the probe count by the sheet's real
    // size rather than the reference's stated one.
    let Some(range) = sheet
        .used_range()
        .and_then(|used| range.intersection(&used))
    else {
        return (Vec::new(), false);
    };
    let mut out = Vec::new();
    let mut capped = false;
    for (at, cell) in sheet.iter_range(range) {
        if out.len() >= limit {
            capped = true;
            break;
        }
        out.push(fact(workbook, at, cell));
    }
    out.sort_by_key(|f| (f.cell.row, f.cell.col));
    (out, capped)
}

/// One cell.
pub fn cell(workbook: &Workbook, at: CellRef) -> Option<CellFact> {
    let sheet = workbook.sheet(at.sheet)?;
    sheet.get_ref(at).map(|c| fact(workbook, at, c))
}

fn fact(workbook: &Workbook, at: CellRef, cell: &eg_model::Cell) -> CellFact {
    CellFact {
        cell: at,
        a1: workbook.cite(at),
        kind: cell.value.kind(),
        formula: cell.formula.clone(),
        value: cell.value.clone(),
    }
}

/// The cells a formula reads, resolved but not followed.
///
/// One cell's own text, so this is as cheap as reading it. `None` when the cell
/// is absent or holds no formula — a literal reads nothing, which is a
/// different answer from "not found" only to the caller that cares.
pub fn precedents_of(workbook: &Workbook, at: CellRef) -> Vec<Reference> {
    let Some(sheet) = workbook.sheet(at.sheet) else {
        return Vec::new();
    };
    let Some(formula) = sheet.get_ref(at).and_then(|c| c.formula.as_deref()) else {
        return Vec::new();
    };

    let sheets = sheet_ids(workbook);
    let mut spans = Vec::new();
    scan_references_into(formula, &mut spans);
    spans
        .iter()
        .map(|span| resolve(at, span, formula, &sheets))
        .collect()
}

/// The cells whose formulas read anything in `range`.
///
/// This is the expensive direction: nothing records who reads a cell, so every
/// formula in the workbook is scanned. Linear in the workbook, which on the
/// reference file is 6.79 million formulas — seconds, not milliseconds, and the
/// reason this is a separate function from [`precedents_of`].
///
/// `limit` caps the returned list; the counts in the report are exact.
pub fn dependents_of(
    workbook: &Workbook,
    range: RangeRef,
    limit: usize,
) -> (Vec<Reference>, ScanReport) {
    let sheets = sheet_ids(workbook);
    let mut out = Vec::new();
    let mut report = ScanReport::default();
    // Reused across every formula in the workbook, which is the whole reason
    // `scan_references_into` takes a buffer.
    let mut spans: Vec<ReferenceSpan> = Vec::new();

    for sheet in &workbook.sheets {
        for (at, cell) in sheet.iter() {
            let Some(formula) = cell.formula.as_deref() else {
                continue;
            };
            report.formulas_scanned += 1;
            scan_references_into(formula, &mut spans);
            for span in &spans {
                report.references_scanned += 1;
                let reference = resolve(at, span, formula, &sheets);
                // Any sheet of a 3-D span reading the range makes the formula
                // a dependent, and it is reported once however many do.
                if reference
                    .target
                    .ranges()
                    .iter()
                    .any(|t| overlaps(*t, range))
                {
                    report.matches += 1;
                    if out.len() < limit {
                        out.push(reference);
                    } else {
                        report.capped = true;
                    }
                }
            }
        }
    }
    (out, report)
}

/// Whether two ranges share a cell.
pub(crate) fn overlaps(a: RangeRef, b: RangeRef) -> bool {
    a.sheet == b.sheet
        && a.top <= b.bottom
        && b.top <= a.bottom
        && a.left <= b.right
        && b.left <= a.right
}

/// Resolve one scanned reference against the workbook.
pub(crate) fn resolve(
    at: CellRef,
    span: &ReferenceSpan,
    formula: &str,
    sheets: &FxHashMap<String, SheetId>,
) -> Reference {
    let parsed = &span.parsed;
    let text = span.text(formula).to_string();

    if let Some(token) = &parsed.workbook {
        return Reference {
            from: at,
            text,
            target: Target::ExternalWorkbook(token.clone()),
        };
    }

    // Every sheet the reference names — the formula's own for an unqualified
    // one, and all of `Jan:Dec` for a 3-D one. Decided by `eg-model` rather
    // than here, because the graph lifts against the same function: this
    // layer having its own answer, the start sheet and nothing else, made a
    // what-if report every other sheet of a span as unaffected.
    let target =
        match parsed.spanned_sheets(at.sheet, |name| sheets.get(&name.to_uppercase()).copied()) {
            Err(name) => Target::UnknownSheet(name.to_string()),
            Ok(span) if !span.is_multi_sheet() => Target::Cells(parsed.resolve(span.first())),
            Ok(span) => Target::Spanned(span.iter().map(|s| parsed.resolve(s)).collect()),
        };
    Reference {
        from: at,
        text,
        target,
    }
}

/// Sheet ids by upper-cased name, because Excel sheet names are
/// case-insensitive and a formula may not spell one the way the tab does.
pub(crate) fn sheet_ids(workbook: &Workbook) -> FxHashMap<String, SheetId> {
    workbook
        .sheets
        .iter()
        .map(|s: &Sheet| (s.name.to_uppercase(), s.id))
        .collect()
}
