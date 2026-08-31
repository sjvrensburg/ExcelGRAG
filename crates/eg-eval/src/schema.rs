//! The keys and tables a workbook declares in its own formulas.
//!
//! A spreadsheet has no schema and states one anyway. `VLOOKUP(C2,
//! $BR$4:$BS$12, 2, FALSE)` filled down a column is a declaration: *this
//! column's values are keys into that table, and the answer is its second
//! column.* The reference workbook writes that 115,566 times. Nothing had ever
//! read it.
//!
//! This does. Formulas are already grouped by R1C1 shape, so a filled-down
//! column of 115,004 lookups is one group with one representative formula —
//! which means recovering the schema costs parsing a few hundred formulas
//! rather than millions, and the count of cells behind each relation comes free
//! with the grouping.
//!
//! # What it recovers, and what it will not guess
//!
//! `VLOOKUP` and `HLOOKUP` with an explicit column index, and `INDEX(range,
//! MATCH(key, keys, 0))`, which is the same relation written by someone who
//! wanted it to survive a column being inserted. `XLOOKUP` is recognised where
//! its three plain arguments are present.
//!
//! Approximate lookups — `VLOOKUP` without a `FALSE` fourth argument — are
//! recorded and flagged, because they are a *banding*, not a key: the formula
//! is asking for the last row not past its argument, so the "key" column is a
//! set of thresholds and joining on equality would be wrong.
//!
//! A lookup whose table is on a sheet the workbook does not have, or in another
//! workbook, is counted and dropped. A relation this cannot read is left
//! unrecognised rather than approximated: a schema that guesses is worse than
//! one with holes in it, because a hole is visible.

use eg_model::{RangeRef, SheetId, Workbook};
use eg_structure::group_formulas;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::parse::{parse, Expr};
use crate::trace::sheet_ids;

/// How the relation was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LookupKind {
    Vlookup,
    Hlookup,
    /// `INDEX(…, MATCH(…, …, 0))` — the same relation, insert-proof.
    IndexMatch,
    Xlookup,
}

impl LookupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LookupKind::Vlookup => "VLOOKUP",
            LookupKind::Hlookup => "HLOOKUP",
            LookupKind::IndexMatch => "INDEX/MATCH",
            LookupKind::Xlookup => "XLOOKUP",
        }
    }
}

/// One relation the workbook states: a key column, a table, and what comes back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lookup {
    /// The cells whose formulas do the looking up.
    pub from: RangeRef,
    /// The column holding the key, where the key is a plain reference. `None`
    /// when the key is computed — `VLOOKUP(A2&B2, …)` names no single column.
    pub key: Option<RangeRef>,
    /// The table looked into.
    pub table: RangeRef,
    /// Which column of `table` is returned, 1-based as the formula writes it.
    /// `None` for a shape that names the returned range directly.
    pub column: Option<u32>,
    /// The cells of `table` the answer is taken from, where that is knowable.
    pub returns: Option<RangeRef>,
    pub kind: LookupKind,
    /// Formula cells behind this relation.
    pub cells: u64,
    /// Whether the lookup is approximate — a banding rather than a key.
    ///
    /// An approximate `VLOOKUP` asks for the last row not past its argument, so
    /// the first column is a set of thresholds. Joining on equality would be
    /// wrong, and the flag is how a caller knows not to.
    pub approximate: bool,
}

/// What a scan recovered, and what it could not.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    /// Merged and sorted, heaviest first: the relations most of the workbook
    /// rests on come first.
    pub lookups: Vec<Lookup>,
    /// Formula groups examined.
    pub groups: u64,
    /// Groups holding a lookup call.
    pub with_lookups: u64,
    /// Lookup calls whose shape this does not read — a computed table range, a
    /// column index that is itself a formula.
    pub unrecognised: u64,
    /// Lookups into a sheet the workbook does not have, or another workbook.
    pub unresolvable: u64,
}

impl Schema {
    pub fn is_empty(&self) -> bool {
        self.lookups.is_empty()
    }

    /// The relations that read as keys rather than bandings.
    pub fn keys(&self) -> impl Iterator<Item = &Lookup> {
        self.lookups.iter().filter(|l| !l.approximate)
    }
}

/// Read every lookup relation a workbook states.
///
/// One pass over the formula groups, which is a few hundred formulas on a
/// workbook of six million.
pub fn infer_schema(workbook: &Workbook) -> Schema {
    let sheets = sheet_ids(workbook);
    let mut schema = Schema::default();
    // Merged on everything but the count, so one relation written in twenty
    // blocks of a column is one relation carrying twenty blocks' worth of
    // cells.
    let mut merged: FxHashMap<Key, (Lookup, u64)> = FxHashMap::default();

    for sheet in &workbook.sheets {
        let (groups, _) = group_formulas(sheet);
        for group in groups {
            schema.groups += 1;
            let Ok(expr) = parse(&group.representative) else {
                continue;
            };
            let mut found = Vec::new();
            walk(&expr, &mut found);
            if found.is_empty() {
                continue;
            }
            schema.with_lookups += 1;
            for call in found {
                match read(&call, group.range, &sheets, sheet.id) {
                    Outcome::Found(mut lookup) => {
                        lookup.cells = group.cell_count;
                        let id = Key::of(&lookup);
                        merged
                            .entry(id)
                            .and_modify(|(held, cells)| {
                                *cells += group.cell_count;
                                // One relation written in two blocks — a column
                                // broken by a hand-edited row — covers both.
                                held.from = widen(held.from, lookup.from);
                                held.key = match (held.key, lookup.key) {
                                    (Some(a), Some(b)) => Some(widen(a, b)),
                                    (a, b) => a.or(b),
                                };
                            })
                            .or_insert((lookup, group.cell_count));
                    }
                    Outcome::Unresolvable => schema.unresolvable += 1,
                    Outcome::Unrecognised => schema.unrecognised += 1,
                }
            }
        }
    }

    let mut lookups: Vec<Lookup> = merged
        .into_values()
        .map(|(mut lookup, cells)| {
            lookup.cells = cells;
            lookup
        })
        .collect();
    // Heaviest first, then by position, so two runs over one workbook produce
    // the same schema. A hash map's order is not an order.
    lookups.sort_by(|a, b| {
        b.cells
            .cmp(&a.cells)
            .then_with(|| a.from.sheet.0.cmp(&b.from.sheet.0))
            .then_with(|| a.from.left.cmp(&b.from.left))
            .then_with(|| a.from.top.cmp(&b.from.top))
    });
    schema.lookups = lookups;
    schema
}

/// A relation's identity, for merging.
///
/// Columns, not runs. A column broken by one hand-edited row groups into two
/// runs of the same shape, and that is one relation written twice — keying on
/// the rows would report it as two.
#[derive(PartialEq, Eq, Hash)]
struct Key {
    from_sheet: SheetId,
    from_left: u16,
    key_left: Option<u16>,
    table: RangeRef,
    column: Option<u32>,
    kind: LookupKind,
    approximate: bool,
}

impl Key {
    fn of(l: &Lookup) -> Key {
        Key {
            from_sheet: l.from.sheet,
            from_left: l.from.left,
            key_left: l.key.map(|k| k.left),
            table: l.table,
            column: l.column,
            kind: l.kind,
            approximate: l.approximate,
        }
    }
}

/// The bounding box of two ranges on one sheet.
fn widen(a: RangeRef, b: RangeRef) -> RangeRef {
    RangeRef::new(
        a.sheet,
        a.top.min(b.top),
        a.left.min(b.left),
        a.bottom.max(b.bottom),
        a.right.max(b.right),
    )
}

/// A lookup call as written, before its arguments are resolved.
struct Call<'a> {
    name: &'a str,
    args: &'a [Expr],
    /// For `INDEX(range, MATCH(...))`, the inner `MATCH`.
    inner: Option<&'a [Expr]>,
}

enum Outcome {
    Found(Lookup),
    /// The shape is a lookup and this cannot read its arguments.
    Unrecognised,
    /// It reads them, and they point outside this workbook.
    Unresolvable,
}

/// Collect every lookup-shaped call in an expression.
fn walk<'a>(expr: &'a Expr, out: &mut Vec<Call<'a>>) {
    match expr {
        Expr::Call { name, args } => {
            match name.as_str() {
                "VLOOKUP" | "HLOOKUP" | "XLOOKUP" => out.push(Call {
                    name,
                    args,
                    inner: None,
                }),
                // `INDEX(range, MATCH(key, keys, 0))` is one relation written as
                // two calls, and reading them separately would find a table with
                // no key and a key with no table.
                "INDEX" => {
                    let inner = args.iter().skip(1).find_map(|a| match a {
                        Expr::Call { name, args } if name == "MATCH" => Some(args.as_slice()),
                        _ => None,
                    });
                    if inner.is_some() {
                        out.push(Call { name, args, inner });
                    }
                }
                _ => {}
            }
            for arg in args {
                walk(arg, out);
            }
        }
        Expr::Unary { arg, .. } => walk(arg, out),
        Expr::Binary { lhs, rhs, .. } => {
            walk(lhs, out);
            walk(rhs, out);
        }
        _ => {}
    }
}

fn read(
    call: &Call<'_>,
    from: RangeRef,
    sheets: &FxHashMap<String, SheetId>,
    here: SheetId,
) -> Outcome {
    let range = |expr: Option<&Expr>| -> Option<Result<RangeRef, ()>> {
        match expr? {
            Expr::Reference { parsed, .. } => {
                // A table this crate cannot address is refused rather than
                // narrowed: reading `Jan:Dec!A:B` as `Jan!A:B` would state a
                // foreign key over one sheet of twelve, which is a claim
                // about the workbook that the workbook does not make.
                if parsed.workbook.is_some() || parsed.end_sheet_name.is_some() {
                    return Some(Err(()));
                }
                let sheet = match &parsed.sheet_name {
                    None => here,
                    Some(name) => match sheets.get(&name.to_uppercase()) {
                        Some(&id) => id,
                        None => return Some(Err(())),
                    },
                };
                Some(Ok(parsed.resolve(sheet)))
            }
            _ => None,
        }
    };
    let literal = |expr: Option<&Expr>| -> Option<f64> {
        match expr? {
            Expr::Literal(eg_model::CellValue::Number(n)) => Some(*n),
            _ => None,
        }
    };

    match (call.name, call.inner) {
        ("VLOOKUP" | "HLOOKUP", _) => {
            let table = match range(call.args.get(1)) {
                Some(Ok(r)) => r,
                Some(Err(())) => return Outcome::Unresolvable,
                None => return Outcome::Unrecognised,
            };
            let Some(index) = literal(call.args.get(2)) else {
                return Outcome::Unrecognised;
            };
            let key = key_column(range(call.args.first()), from);
            // Excel's fourth argument defaults to TRUE — approximate — and
            // nearly every correct lookup writes FALSE. Absent means banding.
            let approximate = match call.args.get(3) {
                None => true,
                Some(Expr::Literal(eg_model::CellValue::Bool(b))) => *b,
                Some(Expr::Literal(eg_model::CellValue::Number(n))) => *n != 0.0,
                // Written but empty (`VLOOKUP(x,tbl,2,)`) coerces to 0/FALSE
                // like any other empty argument — exact, not the default —
                // the same distinction `calc.rs`'s evaluator makes. Without
                // this, such a call fell to `Unrecognised` below and the
                // declared key it names was dropped from schema inference
                // entirely.
                Some(Expr::Literal(eg_model::CellValue::Empty)) => false,
                // `FALSE()` rather than `FALSE`. ODF's formula syntax spells
                // the boolean literals as zero-argument calls, so every
                // workbook LibreOffice has ever saved writes the exact-match
                // flag this way — and reading the schema more strictly than
                // `calc.rs` evaluates it meant such a workbook lost every
                // declared key it had, silently, to `Unrecognised`.
                Some(Expr::Call { name, args }) if args.is_empty() => {
                    match name.to_uppercase().as_str() {
                        "FALSE" => false,
                        "TRUE" => true,
                        _ => return Outcome::Unrecognised,
                    }
                }
                Some(_) => return Outcome::Unrecognised,
            };
            let vertical = call.name == "VLOOKUP";
            let column = index as u32;
            let returns = returns_of(table, column, vertical);
            Outcome::Found(Lookup {
                from,
                key,
                table,
                column: Some(column),
                returns,
                kind: if vertical {
                    LookupKind::Vlookup
                } else {
                    LookupKind::Hlookup
                },
                cells: 0,
                approximate,
            })
        }
        ("XLOOKUP", _) => {
            let keys = match range(call.args.get(1)) {
                Some(Ok(r)) => r,
                Some(Err(())) => return Outcome::Unresolvable,
                None => return Outcome::Unrecognised,
            };
            let returns = match range(call.args.get(2)) {
                Some(Ok(r)) => r,
                Some(Err(())) => return Outcome::Unresolvable,
                None => return Outcome::Unrecognised,
            };
            Outcome::Found(Lookup {
                from,
                key: key_column(range(call.args.first()), from),
                table: keys,
                column: None,
                returns: Some(returns),
                kind: LookupKind::Xlookup,
                cells: 0,
                approximate: false,
            })
        }
        ("INDEX", Some(inner)) => {
            let returns = match range(call.args.first()) {
                Some(Ok(r)) => r,
                Some(Err(())) => return Outcome::Unresolvable,
                None => return Outcome::Unrecognised,
            };
            let keys = match range(inner.get(1)) {
                Some(Ok(r)) => r,
                Some(Err(())) => return Outcome::Unresolvable,
                None => return Outcome::Unrecognised,
            };
            // A MATCH type of 0 is exact; anything else, or nothing, is a
            // banding over sorted keys. A written-but-empty argument
            // (`MATCH(k,r,)`) coerces to 0 like any other empty argument —
            // exact — which is not what an omitted one means.
            let approximate = match inner.get(2) {
                None => true,
                Some(Expr::Literal(eg_model::CellValue::Empty)) => false,
                Some(expr) => !matches!(literal(Some(expr)), Some(t) if t == 0.0),
            };
            Outcome::Found(Lookup {
                from,
                key: key_column(range(inner.first()), from),
                table: keys,
                column: None,
                returns: Some(returns),
                kind: LookupKind::IndexMatch,
                cells: 0,
                approximate,
            })
        }
        _ => Outcome::Unrecognised,
    }
}

/// The cells a key argument names, when it names any.
///
/// A lookup's key is nearly always a single relative cell on the formula's own
/// row — `VLOOKUP(C2, …)` in `AR2` — and the *column* is the relation: the row
/// is only where the group's representative happened to sit. So a key on the
/// group's own first row widens to that column over the group's rows, which is
/// the thing that joins.
///
/// A key that is not on that row is an absolute cell — one fixed lookup rather
/// than a column of them — and is left as the single cell it is. Anything else
/// (a concatenation, a literal, a cross-sheet reference) names no column of the
/// source table, and `None` says so rather than inventing one.
fn key_column(arg: Option<Result<RangeRef, ()>>, from: RangeRef) -> Option<RangeRef> {
    let range = arg?.ok()?;
    let single = range.top == range.bottom && range.left == range.right;
    if !single || range.sheet != from.sheet {
        return None;
    }
    if range.top == from.top {
        return Some(RangeRef::new(
            range.sheet,
            from.top,
            range.left,
            from.bottom,
            range.left,
        ));
    }
    Some(range)
}

/// The cells a `VLOOKUP` index actually reads, when the index is in range.
fn returns_of(table: RangeRef, column: u32, vertical: bool) -> Option<RangeRef> {
    if column == 0 {
        return None;
    }
    if vertical {
        let offset = u16::try_from(column - 1).ok()?;
        let col = table.left.checked_add(offset)?;
        (col <= table.right).then(|| RangeRef::new(table.sheet, table.top, col, table.bottom, col))
    } else {
        let row = table.top.checked_add(column - 1)?;
        (row <= table.bottom).then(|| RangeRef::new(table.sheet, row, table.left, row, table.right))
    }
}
