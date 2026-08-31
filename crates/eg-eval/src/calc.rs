//! Recomputing a formula from the values under it.
//!
//! [`trace`](crate::trace) says which cells a formula stands on. This works out
//! what they add up to, and compares that with the number the workbook has
//! stored — the one Excel last calculated. A recompute that agrees is evidence
//! the stored number means what the formula says it means; one that disagrees
//! is a stale cache, a construct modelled wrongly here, or a real defect, and
//! the caller gets both numbers and the inputs to tell which.
//!
//! # One formula, not the chain behind it
//!
//! Precedents are read as *stored values*, never recursively recomputed. That
//! is a deliberate limit and it is what makes the answer usable: it isolates
//! this cell's arithmetic, so a disagreement is about this formula and nothing
//! else. Each precedent is itself a cell with a formula, and checking it is
//! another call — which also means no dependency order to compute, no cycles to
//! detect, and no risk of one stale value quietly poisoning a thousand results.
//!
//! # Saying "I don't know"
//!
//! A spreadsheet has hundreds of functions and this models a few dozen. The
//! rest are [`Unsupported`] — an outcome, not an error, and never a guess. An
//! evaluator that returned a plausible number for a function it does not
//! implement would be worse than one that returns nothing, because nothing is
//! visibly nothing.
//!
//! Volatile functions are refused by name rather than attempted: `TODAY()`
//! recomputed today is not what the workbook computed when it was saved, so
//! "differs" would be the wrong verdict even when both numbers are right.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;

use eg_model::{CellRef, CellValue, ErrorKind, ParsedRef, RangeRef, SheetId, ValueKind, Workbook};

/// Re-exported so the rest of this crate can keep importing it as
/// `crate::calc::shown`. Lives in `eg-model` (see there for why) because
/// `eg-structure`, upstream of this crate in the pipeline, needs it too.
pub(crate) use eg_model::shown;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::parse::{parse, BinOp, Expr, UnaryOp};

/// Relative tolerance for calling two numbers the same.
///
/// Stored values are doubles that Excel wrote after its own summation order;
/// ours may associate a sum differently and land a few ULPs away. A tolerance
/// this tight cannot hide a real disagreement — a wrong formula is wrong in the
/// third digit, not the fifteenth.
pub const TOLERANCE: f64 = 1e-9;

/// How deep an expression may nest before it is refused rather than recursed.
const MAX_DEPTH: u32 = 128;

/// How long a lookup column has to be before indexing it beats walking it.
///
/// Below this a linear scan wins outright: it stops at the first match and
/// hashes nothing. Above it, one workbook holds one lookup table and millions
/// of formulas asking it questions, which is a different problem.
const INDEX_LOOKUPS_OVER: u32 = 512;

/// What an evaluated expression is: a value, or a reference to cells.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Scalar(CellValue),
    Range(RangeRef),
}

/// Something in the formula this crate does not model.
///
/// Distinct from an Excel error value: `#DIV/0!` is an answer, this is the
/// absence of one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unsupported {
    /// A function not implemented here. Upper-cased.
    Function(String),
    /// A function whose value depends on when you ask, so recomputing it says
    /// nothing about the stored result.
    Volatile(String),
    /// A defined name. Resolving one means following `refers_to`, which is
    /// another formula, and the graph does not keep what it resolved to.
    Name(String),
    /// A reference into another workbook, which is not loaded.
    ExternalWorkbook(String),
    /// A 3-D reference such as `Jan:Dec!B2`.
    ThreeDReference(String),
    /// Multiple cells where one value was needed — Excel's implicit
    /// intersection, which depends on the shape of the formula's own row.
    RangeAsValue(String),
    /// A form of a function that is modelled in general but not in this
    /// shape, such as `INDEX` asked for a whole column.
    Form(String),
    /// A whole-column or whole-row reference (`A:A`, `3:3`), or an explicit
    /// reference that spans the same ground (`A1:A1048576`). `eg-graph` lifts
    /// these to every region they touch, but recomputing one deliberately
    /// refuses rather than evaluates it, by shape rather than by the scan
    /// miss that used to make this construct fail to parse at all: a
    /// workbook using this shorthand for a lookup table's height, say,
    /// deserves a stated refusal, not a guess at which convention (used
    /// range? all populated cells? something else?) Excel would have applied.
    WholeColumnOrRow(String),
    /// The text could not be turned into an expression tree.
    Unparsed(String),
    /// Nesting past [`MAX_DEPTH`].
    TooDeep,
}

impl Unsupported {
    /// A short key for grouping, so a sweep can report *what* it could not do
    /// rather than one line per cell that hit it.
    pub fn key(&self) -> String {
        match self {
            Unsupported::Function(name) => format!("{name}()"),
            Unsupported::Volatile(name) => format!("{name}() is volatile"),
            Unsupported::Name(_) => "defined name".to_string(),
            Unsupported::ExternalWorkbook(_) => "external workbook".to_string(),
            Unsupported::ThreeDReference(_) => "3-D reference".to_string(),
            Unsupported::RangeAsValue(_) => "implicit intersection".to_string(),
            Unsupported::Form(what) => what.clone(),
            Unsupported::WholeColumnOrRow(_) => "whole column/row reference".to_string(),
            // The offset varies from formula to formula; the construct does
            // not, and the construct is what a sweep is asking about.
            Unsupported::Unparsed(what) => {
                let construct = what.split(" at byte").next().unwrap_or(what);
                format!("unparsed: {construct}")
            }
            Unsupported::TooDeep => "nested too deeply".to_string(),
        }
    }
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unsupported::Function(name) => write!(f, "{name}() is not implemented"),
            Unsupported::Volatile(name) => write!(f, "{name}() is volatile"),
            Unsupported::Name(name) => write!(f, "defined name {name}"),
            Unsupported::ExternalWorkbook(token) => write!(f, "external workbook [{token}]"),
            Unsupported::ThreeDReference(text) => write!(f, "3-D reference {text}"),
            Unsupported::RangeAsValue(text) => {
                write!(f, "{text} is many cells where one value was needed")
            }
            Unsupported::Form(what) => write!(f, "{what}"),
            Unsupported::WholeColumnOrRow(text) => {
                write!(f, "{text} is a whole-column or whole-row reference")
            }
            Unsupported::Unparsed(what) => write!(f, "not parsed: {what}"),
            Unsupported::TooDeep => write!(f, "nested more than {MAX_DEPTH} deep"),
        }
    }
}

/// One reference the recompute read, in the order the formula names them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    /// The reference exactly as written, e.g. `Rates!$B$2`.
    pub text: String,
    /// A fully-qualified citation of what it resolved to.
    pub a1: String,
    pub range: RangeRef,
    /// The value read, for a single cell. `None` for a multi-cell range, whose
    /// contribution depends on the function that consumed it.
    pub value: Option<CellValue>,
    /// Cells addressed, by geometry — not how many are populated.
    pub cells: u64,
}

/// What came of recomputing one cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    /// Recomputed, and equal to the stored value within [`TOLERANCE`].
    Agrees(CellValue),
    Differs {
        computed: CellValue,
        stored: CellValue,
    },
    /// Not recomputed, and deliberately not guessed.
    Unsupported(Unsupported),
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Agrees(_) => "agrees",
            Outcome::Differs { .. } => "differs",
            Outcome::Unsupported(_) => "unsupported",
        }
    }

    pub fn agrees(&self) -> bool {
        matches!(self, Outcome::Agrees(_))
    }
}

/// One cell, recomputed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recomputed {
    pub cell: CellRef,
    /// A fully-qualified citation, so a caller can hand it back unchanged.
    pub a1: String,
    /// The formula as written, without its `=`.
    pub formula: String,
    pub outcome: Outcome,
    /// The references the evaluation resolved, whether or not it got an answer.
    /// This is what makes a verdict checkable rather than merely stated.
    pub inputs: Vec<Input>,
}

/// Recompute one cell from the stored values of the cells it reads.
///
/// `None` when the cell is absent or holds no formula: a literal has nothing to
/// recompute, and agreeing with itself would be a verdict about nothing.
pub fn recompute(workbook: &Workbook, at: CellRef) -> Option<Recomputed> {
    recompute_over(workbook, at, &Overrides::new())
}

/// Recompute one cell with some of the workbook's values substituted.
///
/// The comparison is still against the value the workbook stored, so an
/// [`Outcome::Differs`] here says the substitution moved this cell — which is
/// what a what-if is asking. [`crate::whatif`] is the same question asked of
/// everything downstream rather than one cell.
pub fn recompute_over(
    workbook: &Workbook,
    at: CellRef,
    overrides: &Overrides,
) -> Option<Recomputed> {
    let cell = workbook.sheet(at.sheet)?.get_ref(at)?;
    let formula = cell.formula.clone()?;
    Some(recompute_with(
        workbook,
        at,
        &formula,
        &cell.value,
        &sheet_ids(workbook),
        &mut LookupIndex::default(),
        overrides,
    ))
}

/// A workbook set up to be recomputed many times over.
///
/// [`recompute_over`] is a whole standing start: the sheet-name map is built and
/// an empty lookup index allocated for one cell. That is right for one cell and
/// wrong a million times in a row, which is what a what-if does — so the walk
/// holds one of these instead, exactly as [`check`] holds its own.
///
/// The lookup index is the reason this is a type rather than a pair of
/// arguments. It caches a lookup column as it was read, so a substitution
/// *into* such a column would be answered from a map that predates it. The
/// caller says when that happens, with [`Evaluator::invalidate`], and the
/// cached column is dropped.
pub struct Evaluator<'a> {
    workbook: &'a Workbook,
    sheets: FxHashMap<String, SheetId>,
    index: LookupIndex,
}

impl<'a> Evaluator<'a> {
    pub fn new(workbook: &'a Workbook) -> Self {
        Evaluator {
            workbook,
            sheets: sheet_ids(workbook),
            index: LookupIndex::default(),
        }
    }

    /// As [`recompute_over`], reusing everything that does not depend on the
    /// cell.
    pub fn recompute_over(&mut self, at: CellRef, overrides: &Overrides) -> Option<Recomputed> {
        let cell = self.workbook.sheet(at.sheet)?.get_ref(at)?;
        let formula = cell.formula.clone()?;
        let stored = cell.value.clone();
        Some(recompute_with(
            self.workbook,
            at,
            &formula,
            &stored,
            &self.sheets,
            &mut self.index,
            overrides,
        ))
    }

    /// Forget any cached lookup column that `at` sits in.
    ///
    /// Called by whoever changes an override, because a cached column is only
    /// as good as the values it was built from. Cheap: a workbook has a handful
    /// of tables it looks things up in, not one per formula.
    pub fn invalidate(&mut self, at: CellRef) {
        if self.index.columns.is_empty() {
            return;
        }
        self.index.columns.retain(|&(sheet, col, top, bottom), _| {
            !(sheet == at.sheet && col == at.col && (top..=bottom).contains(&at.row))
        });
    }
}

/// Evaluate arbitrary formula text as if it sat in `at`, without comparing it
/// to anything. This is the what-if entry point: relative references resolve
/// against `at`, exactly as they would if the text were typed there.
pub fn evaluate(workbook: &Workbook, at: CellRef, formula: &str) -> Result<CellValue, Unsupported> {
    evaluate_over(workbook, at, formula, &Overrides::new())
}

/// Evaluate formula text with some of the workbook's values substituted.
pub fn evaluate_over(
    workbook: &Workbook,
    at: CellRef,
    formula: &str,
    overrides: &Overrides,
) -> Result<CellValue, Unsupported> {
    let sheets = sheet_ids(workbook);
    let mut index = LookupIndex::default();
    let mut eval = Eval::new(workbook, at, &sheets, &mut index, overrides);
    let expr = parse(formula).map_err(|e| Unsupported::Unparsed(e.to_string()))?;
    let value = eval.eval(&expr)?;
    eval.result(value)
}

fn recompute_with(
    workbook: &Workbook,
    at: CellRef,
    formula: &str,
    stored: &CellValue,
    sheets: &FxHashMap<String, SheetId>,
    index: &mut LookupIndex,
    overrides: &Overrides,
) -> Recomputed {
    let mut eval = Eval::new(workbook, at, sheets, index, overrides);
    let outcome = match parse(formula) {
        Err(e) => Outcome::Unsupported(Unsupported::Unparsed(e.to_string())),
        Ok(expr) => match eval.eval(&expr).and_then(|value| eval.result(value)) {
            Err(reason) => Outcome::Unsupported(reason),
            Ok(computed) => {
                if same(&computed, stored) {
                    Outcome::Agrees(computed)
                } else {
                    Outcome::Differs {
                        computed,
                        stored: stored.clone(),
                    }
                }
            }
        },
    };
    Recomputed {
        cell: at,
        a1: workbook.cite(at),
        formula: formula.to_string(),
        outcome,
        inputs: eval.inputs,
    }
}

/// Whether a recomputed value counts as the stored one.
///
/// Numbers compare within [`TOLERANCE`], relative to their own magnitude.
/// Everything else compares exactly, except blankness: a formula returning `""`
/// and a cell read back as empty are the same fact written two ways.
pub fn same(computed: &CellValue, stored: &CellValue) -> bool {
    match (computed, stored) {
        (CellValue::Number(a), CellValue::Number(b)) => close(*a, *b),
        (CellValue::Error(a), CellValue::Error(b)) => a == b,
        (CellValue::Bool(a), CellValue::Bool(b)) => a == b,
        (CellValue::Text(a), CellValue::Text(b)) => a == b,
        (a, b) if a.is_empty() && b.is_empty() => true,
        // A blank cell is zero in every numeric context, and a formula that
        // returns one is stored as zero.
        (CellValue::Empty, CellValue::Number(n)) | (CellValue::Number(n), CellValue::Empty) => {
            close(*n, 0.0)
        }
        _ => false,
    }
}

fn close(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    if !a.is_finite() || !b.is_finite() {
        return false;
    }
    (a - b).abs() <= TOLERANCE * a.abs().max(b.abs()).max(1.0)
}

/// What a sweep looked at, and what it could not do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    pub formulas: u64,
    pub agreed: u64,
    pub differed: u64,
    pub unsupported: u64,
    /// Reasons for the unsupported ones, by [`Unsupported::key`], commonest
    /// first. This is the list that says what to implement next.
    pub reasons: Vec<(String, u64)>,
    /// Whether the returned list of disagreements was cut short. The counts
    /// above are exact regardless.
    pub capped: bool,
}

/// Recompute every formula in `scope`, or in the whole workbook when it is
/// `None`.
///
/// The returned list holds the *disagreements* — the cells worth looking at —
/// capped at `limit`. Everything else is in the counts, because a sweep of a
/// six-million-formula workbook that returned six million results would be a
/// second workbook.
pub fn check(
    workbook: &Workbook,
    scope: Option<RangeRef>,
    limit: usize,
) -> (Vec<Recomputed>, CheckReport) {
    let ids = sheet_ids(workbook);
    // One index for the whole sweep: the tables a workbook looks things up in
    // are the same tables for every formula that asks.
    let mut index = LookupIndex::default();
    let mut out = Vec::new();
    let mut report = CheckReport::default();
    let mut reasons: FxHashMap<String, u64> = FxHashMap::default();

    // A scope is a range on one sheet, and scanning it as a range rather than
    // filtering the sheet is the difference between reading a table and reading
    // the sheet it sits on.
    let sheets: Vec<&eg_model::Sheet> = match scope {
        Some(range) => workbook.sheet(range.sheet).into_iter().collect(),
        None => workbook.sheets.iter().collect(),
    };
    for sheet in sheets {
        let cells: Box<dyn Iterator<Item = (CellRef, &eg_model::Cell)>> = match scope {
            Some(range) => Box::new(sheet.iter_range(range)),
            None => Box::new(sheet.iter()),
        };
        for (at, cell) in cells {
            let Some(formula) = cell.formula.as_deref() else {
                continue;
            };
            report.formulas += 1;
            let result = recompute_with(
                workbook,
                at,
                formula,
                &cell.value,
                &ids,
                &mut index,
                &Overrides::new(),
            );
            match &result.outcome {
                Outcome::Agrees(_) => report.agreed += 1,
                Outcome::Unsupported(reason) => {
                    report.unsupported += 1;
                    *reasons.entry(reason.key()).or_default() += 1;
                }
                Outcome::Differs { .. } => {
                    report.differed += 1;
                    if out.len() < limit {
                        out.push(result);
                    } else {
                        report.capped = true;
                    }
                }
            }
        }
    }

    report.reasons = reasons.into_iter().collect();
    report
        .reasons
        .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    (out, report)
}

/// Sheet ids by upper-cased name, as [`crate::trace`] resolves them: Excel's
/// sheet names are case-insensitive and a formula need not spell one the way
/// its tab does.
fn sheet_ids(workbook: &Workbook) -> FxHashMap<String, SheetId> {
    workbook
        .sheets
        .iter()
        .map(|s| (s.name.to_uppercase(), s.id))
        .collect()
}

/// A spreadsheet value as something hashable.
///
/// Built to match what [`compare_for_lookup`] would say: a number never equals
/// text however it reads, text ignores case, and a number is keyed on the 15
/// digits a sheet shows rather than on its bits, so a lookup finds 16.88 when
/// the cell holds the double just above it.
#[derive(PartialEq, Eq, Hash)]
enum Key {
    Number(u64),
    Text(String),
    Bool(bool),
}

impl Key {
    fn of(value: &CellValue) -> Option<Key> {
        Some(match value {
            CellValue::Number(n) => {
                let n = shown(*n);
                // -0.0 and 0.0 are the same cell to a spreadsheet and different
                // bit patterns to a hash.
                Key::Number(if n == 0.0 {
                    0f64.to_bits()
                } else {
                    n.to_bits()
                })
            }
            CellValue::Text(s) => Key::Text(s.to_uppercase()),
            CellValue::Bool(b) => Key::Bool(*b),
            CellValue::Empty | CellValue::Error(_) => return None,
        })
    }
}

/// Cell values standing in for the workbook's own.
///
/// This is what makes a what-if a question rather than an edit: the workbook is
/// never modified — it cannot be, for XLSB, which no Rust crate can write — so
/// a substituted value lives here and every read goes through it.
///
/// Empty in an ordinary recompute, and the evaluator checks that before it
/// hashes anything, so the sweep pays a branch rather than a lookup per cell.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    values: FxHashMap<CellRef, CellValue>,
    /// The same addresses, ordered by column and then row within each sheet, so
    /// that reading a *range* costs a bounded lookup rather than a walk over
    /// every substitution.
    ///
    /// Column-major because a spreadsheet range is usually tall and narrow: in
    /// row order, a one-column range's own cells are separated by every
    /// substitution on the rows between them. Needed because a what-if's
    /// overlay is not the handful of cells this type was first written for —
    /// the walk puts every cell it recomputes into it, which on the reference
    /// workbook is 1.2 million.
    index: FxHashMap<SheetId, BTreeSet<(u16, u32)>>,
}

impl Overrides {
    pub fn new() -> Self {
        Self::default()
    }

    /// Substitute `value` for whatever `at` holds.
    pub fn set(&mut self, at: CellRef, value: CellValue) {
        if self.values.insert(at, value).is_none() {
            self.index
                .entry(at.sheet)
                .or_default()
                .insert((at.col, at.row));
        }
    }

    pub fn get(&self, at: CellRef) -> Option<&CellValue> {
        if self.values.is_empty() {
            return None;
        }
        self.values.get(&at)
    }

    pub fn contains(&self, at: CellRef) -> bool {
        self.get(at).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn cells(&self) -> impl Iterator<Item = (CellRef, &CellValue)> + '_ {
        self.values.iter().map(|(at, v)| (*at, v))
    }

    /// The substitutions inside `range`, for a read that walks cells rather
    /// than naming one.
    ///
    /// Returned as a vector because the two ways of finding them have different
    /// shapes, and because the answer is empty for almost every range — an
    /// empty `Vec` allocates nothing, which is what this costs on the reads
    /// that have no substitution in them.
    fn in_range(&self, range: RangeRef) -> Vec<(CellRef, &CellValue)> {
        let Some(index) = self.index.get(&range.sheet) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut push = |col: u16, row: u32| {
            let at = CellRef::new(range.sheet, row, col);
            if let Some(value) = self.values.get(&at) {
                out.push((at, value));
            }
        };
        let columns = usize::from(range.right - range.left) + 1;
        if columns >= index.len() {
            // A range wider than the sheet has substitutions — a whole-row or
            // whole-sheet reference — is cheaper walked than probed per column.
            for &(col, row) in index.iter() {
                if (range.left..=range.right).contains(&col)
                    && (range.top..=range.bottom).contains(&row)
                {
                    push(col, row);
                }
            }
        } else {
            for col in range.left..=range.right {
                for &(_, row) in index.range((col, range.top)..=(col, range.bottom)) {
                    push(col, row);
                }
            }
        }
        out
    }
}

/// Exact-match lookup columns, built once and asked many times.
///
/// A lookup walks its first column until it matches. One formula doing that is
/// nothing; 115,004 of them into the same 100,000-row table is a quarter of a
/// trillion probes, and that is an ordinary shape for a workbook — one table of
/// rates, one formula per debtor. So the column is turned into a map the first
/// time it is asked and answered from the map after that.
///
/// Only exact matches are indexed. An approximate lookup wants the last row not
/// past the key, which is an ordering question a hash cannot answer.
///
/// The index is valid only while the workbook does not change, which is the
/// whole life of a sweep — and, since it is built through whatever
/// [`Overrides`] were in force, only for those. A what-if that substitutes a
/// value into a lookup column must not reuse an index built without it.
#[derive(Default)]
pub struct LookupIndex {
    columns: FxHashMap<(SheetId, u16, u32, u32), FxHashMap<Key, u32>>,
}

impl LookupIndex {
    /// The first row offset in `column` holding `key`, or `None`.
    fn find(
        &mut self,
        workbook: &Workbook,
        overrides: &Overrides,
        column: RangeRef,
        key: &CellValue,
    ) -> Option<u32> {
        let key = Key::of(key)?;
        let (top, left) = (column.top, column.left);
        let built = self
            .columns
            .entry((column.sheet, left, top, column.bottom))
            .or_insert_with(|| {
                let mut map = FxHashMap::default();
                let Some(sheet) = workbook.sheet(column.sheet) else {
                    return map;
                };
                let range = column;
                for (at, cell) in sheet.iter_range(range) {
                    let value = match overrides.get(at) {
                        Some(v) => v,
                        None => &cell.value,
                    };
                    if let Some(key) = Key::of(value) {
                        // First occurrence wins, as a lookup takes the first
                        // row that matches.
                        map.entry(key).or_insert(at.row - top);
                    }
                }
                // A substitution into a cell the sheet leaves empty is a row
                // the loop above never saw.
                for (at, value) in overrides.in_range(range) {
                    if let Some(key) = Key::of(value) {
                        map.entry(key).or_insert(at.row - top);
                    }
                }
                map
            });
        built.get(&key).copied()
    }
}

struct Eval<'a> {
    workbook: &'a Workbook,
    at: CellRef,
    sheets: &'a FxHashMap<String, SheetId>,
    index: &'a mut LookupIndex,
    overrides: &'a Overrides,
    inputs: Vec<Input>,
    depth: u32,
}

impl<'a> Eval<'a> {
    fn new(
        workbook: &'a Workbook,
        at: CellRef,
        sheets: &'a FxHashMap<String, SheetId>,
        index: &'a mut LookupIndex,
        overrides: &'a Overrides,
    ) -> Self {
        Self {
            workbook,
            at,
            sheets,
            index,
            overrides,
            inputs: Vec::new(),
            depth: 0,
        }
    }

    /// The value a cell would show. A reference left over at the top of a
    /// formula is dereferenced, and a blank one shows as zero — `=A1` over an
    /// empty A1 stores 0, not blank. A multi-cell range left at the top
    /// (`=A1:C1`, implicit intersection/spill) is `deref`'s one refusal,
    /// [`Unsupported::RangeAsValue`], and is propagated rather than mapped to
    /// a guessed `#VALUE!` — the same "refused by name, not guessed" rule
    /// unsupported functions get.
    fn result(&self, value: Value) -> Result<CellValue, Unsupported> {
        Ok(match self.deref(value)? {
            CellValue::Empty => CellValue::Number(0.0),
            value => value,
        })
    }

    fn eval(&mut self, expr: &Expr) -> Result<Value, Unsupported> {
        if self.depth >= MAX_DEPTH {
            return Err(Unsupported::TooDeep);
        }
        self.depth += 1;
        let out = self.eval_inner(expr);
        self.depth -= 1;
        out
    }

    fn eval_inner(&mut self, expr: &Expr) -> Result<Value, Unsupported> {
        match expr {
            Expr::Literal(value) => Ok(Value::Scalar(value.clone())),
            Expr::Reference { parsed, text } => self.reference(parsed, text),
            Expr::Name { sheet, name } => self.defined_name(sheet.as_deref(), name),
            Expr::Unary { op, arg } => {
                let arg = self.scalar(arg)?;
                Ok(Value::Scalar(unary(*op, &arg)))
            }
            Expr::Binary { op, lhs, rhs } => {
                let lhs = self.scalar(lhs)?;
                let rhs = self.scalar(rhs)?;
                Ok(Value::Scalar(binary(*op, &lhs, &rhs)))
            }
            Expr::Call { name, args } => self.call(name, args),
        }
    }

    /// Resolve a reference and record it as an input.
    fn reference(&mut self, parsed: &ParsedRef, text: &str) -> Result<Value, Unsupported> {
        if let Some(token) = &parsed.workbook {
            return Err(Unsupported::ExternalWorkbook(token.clone()));
        }
        if parsed.end_sheet_name.is_some() {
            return Err(Unsupported::ThreeDReference(text.to_string()));
        }
        if parsed.is_whole_column() || parsed.is_whole_row() {
            return Err(Unsupported::WholeColumnOrRow(text.to_string()));
        }
        let sheet = match &parsed.sheet_name {
            None => self.at.sheet,
            Some(name) => match self.sheets.get(&name.to_uppercase()) {
                Some(&id) => id,
                // A sheet the workbook does not have is a #REF! break, which is
                // an answer: Excel shows the error rather than refusing.
                None => return Ok(Value::Scalar(CellValue::Error(ErrorKind::Ref))),
            },
        };
        let range = parsed.resolve(sheet);
        self.record(text.to_string(), range);
        Ok(Value::Range(range))
    }

    /// A defined name, resolved to what it refers to.
    ///
    /// Names are how a real workbook writes down the thing it looks up — the
    /// reference workbook has 855,000 formulas naming one — so refusing them
    /// would leave an eighth of it unrecomputable. The name is resolved the way
    /// Excel scopes it: a sheet's own name first, then the workbook's.
    ///
    /// What it refers to must itself be a plain reference. A name standing for
    /// a constant or a formula is refused rather than followed, because
    /// following it is evaluating a second formula in the first one's cell and
    /// the answer would no longer be about this cell.
    fn defined_name(&mut self, sheet: Option<&str>, name: &str) -> Result<Value, Unsupported> {
        let scope = match sheet {
            Some(qualifier) => match self.sheets.get(&qualifier.to_uppercase()) {
                Some(&id) => Some(id),
                None => return Ok(Value::Scalar(CellValue::Error(ErrorKind::Ref))),
            },
            None => Some(self.at.sheet),
        };
        let named = |want: Option<SheetId>| {
            self.workbook
                .defined_names
                .iter()
                .find(|d| d.scope == want && d.name.eq_ignore_ascii_case(name))
        };
        let Some(defined) = named(scope).or_else(|| named(None)) else {
            // Excel shows #NAME? for a name it cannot find, but a name this
            // crate failed to *understand* is not the same claim, so an unknown
            // name is reported rather than answered.
            return Err(Unsupported::Name(qualified(sheet, name)));
        };

        let refers_to = defined.refers_to.trim_start_matches('=');
        let Ok(parsed) = eg_model::parse_a1(refers_to) else {
            return Err(Unsupported::Name(qualified(sheet, name)));
        };
        if let Some(token) = &parsed.workbook {
            return Err(Unsupported::ExternalWorkbook(token.clone()));
        }
        if parsed.end_sheet_name.is_some() {
            return Err(Unsupported::ThreeDReference(refers_to.to_string()));
        }
        if parsed.is_whole_column() || parsed.is_whole_row() {
            return Err(Unsupported::WholeColumnOrRow(refers_to.to_string()));
        }
        let target = match &parsed.sheet_name {
            Some(named_sheet) => match self.sheets.get(&named_sheet.to_uppercase()) {
                Some(&id) => id,
                None => return Ok(Value::Scalar(CellValue::Error(ErrorKind::Ref))),
            },
            None => return Err(Unsupported::Name(qualified(sheet, name))),
        };
        let range = parsed.resolve(target);
        self.record(qualified(sheet, name), range);
        Ok(Value::Range(range))
    }

    /// Note what a reference resolved to, so the verdict can be checked against
    /// the cells it stands on.
    fn record(&mut self, text: String, range: RangeRef) {
        let value = (range.top == range.bottom && range.left == range.right)
            .then(|| self.cell_value(range.top_left()));
        self.inputs.push(Input {
            text,
            a1: self.workbook.cite_range(range),
            range,
            value,
            cells: range.cell_count(),
        });
    }

    fn cell_value(&self, at: CellRef) -> CellValue {
        if let Some(value) = self.overrides.get(at) {
            return value.clone();
        }
        self.workbook
            .sheet(at.sheet)
            .map(|s| s.value(at.row, at.col))
            .unwrap_or(CellValue::Empty)
    }

    /// A single value, dereferencing a one-cell range.
    fn scalar(&mut self, expr: &Expr) -> Result<CellValue, Unsupported> {
        let value = self.eval(expr)?;
        self.deref(value)
    }

    fn deref(&self, value: Value) -> Result<CellValue, Unsupported> {
        match value {
            Value::Scalar(v) => Ok(v),
            Value::Range(r) if r.top == r.bottom && r.left == r.right => {
                Ok(self.cell_value(r.top_left()))
            }
            // Excel would intersect the range with the formula's own row or
            // column, which depends on where the formula sits and is a
            // different question from what it computes.
            Value::Range(r) => Err(Unsupported::RangeAsValue(self.workbook.cite_range(r))),
        }
    }

    /// The cells of a range that can hold anything, clipped to what the sheet
    /// actually uses. A million-row reference costs its populated cells, not
    /// its geometry.
    fn populated(&self, range: RangeRef) -> impl Iterator<Item = Cow<'_, CellValue>> + '_ {
        let sheet = self.workbook.sheet(range.sheet);
        let clipped = sheet
            .and_then(|s| s.used_range())
            .and_then(|used| intersect(range, used));
        let overrides = self.overrides;

        // A substitution into a cell the sheet leaves empty is not in the grid
        // to be walked over, and a range that addresses it still reads it. They
        // are gathered and sorted rather than appended, because they have to
        // arrive in address order among the stored cells: `SUM` does not care
        // what order it adds in, `CONCAT` cares entirely.
        //
        // Skipped outright when there is nothing substituted, which is every
        // read outside a what-if — `in_range` walks the whole overlay, and
        // during a walk that overlay holds every cell that has moved so far.
        let mut extra: Vec<(CellRef, &CellValue)> = Vec::new();
        if !overrides.is_empty() {
            extra.extend(
                overrides
                    .in_range(range)
                    .into_iter()
                    .filter(|(at, _)| sheet.map(|s| s.get_ref(*at).is_none()).unwrap_or(true)),
            );
            extra.sort_unstable_by_key(|(at, _)| (at.row, at.col));
        }

        let mut stored = clipped
            .into_iter()
            .flat_map(move |r| sheet.expect("clipped implies a sheet").iter_range(r))
            .peekable();
        let mut next = 0usize;
        std::iter::from_fn(move || {
            let ahead = extra.get(next).map(|(at, _)| (at.row, at.col));
            let take_extra = match (stored.peek(), ahead) {
                (Some((at, _)), Some(pos)) => (at.row, at.col) > pos,
                (None, Some(_)) => true,
                (_, None) => false,
            };
            if take_extra {
                let value = extra[next].1;
                next += 1;
                return Some(Cow::Borrowed(value));
            }
            let (at, cell) = stored.next()?;
            Some(match overrides.get(at) {
                Some(value) => Cow::Borrowed(value),
                None => Cow::Borrowed(&cell.value),
            })
        })
    }

    /// Every value an argument list contributes, flagged with whether it came
    /// from a range — which decides whether text and booleans count.
    /// Returns how many cells the arguments address by geometry, which is more
    /// than `visit` sees: an unpopulated cell is absent from the sheet, and
    /// only a function that counts blanks cares that it was addressed at all.
    fn spread(
        &mut self,
        args: &[Expr],
        mut visit: impl FnMut(&CellValue, bool),
    ) -> Result<u64, Unsupported> {
        let values: Vec<Value> = args
            .iter()
            .map(|a| self.eval(a))
            .collect::<Result<_, _>>()?;
        let mut addressed = 0u64;
        for value in values {
            match value {
                Value::Scalar(v) => {
                    addressed += 1;
                    visit(&v, false)
                }
                Value::Range(r) if r.top == r.bottom && r.left == r.right => {
                    addressed += 1;
                    visit(&self.cell_value(r.top_left()), true)
                }
                Value::Range(r) => {
                    addressed += r.cell_count();
                    for v in self.populated(r) {
                        visit(&v, true);
                    }
                }
            }
        }
        Ok(addressed)
    }

    /// The numbers an argument list contributes, by Excel's rules: text and
    /// booleans written into the formula are coerced, the same values sitting
    /// in a referenced range are ignored.
    fn numbers(&mut self, args: &[Expr]) -> Result<Result<Vec<f64>, ErrorKind>, Unsupported> {
        let mut out = Vec::new();
        let mut failed = None;
        self.spread(args, |value, from_range| {
            if failed.is_some() {
                return;
            }
            match (value, from_range) {
                (CellValue::Error(e), _) => failed = Some(*e),
                (CellValue::Number(n), _) => out.push(*n),
                (_, true) => {}
                (other, false) => match to_number(other) {
                    Ok(n) => out.push(n),
                    Err(e) => failed = Some(e),
                },
            }
        })?;
        Ok(match failed {
            Some(e) => Err(e),
            None => Ok(out),
        })
    }

    fn range_arg(&mut self, expr: &Expr) -> Result<Result<RangeRef, ErrorKind>, Unsupported> {
        match self.eval(expr)? {
            Value::Range(r) => Ok(Ok(r)),
            Value::Scalar(CellValue::Error(e)) => Ok(Err(e)),
            Value::Scalar(_) => Ok(Err(ErrorKind::Value)),
        }
    }

    fn cell_at(&self, range: RangeRef, row_offset: u32, col_offset: u16) -> CellValue {
        let row = range.top + row_offset;
        let col = range.left + col_offset;
        if row > range.bottom || col > range.right {
            return CellValue::Error(ErrorKind::Ref);
        }
        self.cell_value(CellRef::new(range.sheet, row, col))
    }
}

/// A name as it was written, for citing it back.
fn qualified(sheet: Option<&str>, name: &str) -> String {
    match sheet {
        Some(sheet) => format!("{}!{name}", eg_model::quote_sheet_name(sheet)),
        None => name.to_string(),
    }
}

fn intersect(a: RangeRef, b: RangeRef) -> Option<RangeRef> {
    let top = a.top.max(b.top);
    let left = a.left.max(b.left);
    let bottom = a.bottom.min(b.bottom);
    let right = a.right.min(b.right);
    (top <= bottom && left <= right).then(|| RangeRef::new(a.sheet, top, left, bottom, right))
}

// ---- coercion -----------------------------------------------------------

fn to_number(value: &CellValue) -> Result<f64, ErrorKind> {
    match value {
        CellValue::Number(n) => Ok(*n),
        CellValue::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        CellValue::Empty => Ok(0.0),
        CellValue::Error(e) => Err(*e),
        CellValue::Text(s) => {
            let t = s.trim();
            if t.is_empty() {
                return Err(ErrorKind::Value);
            }
            // Rust's `f64::from_str` accepts "inf", "infinity" and "nan"
            // (any case) as valid floats; Excel has none of the three, and a
            // cell holding the text "inf" coercing to a number Excel could
            // never produce is worse than refusing it as text that is not
            // one — `#VALUE!`, the same as any other unparseable text.
            match t.parse::<f64>() {
                Ok(n) if n.is_finite() => Ok(n),
                _ => Err(ErrorKind::Value),
            }
        }
    }
}

fn to_text(value: &CellValue) -> Result<String, ErrorKind> {
    match value {
        CellValue::Error(e) => Err(*e),
        CellValue::Empty => Ok(String::new()),
        other => Ok(other.to_display()),
    }
}

fn to_bool(value: &CellValue) -> Result<bool, ErrorKind> {
    match value {
        CellValue::Bool(b) => Ok(*b),
        CellValue::Number(n) => Ok(*n != 0.0),
        CellValue::Empty => Ok(false),
        CellValue::Error(e) => Err(*e),
        CellValue::Text(s) if s.eq_ignore_ascii_case("TRUE") => Ok(true),
        CellValue::Text(s) if s.eq_ignore_ascii_case("FALSE") => Ok(false),
        CellValue::Text(_) => Err(ErrorKind::Value),
    }
}

fn error_of(value: &CellValue) -> Option<ErrorKind> {
    match value {
        CellValue::Error(e) => Some(*e),
        _ => None,
    }
}

// ---- operators ----------------------------------------------------------

fn unary(op: UnaryOp, arg: &CellValue) -> CellValue {
    if let Some(e) = error_of(arg) {
        return CellValue::Error(e);
    }
    // A unary plus passes its operand through untouched. It is not a coercion:
    // `=+"3 Points"` is that text, which matters because Lotus-style formulas
    // beginning `=+` are everywhere in real workbooks.
    if op == UnaryOp::Plus {
        return arg.clone();
    }
    let n = match to_number(arg) {
        Ok(n) => n,
        Err(e) => return CellValue::Error(e),
    };
    CellValue::Number(match op {
        UnaryOp::Neg => -n,
        UnaryOp::Percent => n / 100.0,
        UnaryOp::Plus => unreachable!("handled above"),
    })
}

fn binary(op: BinOp, lhs: &CellValue, rhs: &CellValue) -> CellValue {
    if let Some(e) = error_of(lhs).or_else(|| error_of(rhs)) {
        return CellValue::Error(e);
    }
    match op {
        BinOp::Concat => match (to_text(lhs), to_text(rhs)) {
            (Ok(a), Ok(b)) => CellValue::Text(a + &b),
            (Err(e), _) | (_, Err(e)) => CellValue::Error(e),
        },
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let ord = compare(lhs, rhs);
            CellValue::Bool(match op {
                BinOp::Eq => ord == std::cmp::Ordering::Equal,
                BinOp::Ne => ord != std::cmp::Ordering::Equal,
                BinOp::Lt => ord == std::cmp::Ordering::Less,
                BinOp::Le => ord != std::cmp::Ordering::Greater,
                BinOp::Gt => ord == std::cmp::Ordering::Greater,
                _ => ord != std::cmp::Ordering::Less,
            })
        }
        _ => {
            let (a, b) = match (to_number(lhs), to_number(rhs)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => return CellValue::Error(e),
            };
            match op {
                // Excel has no infinity: `1E308*10` is `#NUM!`, not a value
                // that then poisons every comparison downstream of it. All
                // four basic operators can overflow a finite pair of
                // operands into one, so all four are routed through the same
                // finite-or-`#NUM!` check `Pow` already used.
                BinOp::Add => number_or_num_error(cancelling(a, -b, a + b)),
                BinOp::Sub => number_or_num_error(cancelling(a, b, a - b)),
                BinOp::Mul => number_or_num_error(a * b),
                BinOp::Div if b == 0.0 => CellValue::Error(ErrorKind::Div0),
                BinOp::Div => number_or_num_error(a / b),
                BinOp::Pow => number_or_num_error(a.powf(b)),
                _ => unreachable!("comparison and concat handled above"),
            }
        }
    }
}

/// Excel's answer for an addition or subtraction whose operands cancel.
///
/// A sheet carries 15 significant digits, so two numbers equal in all of them
/// are the same number and their difference is zero — not the 1.49e-8 that the
/// doubles behind them differ by. Excel forces that result to zero, and a
/// column of differences that should read as empty otherwise fills with
/// fifteen-zero dust.
///
/// The same 15 digits decide [`compare`], for the same reason.
fn cancelling(lhs: f64, rhs: f64, result: f64) -> f64 {
    if result != 0.0 && shown(lhs) == shown(rhs) {
        0.0
    } else {
        result
    }
}

/// Excel has no infinity and no NaN: an operation that would produce one is
/// `#NUM!`.
fn number_or_num_error(n: f64) -> CellValue {
    if n.is_finite() {
        CellValue::Number(n)
    } else {
        CellValue::Error(ErrorKind::Num)
    }
}

/// Excel's ordering: numbers below text below booleans, text compared without
/// regard to case, and a blank taking the type of whatever it is compared with.
fn compare(lhs: &CellValue, rhs: &CellValue) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (lhs, rhs) = (fill_blank(lhs, rhs), fill_blank(rhs, lhs));
    match (&lhs, &rhs) {
        (CellValue::Number(a), CellValue::Number(b)) => {
            shown(*a).partial_cmp(&shown(*b)).unwrap_or(Ordering::Equal)
        }
        (CellValue::Text(a), CellValue::Text(b)) => a.to_uppercase().cmp(&b.to_uppercase()),
        (CellValue::Bool(a), CellValue::Bool(b)) => a.cmp(b),
        _ => rank(&lhs).cmp(&rank(&rhs)),
    }
}

/// A blank compared with a value of some type behaves as that type's zero.
fn fill_blank(value: &CellValue, other: &CellValue) -> CellValue {
    match (value, other) {
        (CellValue::Empty, CellValue::Text(_)) => CellValue::Text(String::new()),
        (CellValue::Empty, CellValue::Bool(_)) => CellValue::Bool(false),
        (CellValue::Empty, _) => CellValue::Number(0.0),
        (other, _) => other.clone(),
    }
}

fn rank(value: &CellValue) -> u8 {
    match value.kind() {
        ValueKind::Number | ValueKind::Empty => 0,
        ValueKind::Text => 1,
        ValueKind::Bool => 2,
        ValueKind::Error => 3,
    }
}

// ---- functions ----------------------------------------------------------

/// Functions whose value depends on when you ask, or on something outside the
/// stored grid. Recomputing one and comparing it with what the workbook saved
/// would produce a disagreement that says nothing about the workbook.
const VOLATILE: &[&str] = &[
    "TODAY",
    "NOW",
    "RAND",
    "RANDBETWEEN",
    "RANDARRAY",
    "OFFSET",
    "INDIRECT",
    "CELL",
    "INFO",
];

/// Every function name [`Eval::call`] actually evaluates (including `IF`,
/// `IFERROR`, `IFNA`, which are dispatched before the big `match` below).
///
/// A name outside this list — and outside [`VOLATILE`] — is unmodelled, and
/// must be refused by name (`Unsupported::Function`) rather than answered
/// with a guessed `#VALUE!`, however many arguments the call carries: a
/// zero-argument call to something like `PI()` or `ROW()` is not malformed,
/// it is simply not implemented here.
const MODELLED: &[&str] = &[
    "IF",
    "IFERROR",
    "IFNA",
    "SUM",
    "PRODUCT",
    "AVERAGE",
    "MIN",
    "MAX",
    "COUNT",
    "COUNTA",
    "COUNTBLANK",
    "AND",
    "OR",
    "NOT",
    "TRUE",
    "FALSE",
    "NA",
    "ISBLANK",
    "ISNUMBER",
    "ISTEXT",
    "ISNONTEXT",
    "ISLOGICAL",
    "ISERROR",
    "ISERR",
    "ISNA",
    "ABS",
    "INT",
    "SIGN",
    "SQRT",
    "EXP",
    "LN",
    "LOG10",
    "ROUND",
    "ROUNDUP",
    "ROUNDDOWN",
    "TRUNC",
    "MOD",
    "POWER",
    "LOG",
    "PV",
    "LEN",
    "TRIM",
    "UPPER",
    "LOWER",
    "LEFT",
    "RIGHT",
    "MID",
    "CONCATENATE",
    "CONCAT",
    "EXACT",
    "VLOOKUP",
    "HLOOKUP",
    "MATCH",
    "INDEX",
];

fn err(kind: ErrorKind) -> Value {
    Value::Scalar(CellValue::Error(kind))
}

fn num(n: f64) -> Value {
    Value::Scalar(CellValue::Number(n))
}

fn boolean(b: bool) -> Value {
    Value::Scalar(CellValue::Bool(b))
}

fn text(s: impl Into<String>) -> Value {
    Value::Scalar(CellValue::Text(s.into()))
}

/// Round the way a spreadsheet does: half away from zero, and on the number as
/// it would be *shown* rather than as it is stored. Excel carries 15 decimal
/// digits, so 2.675 rounds to 2.68 even though the nearest double is a hair
/// under; rounding the raw product would give 2.67 and disagree with every
/// stored value in a workbook that uses ROUND.
fn round_to(n: f64, digits: i32, mode: Rounding) -> f64 {
    if !n.is_finite() {
        return n;
    }
    let factor = 10f64.powi(digits.clamp(-300, 300));
    let scaled = n * factor;
    if !scaled.is_finite() {
        return n;
    }
    let scaled = shown(scaled);
    let rounded = match mode {
        Rounding::Half if scaled >= 0.0 => (scaled + 0.5).floor(),
        Rounding::Half => (scaled - 0.5).ceil(),
        Rounding::Up if scaled >= 0.0 => scaled.ceil(),
        Rounding::Up => scaled.floor(),
        Rounding::Down if scaled >= 0.0 => scaled.floor(),
        Rounding::Down => scaled.ceil(),
    };
    rounded / factor
}

#[derive(Clone, Copy)]
enum Rounding {
    Half,
    Up,
    Down,
}

impl Eval<'_> {
    fn call(&mut self, name: &str, args: &[Expr]) -> Result<Value, Unsupported> {
        // "Is this function modelled at all?" is asked first: the arity rule
        // right below is Excel's rule for a malformed call to a *known*
        // function, and must not fire for a function this evaluator has never
        // implemented. Refuse those by name regardless of how many arguments
        // they were given, rather than guessing #VALUE! for some arities and
        // Unsupported for others.
        if !MODELLED.contains(&name) && !VOLATILE.contains(&name) {
            return Err(Unsupported::Function(name.to_string()));
        }

        // Excel refuses a call with too few arguments at entry, so a formula
        // out of a workbook always has enough. One that does not is malformed
        // rather than unmodelled, and #VALUE! is what it deserves.
        if args.is_empty() && !matches!(name, "TRUE" | "FALSE" | "NA") && !VOLATILE.contains(&name)
        {
            return Ok(err(ErrorKind::Value));
        }
        if args.len() < 2 && matches!(name, "EXACT" | "IFERROR" | "IFNA") {
            return Ok(err(ErrorKind::Value));
        }

        // Excel does not evaluate the branch it does not take, and neither do
        // we — a formula whose dead branch calls something unmodelled still has
        // a defensible answer.
        match name {
            "IF" => {
                let condition = self.scalar(&args[0])?;
                if let Some(e) = error_of(&condition) {
                    return Ok(err(e));
                }
                let taken = match to_bool(&condition) {
                    Ok(b) => b,
                    Err(e) => return Ok(err(e)),
                };
                let branch = if taken { args.get(1) } else { args.get(2) };
                return match branch {
                    Some(expr) => Ok(Value::Scalar(self.scalar(expr)?)),
                    None => Ok(boolean(false)),
                };
            }
            "IFERROR" | "IFNA" => {
                let value = self.scalar(&args[0])?;
                let caught = match (name, error_of(&value)) {
                    ("IFNA", Some(ErrorKind::NA)) => true,
                    ("IFNA", _) => false,
                    (_, some) => some.is_some(),
                };
                return if caught {
                    Ok(Value::Scalar(self.scalar(&args[1])?))
                } else {
                    Ok(Value::Scalar(value))
                };
            }
            _ => {}
        }

        if VOLATILE.contains(&name) {
            return Err(Unsupported::Volatile(name.to_string()));
        }

        match name {
            // -- aggregates over whatever the arguments contribute ----------
            "SUM" | "PRODUCT" | "AVERAGE" | "MIN" | "MAX" => {
                let numbers = match self.numbers(args)? {
                    Ok(n) => n,
                    Err(e) => return Ok(err(e)),
                };
                Ok(match name {
                    "SUM" => num(numbers.iter().sum()),
                    "PRODUCT" if numbers.is_empty() => num(0.0),
                    "PRODUCT" => num(numbers.iter().product()),
                    "AVERAGE" if numbers.is_empty() => err(ErrorKind::Div0),
                    "AVERAGE" => num(numbers.iter().sum::<f64>() / numbers.len() as f64),
                    // MIN and MAX of nothing are 0 in Excel, not an error.
                    _ if numbers.is_empty() => num(0.0),
                    "MIN" => num(numbers.iter().copied().fold(f64::INFINITY, f64::min)),
                    _ => num(numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
                })
            }
            "COUNT" | "COUNTA" | "COUNTBLANK" => {
                let mut counted = 0u64;
                let mut present = 0u64;
                let addressed = self.spread(args, |value, from_range| {
                    // `present` feeds only COUNTBLANK below, which — unlike
                    // COUNTA just beside it — treats a cached `""` (a formula
                    // that evaluated to the empty string) as blank, matching
                    // both Excel and `CellValue::is_empty()` elsewhere in
                    // this crate. Excel's COUNTA disagrees with COUNTBLANK
                    // here on purpose, so `counted`'s own COUNTA arm below is
                    // deliberately left as the raw `Empty` check.
                    if !value.is_empty() {
                        present += 1;
                    }
                    counted += u64::from(match name {
                        "COUNT" => match (value, from_range) {
                            (CellValue::Number(_), _) => true,
                            (_, true) => false,
                            (other, false) => to_number(other).is_ok(),
                        },
                        "COUNTA" => !matches!(value, CellValue::Empty),
                        _ => false,
                    });
                })?;
                // Unpopulated cells never reach the visitor: a blank is a cell
                // the arguments addressed and the sheet does not hold.
                Ok(num(if name == "COUNTBLANK" {
                    addressed.saturating_sub(present) as f64
                } else {
                    counted as f64
                }))
            }

            // -- logic ------------------------------------------------------
            "AND" | "OR" => {
                let mut result = name == "AND";
                let mut failed = None;
                let mut seen = false;
                self.spread(args, |value, from_range| {
                    if failed.is_some() {
                        return;
                    }
                    // A range contributes only its booleans and numbers; text
                    // and blanks in one are ignored rather than an error.
                    if from_range && matches!(value, CellValue::Text(_) | CellValue::Empty) {
                        return;
                    }
                    match to_bool(value) {
                        Err(e) => failed = Some(e),
                        Ok(b) => {
                            seen = true;
                            result = if name == "AND" {
                                result && b
                            } else {
                                result || b
                            };
                        }
                    }
                })?;
                Ok(match (failed, seen) {
                    (Some(e), _) => err(e),
                    (None, false) => err(ErrorKind::Value),
                    (None, true) => boolean(result),
                })
            }
            "NOT" => {
                let value = self.scalar(&args[0])?;
                Ok(match to_bool(&value) {
                    Ok(b) => boolean(!b),
                    Err(e) => err(e),
                })
            }
            "TRUE" => Ok(boolean(true)),
            "FALSE" => Ok(boolean(false)),
            "NA" => Ok(err(ErrorKind::NA)),

            // -- information ------------------------------------------------
            "ISBLANK" | "ISNUMBER" | "ISTEXT" | "ISNONTEXT" | "ISLOGICAL" | "ISERROR" | "ISERR"
            | "ISNA" => {
                let value = self.scalar(&args[0])?;
                let kind = value.kind();
                Ok(boolean(match name {
                    "ISBLANK" => matches!(value, CellValue::Empty),
                    "ISNUMBER" => kind == ValueKind::Number,
                    "ISTEXT" => kind == ValueKind::Text,
                    "ISNONTEXT" => kind != ValueKind::Text,
                    "ISLOGICAL" => kind == ValueKind::Bool,
                    "ISERROR" => kind == ValueKind::Error,
                    "ISERR" => kind == ValueKind::Error && error_of(&value) != Some(ErrorKind::NA),
                    _ => error_of(&value) == Some(ErrorKind::NA),
                }))
            }

            // -- arithmetic -------------------------------------------------
            "ABS" | "INT" | "SIGN" | "SQRT" | "EXP" | "LN" | "LOG10" => {
                let n = match self.number(&args[0])? {
                    Ok(n) => n,
                    Err(e) => return Ok(err(e)),
                };
                Ok(match name {
                    "ABS" => num(n.abs()),
                    "INT" => num(n.floor()),
                    "SIGN" if n == 0.0 => num(0.0),
                    "SIGN" => num(n.signum()),
                    "SQRT" if n < 0.0 => err(ErrorKind::Num),
                    "SQRT" => num(n.sqrt()),
                    "EXP" => Value::Scalar(number_or_num_error(n.exp())),
                    "LN" if n <= 0.0 => err(ErrorKind::Num),
                    "LN" => num(n.ln()),
                    "LOG10" if n <= 0.0 => err(ErrorKind::Num),
                    _ => num(n.log10()),
                })
            }
            "ROUND" | "ROUNDUP" | "ROUNDDOWN" | "TRUNC" => {
                let n = match self.number(&args[0])? {
                    Ok(n) => n,
                    Err(e) => return Ok(err(e)),
                };
                let digits = match args.get(1) {
                    Some(expr) => match self.number(expr)? {
                        Ok(d) => d.trunc() as i32,
                        Err(e) => return Ok(err(e)),
                    },
                    None => 0,
                };
                let mode = match name {
                    "ROUND" => Rounding::Half,
                    "ROUNDUP" => Rounding::Up,
                    _ => Rounding::Down,
                };
                Ok(num(round_to(n, digits, mode)))
            }
            "MOD" | "POWER" | "LOG" => {
                let a = match self.number(&args[0])? {
                    Ok(n) => n,
                    Err(e) => return Ok(err(e)),
                };
                let b = match args.get(1) {
                    Some(expr) => match self.number(expr)? {
                        Ok(n) => n,
                        Err(e) => return Ok(err(e)),
                    },
                    None if name == "LOG" => 10.0,
                    None => return Ok(err(ErrorKind::Value)),
                };
                Ok(match name {
                    // Excel's MOD takes the sign of the divisor, unlike Rust's %.
                    "MOD" if b == 0.0 => err(ErrorKind::Div0),
                    "MOD" => num(a - b * (a / b).floor()),
                    "POWER" => Value::Scalar(number_or_num_error(a.powf(b))),
                    _ if a <= 0.0 || b <= 0.0 || b == 1.0 => err(ErrorKind::Num),
                    _ => num(a.log(b)),
                })
            }

            // -- financial --------------------------------------------------
            "PV" => self.present_value(args),

            // -- text -------------------------------------------------------
            "LEN" | "TRIM" | "UPPER" | "LOWER" => {
                let value = self.scalar(&args[0])?;
                let s = match to_text(&value) {
                    Ok(s) => s,
                    Err(e) => return Ok(err(e)),
                };
                Ok(match name {
                    "LEN" => num(s.chars().count() as f64),
                    // Excel's TRIM also collapses runs of interior spaces.
                    "TRIM" => text(s.split_whitespace().collect::<Vec<_>>().join(" ")),
                    "UPPER" => text(s.to_uppercase()),
                    _ => text(s.to_lowercase()),
                })
            }
            "LEFT" | "RIGHT" | "MID" => {
                let value = self.scalar(&args[0])?;
                let s = match to_text(&value) {
                    Ok(s) => s,
                    Err(e) => return Ok(err(e)),
                };
                let chars: Vec<char> = s.chars().collect();
                let arg_number = |eval: &mut Self,
                                  index: usize,
                                  default: f64|
                 -> Result<Result<f64, ErrorKind>, Unsupported> {
                    match args.get(index) {
                        Some(expr) => eval.number(expr),
                        None => Ok(Ok(default)),
                    }
                };
                if name == "MID" {
                    let start = match arg_number(self, 1, 1.0)? {
                        Ok(n) => n.trunc(),
                        Err(e) => return Ok(err(e)),
                    };
                    let count = match arg_number(self, 2, 0.0)? {
                        Ok(n) => n.trunc(),
                        Err(e) => return Ok(err(e)),
                    };
                    if start < 1.0 || count < 0.0 {
                        return Ok(err(ErrorKind::Value));
                    }
                    let from = (start as usize - 1).min(chars.len());
                    let to = from.saturating_add(count as usize).min(chars.len());
                    return Ok(text(chars[from..to].iter().collect::<String>()));
                }
                let count = match arg_number(self, 1, 1.0)? {
                    Ok(n) => n.trunc(),
                    Err(e) => return Ok(err(e)),
                };
                if count < 0.0 {
                    return Ok(err(ErrorKind::Value));
                }
                let count = (count as usize).min(chars.len());
                Ok(text(if name == "LEFT" {
                    chars[..count].iter().collect::<String>()
                } else {
                    chars[chars.len() - count..].iter().collect::<String>()
                }))
            }
            "CONCATENATE" | "CONCAT" => {
                let mut out = String::new();
                let mut failed = None;
                self.spread(args, |value, _| {
                    if failed.is_some() {
                        return;
                    }
                    match to_text(value) {
                        Ok(s) => out.push_str(&s),
                        Err(e) => failed = Some(e),
                    }
                })?;
                Ok(match failed {
                    Some(e) => err(e),
                    None => text(out),
                })
            }
            "EXACT" => {
                let a = self.scalar(&args[0])?;
                let b = self.scalar(&args[1])?;
                Ok(match (to_text(&a), to_text(&b)) {
                    (Ok(a), Ok(b)) => boolean(a == b),
                    (Err(e), _) | (_, Err(e)) => err(e),
                })
            }

            // -- lookup -----------------------------------------------------
            "VLOOKUP" | "HLOOKUP" => self.lookup(name == "VLOOKUP", args),
            "MATCH" => self.match_(args),
            "INDEX" => self.index(args),

            _ => Err(Unsupported::Function(name.to_string())),
        }
    }

    /// One numeric argument, with an Excel error kept as an answer rather than
    /// a failure.
    fn number(&mut self, expr: &Expr) -> Result<Result<f64, ErrorKind>, Unsupported> {
        let value = self.scalar(expr)?;
        Ok(to_number(&value))
    }

    /// An optional numeric argument: absent, or written as an empty slot in
    /// `PV(r,n,0,,0)`, takes the default rather than reading a cell.
    fn number_or(
        &mut self,
        arg: Option<&Expr>,
        default: f64,
    ) -> Result<Result<f64, ErrorKind>, Unsupported> {
        match arg {
            None | Some(Expr::Literal(CellValue::Empty)) => Ok(Ok(default)),
            Some(expr) => self.number(expr),
        }
    }

    /// `PV(rate, nper, pmt, [fv], [type])` — what a future stream is worth now.
    ///
    /// Modelled ahead of the rest of the financial family because it is the
    /// single largest gap in the sweep: 115,566 of the reference workbook's
    /// unsupported formulas are this one function, discounting each overdue
    /// amount by the days it has been outstanding.
    ///
    /// Excel's identity, and its sign convention with it — money paid out is
    /// negative, which is why the whole expression is negated:
    ///
    /// ```text
    /// rate ≠ 0:  -(fv + pmt · (1 + rate·type) · ((1+rate)^nper − 1) / rate) / (1+rate)^nper
    /// rate = 0:  -(fv + pmt · nper)
    /// ```
    ///
    /// `type` says whether each payment falls at the end of its period (0) or
    /// the start (1); Excel documents anything else as `#NUM!` rather than
    /// rounding it to one, so that is what this returns.
    fn present_value(&mut self, args: &[Expr]) -> Result<Value, Unsupported> {
        if args.len() < 3 {
            return Ok(err(ErrorKind::Value));
        }
        let rate = match self.number(&args[0])? {
            Ok(n) => n,
            Err(e) => return Ok(err(e)),
        };
        let nper = match self.number(&args[1])? {
            Ok(n) => n,
            Err(e) => return Ok(err(e)),
        };
        let pmt = match self.number(&args[2])? {
            Ok(n) => n,
            Err(e) => return Ok(err(e)),
        };
        let future = match self.number_or(args.get(3), 0.0)? {
            Ok(n) => n,
            Err(e) => return Ok(err(e)),
        };
        let due = match self.number_or(args.get(4), 0.0)? {
            Ok(n) => n,
            Err(e) => return Ok(err(e)),
        };
        if due != 0.0 && due != 1.0 {
            return Ok(err(ErrorKind::Num));
        }
        let present = if rate == 0.0 {
            -(future + pmt * nper)
        } else {
            // A rate of exactly -1 sends this to zero and the division to an
            // infinity, which `number_or_num_error` turns into #NUM! — the
            // same answer Excel gives, arrived at rather than special-cased.
            let growth = (1.0 + rate).powf(nper);
            -(future + pmt * (1.0 + rate * due) * (growth - 1.0) / rate) / growth
        };
        Ok(Value::Scalar(number_or_num_error(present)))
    }

    /// VLOOKUP and HLOOKUP, which differ only in which axis they walk.
    fn lookup(&mut self, vertical: bool, args: &[Expr]) -> Result<Value, Unsupported> {
        if args.len() < 3 {
            return Ok(err(ErrorKind::Value));
        }
        let key = self.scalar(&args[0])?;
        if let Some(e) = error_of(&key) {
            return Ok(err(e));
        }
        let table = match self.range_arg(&args[1])? {
            Ok(r) => r,
            Err(e) => return Ok(err(e)),
        };
        let index = match self.number(&args[2])? {
            Ok(n) => n.trunc(),
            Err(e) => return Ok(err(e)),
        };
        let approximate = match args.get(3) {
            // Omitted entirely: Excel's own default, TRUE.
            None => true,
            // Written but empty (`VLOOKUP(x,tbl,2,)`) coerces like any other
            // empty argument — 0, i.e. FALSE — which is not the same as
            // leaving it out.
            Some(Expr::Literal(CellValue::Empty)) => false,
            Some(expr) => {
                let value = self.scalar(expr)?;
                match to_bool(&value) {
                    Ok(b) => b,
                    Err(e) => return Ok(err(e)),
                }
            }
        };
        if index < 1.0 {
            return Ok(err(ErrorKind::Value));
        }
        let index = index as u64 - 1;
        let span = if vertical {
            u64::from(table.right - table.left)
        } else {
            u64::from(table.bottom - table.top)
        };
        if index > span {
            return Ok(err(ErrorKind::Ref));
        }

        let last = self.last_row(table);
        // The index is an exact-match lookup by value, not a glob — a
        // wildcard key has to fall through to the scan below, whatever the
        // table's size.
        if vertical
            && !approximate
            && !key_has_wildcard(&key)
            && last - table.top >= INDEX_LOOKUPS_OVER
        {
            let column = RangeRef::new(table.sheet, table.top, table.left, last, table.left);
            let found = self.index.find(self.workbook, self.overrides, column, &key);
            return Ok(match found {
                Some(row) => Value::Scalar(self.cell_at(table, row, index as u16)),
                None => err(ErrorKind::NA),
            });
        }

        let steps = if vertical {
            u64::from(last - table.top)
        } else {
            u64::from(table.right - table.left)
        };
        let mut best: Option<u64> = None;
        for step in 0..=steps {
            let probe = if vertical {
                self.cell_at(table, step as u32, 0)
            } else {
                self.cell_at(table, 0, step as u16)
            };
            if !approximate {
                // Excel matches a text key holding `*`/`?` as a wildcard in
                // exact mode, the same as it does in MATCH and the *IF
                // functions — a comparison ordinary equality cannot express,
                // so this is checked apart from `compare_for_lookup` below.
                if exact_match_for_lookup(&probe, &key) {
                    best = Some(step);
                    break;
                }
                continue;
            }
            // An approximate lookup wants the last row at or before the key,
            // not the first row that matches it — a sorted column with
            // duplicate keys (10, 10, 10, 20) is exactly where those differ.
            // Excel's own binary search lands on the last of a run of equal
            // keys, so an equal match here keeps scanning rather than
            // stopping, the same as a lesser one.
            if let Some(std::cmp::Ordering::Equal | std::cmp::Ordering::Less) =
                compare_for_lookup(&probe, &key)
            {
                best = Some(step);
            }
        }
        let Some(step) = best else {
            return Ok(err(ErrorKind::NA));
        };
        Ok(Value::Scalar(if vertical {
            self.cell_at(table, step as u32, index as u16)
        } else {
            self.cell_at(table, index as u32, step as u16)
        }))
    }

    fn match_(&mut self, args: &[Expr]) -> Result<Value, Unsupported> {
        if args.len() < 2 {
            return Ok(err(ErrorKind::Value));
        }
        let key = self.scalar(&args[0])?;
        if let Some(e) = error_of(&key) {
            return Ok(err(e));
        }
        let vector = match self.range_arg(&args[1])? {
            Ok(r) => r,
            Err(e) => return Ok(err(e)),
        };
        let mode = match args.get(2) {
            // Omitted entirely: Excel's own default, 1 (approximate).
            None => 1.0,
            // Written but empty (`MATCH(k,r,)`) coerces to 0 like any other
            // empty argument — exact — not the same as leaving it out.
            Some(Expr::Literal(CellValue::Empty)) => 0.0,
            Some(expr) => match self.number(expr)? {
                Ok(n) => n.trunc(),
                Err(e) => return Ok(err(e)),
            },
        };
        let vertical = vector.right == vector.left;
        let last = self.last_row(vector);
        if vertical
            && mode == 0.0
            && !key_has_wildcard(&key)
            && last - vector.top >= INDEX_LOOKUPS_OVER
        {
            let column = RangeRef::new(vector.sheet, vector.top, vector.left, last, vector.left);
            let found = self.index.find(self.workbook, self.overrides, column, &key);
            return Ok(match found {
                Some(row) => num(row as f64 + 1.0),
                None => err(ErrorKind::NA),
            });
        }

        let steps = if vertical {
            u64::from(last - vector.top)
        } else {
            u64::from(vector.right - vector.left)
        };
        let mut best: Option<u64> = None;
        for step in 0..=steps {
            let probe = if vertical {
                self.cell_at(vector, step as u32, 0)
            } else {
                self.cell_at(vector, 0, step as u16)
            };
            if mode == 0.0 {
                // See VLOOKUP's exact branch: a text key holding `*`/`?` is
                // a wildcard in exact mode, the same as Excel treats one.
                if exact_match_for_lookup(&probe, &key) {
                    best = Some(step);
                    break;
                }
                continue;
            }
            let Some(ord) = compare_for_lookup(&probe, &key) else {
                continue;
            };
            match (mode as i32, ord) {
                // Mode 1 and -1 assume a sorted list and want the *last* row
                // that still qualifies, so a run of duplicate keys keeps
                // scanning past an equal match instead of stopping at the
                // first of them.
                (1, std::cmp::Ordering::Equal | std::cmp::Ordering::Less) => best = Some(step),
                (-1, std::cmp::Ordering::Equal | std::cmp::Ordering::Greater) => {
                    best = Some(step);
                }
                _ => {}
            }
        }
        Ok(match best {
            Some(step) => num(step as f64 + 1.0),
            None => err(ErrorKind::NA),
        })
    }

    fn index(&mut self, args: &[Expr]) -> Result<Value, Unsupported> {
        if args.len() < 2 {
            return Ok(err(ErrorKind::Value));
        }
        let range = match self.range_arg(&args[0])? {
            Ok(r) => r,
            Err(e) => return Ok(err(e)),
        };
        let row = match self.number(&args[1])? {
            Ok(n) => n.trunc(),
            Err(e) => return Ok(err(e)),
        };
        let column = match args.get(2) {
            // Omitted entirely: a plain column index of 1 (unless the
            // one-row-range special case just below overrides it).
            None => 1.0,
            // Written but empty (`INDEX(range,2,)`) coerces to 0 like any
            // other empty argument — and 0 is not a smaller column index
            // here, it is INDEX's own notation for "the whole row", the same
            // outcome an explicit 0 gives.
            Some(Expr::Literal(CellValue::Empty)) => 0.0,
            Some(expr) => match self.number(expr)? {
                Ok(n) => n.trunc(),
                Err(e) => return Ok(err(e)),
            },
        };
        // A one-row range indexed with a single argument is indexed by column.
        let (row, column) = if args.len() < 3 && range.top == range.bottom {
            (1.0, row)
        } else {
            (row, column)
        };
        if row < 0.0 || column < 0.0 {
            return Ok(err(ErrorKind::Value));
        }
        if row == 0.0 || column == 0.0 {
            return Err(Unsupported::Form(
                "INDEX over a whole row or column".to_string(),
            ));
        }
        Ok(Value::Scalar(self.cell_at(
            range,
            row as u32 - 1,
            column as u16 - 1,
        )))
    }

    /// The last row of a range worth probing: past the sheet's used range every
    /// cell is empty, and a lookup table declared as a million rows is common.
    fn last_row(&self, range: RangeRef) -> u32 {
        let used = self
            .workbook
            .sheet(range.sheet)
            .and_then(|s| s.used_range())
            .map(|u| u.bottom)
            .unwrap_or(range.top);
        range.bottom.min(used.max(range.top))
    }
}

/// Compare a probed cell with a lookup key, or `None` when the two are not
/// comparable — a lookup skips values of another type rather than ordering
/// text against numbers.
fn compare_for_lookup(probe: &CellValue, key: &CellValue) -> Option<std::cmp::Ordering> {
    if matches!(probe, CellValue::Empty) || probe.kind() != key.kind() {
        return None;
    }
    Some(compare(probe, key))
}

/// Whether `probe` satisfies an exact lookup for `key` — ordinary equality,
/// except a text key holding `*`/`?` is a wildcard, the way Excel treats one
/// in `VLOOKUP`/`HLOOKUP`/`MATCH`'s exact mode. Approximate lookups never
/// reach this: a wildcard has no order, so it only makes sense once "equal"
/// is the whole question.
fn exact_match_for_lookup(probe: &CellValue, key: &CellValue) -> bool {
    if let (CellValue::Text(p), CellValue::Text(k)) = (probe, key) {
        // Always through the wildcard matcher, not just when a `*`/`?` is
        // active: a pattern that is *only* an escape (`A~*B`, meaning the
        // literal text `A*B`) has no active wildcard by that test, but its
        // `~` still has to be stripped before comparing — a raw string
        // compare would see the tilde and never match the plain text it
        // stands for. A pattern with none of `*`, `?`, `~` degenerates to
        // ordinary literal comparison here anyway.
        return wildcard_match(k, p);
    }
    compare_for_lookup(probe, key) == Some(std::cmp::Ordering::Equal)
}

/// Whether `key` needs [`wildcard_match`] rather than a value index's exact
/// hash lookup — any of `*`, `?` or `~`, escaped or not, since even a
/// pattern that is only an escape sequence still needs de-escaping before
/// it can be compared as plain text.
fn key_has_wildcard(key: &CellValue) -> bool {
    matches!(key, CellValue::Text(s) if s.contains(['*', '?', '~']))
}

/// One token of a parsed wildcard pattern: `*` matches any run of characters
/// (including none), `?` matches exactly one, and anything else — including
/// `*`, `?` or `~` written after a `~` — matches itself literally.
enum WildcardToken {
    Any,
    One,
    Literal(char),
}

fn parse_wildcard_pattern(pattern: &str) -> Vec<WildcardToken> {
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '~' => WildcardToken::Literal(chars.next().unwrap_or('~')),
            '*' => WildcardToken::Any,
            '?' => WildcardToken::One,
            other => WildcardToken::Literal(other),
        });
    }
    tokens
}

/// Excel's wildcard match, case-insensitive like every other text comparison
/// this crate makes. Classic DP over pattern tokens and text characters:
/// `matched[i][j]` is whether the first `i` tokens of the pattern account for
/// the first `j` characters of the text.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern = parse_wildcard_pattern(&pattern.to_uppercase());
    let text: Vec<char> = text.to_uppercase().chars().collect();
    let (m, n) = (pattern.len(), text.len());

    let mut matched = vec![vec![false; n + 1]; m + 1];
    matched[0][0] = true;
    for i in 1..=m {
        if let WildcardToken::Any = pattern[i - 1] {
            matched[i][0] = matched[i - 1][0];
        }
    }
    for i in 1..=m {
        for j in 1..=n {
            matched[i][j] = match pattern[i - 1] {
                WildcardToken::Any => matched[i - 1][j] || matched[i][j - 1],
                WildcardToken::One => matched[i - 1][j - 1],
                WildcardToken::Literal(c) => c == text[j - 1] && matched[i - 1][j - 1],
            };
        }
    }
    matched[m][n]
}
