//! Change a cell, and find out what else moves.
//!
//! [`recompute`](crate::recompute) asks one formula whether it still agrees
//! with the number beside it, deliberately reading its precedents as stored
//! values so that a disagreement is about that formula alone. A what-if is the
//! opposite question, and it needs the opposite of that limit: substitute a
//! value, then follow it as far as it goes.
//!
//! Three things follow, and each is a cost the one-formula recompute avoided:
//!
//! - **Finding what is downstream.** Nothing in a workbook records who reads a
//!   cell, so the closure is found by scanning every formula, once per level of
//!   the chain. Two levels on the reference workbook is two passes over 6.79
//!   million formulas.
//! - **Order.** A cell must not be computed before the cells it reads, so the
//!   affected set is sorted by its own dependencies. A level is a *shortest*
//!   distance from the change, not a topological rank, so a cell reached early
//!   can read one that only moves later: that cell is judged again when its
//!   input moves, and holds whatever the last visit said. What cannot be
//!   ordered is a cycle, and a cycle is reported rather than iterated to a
//!   fixed point — Excel's iterative calculation is a setting this cannot see.
//! - **Honesty about what it could not do.** A cell whose formula this crate
//!   does not model has no new value, and neither does anything downstream of
//!   it. Those are [`Blocked`], not "unchanged": quietly leaving the stored
//!   value in place would report a smaller impact than the change really has.
//!
//! The workbook is never modified. It cannot be — XLSB is read-only here, since
//! no Rust crate can write it — so substitutions live in an [`Overrides`]
//! overlay that every read goes through, and the answer is a report rather than
//! a file.
//!
//! ```no_run
//! # use eg_eval::whatif::{what_if, Change, WhatIfOptions};
//! # use eg_model::{CellRef, CellValue, SheetId};
//! let loaded = eg_ingest::load("book.xlsx")?;
//! let change = Change::new(CellRef::new(SheetId(1), 3, 1), CellValue::Number(0.15));
//! let impact = what_if(&loaded.workbook, &[change], &WhatIfOptions::default());
//! println!("{} cells downstream, {} of them moved", impact.report.affected, impact.report.moved);
//! # Ok::<(), eg_ingest::IngestError>(())
//! ```

use eg_model::formula::scan_references_into;
use eg_model::{CellRef, CellValue, RangeRef, ReferenceSpan, SheetId, Workbook};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use crate::calc::{recompute_over, same, Outcome, Overrides, Unsupported};
use crate::trace::{overlaps, resolve, sheet_ids};

/// One substitution: what to put in a cell, in place of what it holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub cell: CellRef,
    pub value: CellValue,
}

impl Change {
    pub fn new(cell: CellRef, value: CellValue) -> Self {
        Self { cell, value }
    }
}

/// A substitution, with what the cell held before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub cell: CellRef,
    pub a1: String,
    pub before: CellValue,
    pub after: CellValue,
    /// The formula the substitution displaces, if the cell had one. Typing a
    /// value over a formula is what Excel does too, and it is worth saying.
    pub replaced_formula: Option<String>,
}

/// How far to follow a change, and how much to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhatIfOptions {
    /// Levels of the dependency chain to follow. Each one is a full scan of
    /// the workbook's formulas, so this is the knob that costs seconds.
    pub max_levels: usize,
    /// Ceiling on the affected set. Reaching it stops the walk and sets
    /// [`ImpactReport::capped`]; the counts stay exact for what was walked.
    pub max_cells: usize,
    /// How many moved cells to return. The counts are exact regardless.
    pub limit: usize,
}

impl Default for WhatIfOptions {
    fn default() -> Self {
        Self {
            max_levels: 8,
            max_cells: 500_000,
            limit: 50,
        }
    }
}

/// One cell whose value the change moved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Moved {
    pub cell: CellRef,
    pub a1: String,
    pub formula: String,
    /// The value the workbook stores — what Excel last calculated.
    pub before: CellValue,
    /// The value this formula computes once the change is in.
    pub after: CellValue,
    /// How many levels of formula sit between the change and this cell.
    pub level: usize,
    /// Whether this cell already disagreed with its stored value before the
    /// change. If it did, `before` is Excel's number and not this evaluator's,
    /// and the difference is not all down to the change.
    pub was_stale: bool,
}

/// Why a cell downstream of the change has no new value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Blocked {
    /// This crate does not model the formula.
    Formula(Unsupported),
    /// A cell it reads is itself blocked, so this one cannot be answered
    /// either — named, so the chain can be followed back to its cause.
    Upstream(String),
    /// The cell reads, directly or through others, a cell that reads it.
    /// Excel resolves these by iterating; this refuses, because how many
    /// iterations and to what tolerance is a workbook setting, not a fact.
    Cycle,
}

impl Blocked {
    /// A short key for grouping, so a report says *what* stopped it rather
    /// than one line per cell.
    pub fn key(&self) -> String {
        match self {
            Blocked::Formula(reason) => reason.key(),
            Blocked::Upstream(_) => "reads a blocked cell".to_string(),
            Blocked::Cycle => "circular reference".to_string(),
        }
    }
}

/// Which limit ended the walk before the change ran out of cells to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stopped {
    /// [`WhatIfOptions::max_levels`]: there are cells further down the chain.
    Levels,
    /// [`WhatIfOptions::max_cells`]: the change reaches more of the workbook
    /// than the caller allowed for.
    Cells,
}

impl Stopped {
    pub fn as_str(&self) -> &'static str {
        match self {
            Stopped::Levels => "the level limit",
            Stopped::Cells => "the ceiling on affected cells",
        }
    }
}

/// A cell that could not be given a new value, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unanswered {
    pub cell: CellRef,
    pub a1: String,
    pub formula: String,
    pub reason: Blocked,
}

/// What the walk did, exactly, so a small answer can be told from a capped one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactReport {
    /// Formula cells that read the change, directly or through others.
    pub affected: u64,
    /// Of those, the ones whose value the change moved.
    pub moved: u64,
    /// Of those, the ones that recompute to what they already held. A cell that
    /// does not move cannot move anything downstream of it, which is why the
    /// walk stops at it.
    pub unchanged: u64,
    /// Of those, the ones with no answer. See [`Blocked`].
    pub blocked: u64,
    /// Reasons for the blocked ones, commonest first.
    pub blocked_reasons: Vec<(String, u64)>,
    /// Levels of the chain walked.
    pub levels: usize,
    /// Full passes over the workbook's formulas, which is what this costs.
    pub scans: usize,
    pub formulas_scanned: u64,
    /// Which limit stopped the walk short of the whole closure, if one did.
    /// The counts are exact for what was walked either way.
    pub stopped: Option<Stopped>,
    /// Cells that moved but are not in the returned list.
    pub moved_not_listed: u64,
}

/// The answer to "what if this cell held that instead".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Impact {
    /// The substitutions, with what they displaced.
    pub changes: Vec<Applied>,
    /// The cells that moved, biggest level first within the order found, capped
    /// at [`WhatIfOptions::limit`].
    pub moved: Vec<Moved>,
    /// The cells with no answer, capped at the same limit. Listed before the
    /// counts are read, because these are what a caller must not mistake for
    /// "nothing happened".
    pub unanswered: Vec<Unanswered>,
    pub report: ImpactReport,
}

/// Substitute values into cells and report everything that moves.
///
/// The changed cells themselves are not recomputed: a substituted value stands,
/// formula or not, exactly as it would if it had been typed in.
pub fn what_if(workbook: &Workbook, changes: &[Change], opts: &WhatIfOptions) -> Impact {
    let mut overrides = Overrides::new();
    let mut applied = Vec::new();
    for change in changes {
        let cell = workbook
            .sheet(change.cell.sheet)
            .and_then(|s| s.get_ref(change.cell));
        applied.push(Applied {
            cell: change.cell,
            a1: workbook.cite(change.cell),
            before: cell.map(|c| c.value.clone()).unwrap_or(CellValue::Empty),
            after: change.value.clone(),
            replaced_formula: cell.and_then(|c| c.formula.clone()),
        });
        overrides.set(change.cell, change.value.clone());
    }

    let mut report = ImpactReport::default();
    let mut impact = Impact {
        changes: applied,
        moved: Vec::new(),
        unanswered: Vec::new(),
        report: ImpactReport::default(),
    };
    if changes.is_empty() {
        return impact;
    }

    let sheets = sheet_ids(workbook);
    // What the next scan is looking for: cells whose value has just moved.
    // A cell that recomputed to what it already held is dropped here, because
    // nothing reading it can move either.
    let mut frontier = CellSet::default();
    for change in changes {
        let before = workbook
            .sheet(change.cell.sheet)
            .map(|s| s.value(change.cell.row, change.cell.col))
            .unwrap_or(CellValue::Empty);
        if !same(&change.value, &before) {
            frontier.insert(change.cell);
        }
    }
    frontier.seal();
    // A substituted cell is never recomputed: the value typed into it stands,
    // whatever reads it.
    let mut substituted: FxHashSet<CellRef> = FxHashSet::default();
    for change in changes {
        substituted.insert(change.cell);
    }
    // The verdict each cell currently holds. A cell is *not* retired once it
    // has one: the level a cell is first reached at is its shortest distance
    // from the change, and a cell can read something further down a longer
    // path — `D=A+C` where `C=B` and `B=A` is reached at level 1 and reads a
    // cell that only moves at level 2. So a cell is revisited whenever an input
    // of its moves, and its verdict is whatever the last visit said.
    let mut status: FxHashMap<CellRef, Status> = FxHashMap::default();
    // Where a moved cell sits in the returned list, so a later visit corrects
    // the entry rather than adding a second one for the same cell.
    let mut listed: FxHashMap<CellRef, usize> = FxHashMap::default();
    // Cells with no answer, so a dependent of one can be told it has none
    // either rather than being handed a stale value.
    let mut blocked: FxHashMap<CellRef, String> = FxHashMap::default();
    let mut reasons: FxHashMap<String, u64> = FxHashMap::default();

    for level in 1..=opts.max_levels {
        if frontier.is_empty() {
            break;
        }
        let found = readers_of(workbook, &sheets, &frontier, &substituted, &mut report);
        report.scans += 1;
        if found.is_empty() {
            break;
        }
        report.levels = level;

        // Within one level a cell may read another: both read the frontier, and
        // one also reads the other. Ordering by dependency inside the level is
        // what keeps a cell from being computed before its input.
        let ordered = order_within(workbook, &sheets, &found);
        // What the next scan looks for: cells whose value moved here, and cells
        // that lost their answer here. Both change what reads them.
        let mut moved_here = CellSet::default();

        for step in ordered {
            let (at, cyclic) = step;
            // A cell with no answer keeps none: nothing a later level can do
            // gives it one, and re-reporting it would count it twice.
            if matches!(status.get(&at), Some(Status::Blocked)) {
                continue;
            }
            let cell = match workbook.sheet(at.sheet).and_then(|s| s.get_ref(at)) {
                Some(c) => c,
                None => continue,
            };
            let formula = cell.formula.clone().unwrap_or_default();

            if cyclic {
                status.insert(at, Status::Blocked);
                moved_here.insert(at);
                record_blocked(
                    &mut impact,
                    &mut reasons,
                    &mut blocked,
                    opts,
                    at,
                    workbook.cite(at),
                    formula,
                    Blocked::Cycle,
                );
                continue;
            }
            // A cell reading one this could not answer has no answer either.
            if let Some(cause) = blocking_input(&sheets, at, &formula, &blocked) {
                status.insert(at, Status::Blocked);
                moved_here.insert(at);
                record_blocked(
                    &mut impact,
                    &mut reasons,
                    &mut blocked,
                    opts,
                    at,
                    workbook.cite(at),
                    formula,
                    Blocked::Upstream(cause),
                );
                continue;
            }

            let Some(after) = recompute_over(workbook, at, &overrides) else {
                continue;
            };
            match after.outcome {
                Outcome::Agrees(value) => {
                    // A cell that had moved and now recomputes to what the
                    // workbook stores has stopped moving; the override goes
                    // back to the stored value so nothing downstream reads the
                    // one it had in between.
                    overrides.set(at, value);
                    status.insert(at, Status::Unchanged);
                }
                Outcome::Differs { computed, stored } => {
                    overrides.set(at, computed.clone());
                    moved_here.insert(at);
                    status.insert(at, Status::Moved);
                    match listed.get(&at) {
                        // Reached again down a longer path: the same cell, a
                        // later value.
                        Some(&i) => {
                            impact.moved[i].after = computed;
                            impact.moved[i].level = level;
                        }
                        None if impact.moved.len() < opts.limit => {
                            // Whether this cell's own arithmetic already
                            // disagreed with the workbook is a different fact
                            // from whether the change moved it, and mixing the
                            // two would report a movement that was there before
                            // the question.
                            let was_stale = crate::calc::recompute(workbook, at)
                                .map(|r| !r.outcome.agrees())
                                .unwrap_or(false);
                            listed.insert(at, impact.moved.len());
                            impact.moved.push(Moved {
                                cell: at,
                                a1: after.a1,
                                formula: after.formula,
                                before: stored,
                                after: computed,
                                level,
                                was_stale,
                            });
                        }
                        None => {}
                    }
                }
                Outcome::Unsupported(reason) => {
                    status.insert(at, Status::Blocked);
                    moved_here.insert(at);
                    record_blocked(
                        &mut impact,
                        &mut reasons,
                        &mut blocked,
                        opts,
                        at,
                        after.a1,
                        after.formula,
                        Blocked::Formula(reason),
                    );
                }
            }
        }

        if status.len() >= opts.max_cells {
            report.stopped = Some(Stopped::Cells);
            break;
        }
        moved_here.seal();
        frontier = moved_here;
    }

    // Ending on the last allowed level with somewhere still to go is a
    // stopped walk, and saying so is the difference between "nothing else
    // moves" and "this did not look".
    if report.stopped.is_none() && report.levels == opts.max_levels && !frontier.is_empty() {
        report.stopped = Some(Stopped::Levels);
    }

    // The counts are per cell and not per visit: a cell reached twice is one
    // affected cell holding whatever verdict its last visit gave it.
    report.affected = status.len() as u64;
    for verdict in status.values() {
        match verdict {
            Status::Moved => report.moved += 1,
            Status::Unchanged => report.unchanged += 1,
            Status::Blocked => report.blocked += 1,
        }
    }
    // A cell listed as moved that a later visit settled is no longer one.
    impact
        .moved
        .retain(|m| matches!(status.get(&m.cell), Some(Status::Moved)));
    report.moved_not_listed = report.moved - impact.moved.len() as u64;

    let mut blocked_reasons: Vec<(String, u64)> = reasons.into_iter().collect();
    blocked_reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    report.blocked_reasons = blocked_reasons;
    impact.report = report;
    impact
}

/// The verdict a cell currently holds. Not final until the walk ends: a cell
/// reached again down a longer path is judged again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Moved,
    Unchanged,
    Blocked,
}

#[allow(clippy::too_many_arguments)]
fn record_blocked(
    impact: &mut Impact,
    reasons: &mut FxHashMap<String, u64>,
    blocked: &mut FxHashMap<CellRef, String>,
    opts: &WhatIfOptions,
    at: CellRef,
    a1: String,
    formula: String,
    reason: Blocked,
) {
    *reasons.entry(reason.key()).or_default() += 1;
    blocked.insert(at, a1.clone());
    if impact.unanswered.len() < opts.limit {
        impact.unanswered.push(Unanswered {
            cell: at,
            a1,
            formula,
            reason,
        });
    }
}

/// Whether this cell reads one that has no answer, and which.
fn blocking_input(
    sheets: &FxHashMap<String, SheetId>,
    at: CellRef,
    formula: &str,
    blocked: &FxHashMap<CellRef, String>,
) -> Option<String> {
    if blocked.is_empty() {
        return None;
    }
    let mut spans = Vec::new();
    scan_references_into(formula, &mut spans);
    for span in &spans {
        if let crate::trace::Target::Cells(range) = resolve(at, span, formula, sheets).target {
            // The overwhelmingly common reference is one cell, and answering
            // that by hash keeps a workbook with many blocked cells from
            // costing every one of them per reference of every formula.
            if range.cell_count() == 1 {
                if let Some(a1) = blocked.get(&range.top_left()) {
                    return Some(a1.clone());
                }
                continue;
            }
            for (cell, a1) in blocked {
                if overlaps(RangeRef::single(*cell), range) {
                    return Some(a1.clone());
                }
            }
        }
    }
    None
}

/// The formula cells reading anything in `frontier`, excluding the substituted
/// ones, whose typed value stands. One pass over every formula in the workbook.
///
/// A cell already given a verdict is *not* excluded: it is here because an
/// input of its has just moved, which is exactly the reason to judge it again.
fn readers_of(
    workbook: &Workbook,
    sheets: &FxHashMap<String, SheetId>,
    frontier: &CellSet,
    substituted: &FxHashSet<CellRef>,
    report: &mut ImpactReport,
) -> Vec<CellRef> {
    let mut out = Vec::new();
    let mut spans: Vec<ReferenceSpan> = Vec::new();
    for sheet in &workbook.sheets {
        for (at, cell) in sheet.iter() {
            let Some(formula) = cell.formula.as_deref() else {
                continue;
            };
            report.formulas_scanned += 1;
            if substituted.contains(&at) {
                continue;
            }
            scan_references_into(formula, &mut spans);
            for span in &spans {
                if let crate::trace::Target::Cells(range) =
                    resolve(at, span, formula, sheets).target
                {
                    if frontier.hits(range) {
                        out.push(at);
                        break;
                    }
                }
            }
        }
    }
    out
}

/// Order one level's cells so that none is computed before a cell it reads,
/// and flag the ones that cannot be ordered at all.
///
/// Only edges *within the level* are ordered here. Everything from an earlier
/// level already has a value; a cell that reads one which moves at a *later*
/// level is not ordered but revisited, because a level is a shortest distance
/// from the change and not a topological rank.
fn order_within(
    workbook: &Workbook,
    sheets: &FxHashMap<String, SheetId>,
    cells: &[CellRef],
) -> Vec<(CellRef, bool)> {
    let mut members = CellSet::default();
    for at in cells {
        members.insert(*at);
    }
    members.seal();
    let mut waits_for: FxHashMap<CellRef, Vec<CellRef>> = FxHashMap::default();
    let mut spans: Vec<ReferenceSpan> = Vec::new();

    for &at in cells {
        let Some(formula) = workbook
            .sheet(at.sheet)
            .and_then(|s| s.get_ref(at))
            .and_then(|c| c.formula.as_deref())
        else {
            continue;
        };
        scan_references_into(formula, &mut spans);
        let mut inputs = Vec::new();
        for span in &spans {
            if let crate::trace::Target::Cells(range) = resolve(at, span, formula, sheets).target {
                // Only cells of this level can hold this one up, and the index
                // hands back exactly those: a reference over a million rows
                // costs the few of them that are in the level.
                members.for_each_in(range, |other| {
                    if other != at {
                        inputs.push(other);
                    }
                    true
                });
            }
        }
        if !inputs.is_empty() {
            inputs.sort_unstable_by_key(|c| (c.sheet.0, c.row, c.col));
            inputs.dedup();
            waits_for.insert(at, inputs);
        }
    }

    let mut done: FxHashSet<CellRef> = FxHashSet::default();
    let mut out: Vec<(CellRef, bool)> = Vec::with_capacity(cells.len());
    let mut pending: Vec<CellRef> = cells.to_vec();
    while !pending.is_empty() {
        let mut progressed = false;
        let mut still: Vec<CellRef> = Vec::new();
        for at in pending {
            let ready = waits_for
                .get(&at)
                .map(|inputs| inputs.iter().all(|i| done.contains(i)))
                .unwrap_or(true);
            if ready {
                done.insert(at);
                out.push((at, false));
                progressed = true;
            } else {
                still.push(at);
            }
        }
        if !progressed {
            // Nothing moved, so what is left holds itself up: a cycle, or a
            // chain hanging off one. Neither has a value this can defend.
            out.extend(still.into_iter().map(|at| (at, true)));
            break;
        }
        pending = still;
    }
    out
}

/// A set of cells that a range can be matched against.
///
/// The question is asked once per reference of every formula in the workbook —
/// tens of millions of times against a set that can hold hundreds of thousands
/// of cells — so neither side may be walked whole. Cells are held by column,
/// each column's rows sorted, and a reference is answered by a bounding-box
/// reject and then a binary search per column it spans. What that costs is the
/// overlap, not the product.
#[derive(Debug, Default)]
struct CellSet {
    sheets: FxHashMap<SheetId, SheetCells>,
    len: usize,
}

#[derive(Debug, Default)]
struct SheetCells {
    columns: FxHashMap<u16, Vec<u32>>,
    top: u32,
    left: u16,
    bottom: u32,
    right: u16,
}

impl CellSet {
    fn insert(&mut self, at: CellRef) {
        let entry = self.sheets.entry(at.sheet).or_insert(SheetCells {
            columns: FxHashMap::default(),
            top: at.row,
            left: at.col,
            bottom: at.row,
            right: at.col,
        });
        entry.columns.entry(at.col).or_default().push(at.row);
        entry.top = entry.top.min(at.row);
        entry.left = entry.left.min(at.col);
        entry.bottom = entry.bottom.max(at.row);
        entry.right = entry.right.max(at.col);
        self.len += 1;
    }

    /// Sort each column's rows, which is what makes a reference answerable by
    /// binary search. Every query below assumes this has been called.
    fn seal(&mut self) {
        self.len = 0;
        for sheet in self.sheets.values_mut() {
            for rows in sheet.columns.values_mut() {
                rows.sort_unstable();
                rows.dedup();
                self.len += rows.len();
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether `range` names any cell in the set.
    fn hits(&self, range: RangeRef) -> bool {
        let mut found = false;
        self.for_each_in(range, |_| {
            found = true;
            false
        });
        found
    }

    /// Every cell of the set inside `range`, until `f` returns false.
    fn for_each_in(&self, range: RangeRef, mut f: impl FnMut(CellRef) -> bool) {
        let Some(sheet) = self.sheets.get(&range.sheet) else {
            return;
        };
        let top = range.top.max(sheet.top);
        let bottom = range.bottom.min(sheet.bottom);
        let left = range.left.max(sheet.left);
        let right = range.right.min(sheet.right);
        if top > bottom || left > right {
            return;
        }
        // Walk whichever is narrower: the columns the reference spans, or the
        // columns the set actually holds. A whole-row reference spans 16,384 of
        // the first and a handful of the second.
        let spanned = u64::from(right - left) + 1;
        if spanned <= sheet.columns.len() as u64 {
            for col in left..=right {
                if let Some(rows) = sheet.columns.get(&col) {
                    if !visit_rows(rows, range.sheet, col, top, bottom, &mut f) {
                        return;
                    }
                }
            }
        } else {
            for (&col, rows) in &sheet.columns {
                if col < left || col > right {
                    continue;
                }
                if !visit_rows(rows, range.sheet, col, top, bottom, &mut f) {
                    return;
                }
            }
        }
    }
}

/// The rows of one column between `top` and `bottom`, found by binary search.
/// Returns false as soon as the visitor asks to stop.
fn visit_rows(
    rows: &[u32],
    sheet: SheetId,
    col: u16,
    top: u32,
    bottom: u32,
    f: &mut impl FnMut(CellRef) -> bool,
) -> bool {
    let start = rows.partition_point(|row| *row < top);
    for row in &rows[start..] {
        if *row > bottom {
            break;
        }
        if !f(CellRef::new(sheet, *row, col)) {
            return false;
        }
    }
    true
}
