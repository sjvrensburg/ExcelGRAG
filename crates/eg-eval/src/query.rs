//! Asking a table a question that is not about one cell.
//!
//! Everything else here answers *where* — which cells feed this, what moves if
//! that changes, does this formula still agree with its inputs. None of it
//! answers "what is the total debt outstanding for residential debtors", which
//! is the question a person actually has, and which the workbook only answers
//! if somebody already wrote a cell that computes it.
//!
//! This is that: filter the rows of one table, group them, and aggregate. It is
//! deliberately not SQL and deliberately not a join — [`crate::schema`] recovers
//! what joins to what, and following one is the caller's business.
//!
//! It lives in this crate rather than beside [`eg_structure::read_table`] for
//! one reason: the arithmetic has to be the *evaluator's*. A sheet carries 15
//! significant digits, and a `SUM` here that used plain `f64` addition would
//! disagree with the `SUM()` in the cell next to it — a second opinion that is
//! wrong, which is the one thing this project must not produce.
//!
//! # What it refuses
//!
//! Refusing is most of the design, because the failure mode of a query engine
//! over a spreadsheet is a confident wrong number.
//!
//! - **Two columns with one header.** Real workbooks have `Total` under both
//!   `Q3` and `Q4`. Answering with either is a coin toss.
//! - **Summing a column that is not numeric.** A column that is 40% number and
//!   35% text has no total, and producing one by skipping the text is how a
//!   figure ends up quietly short.
//! - **A region it was not asked about.** Every answer carries the range it was
//!   computed over, because region boundaries are *inferred* — a totals row
//!   swept into a table's body would double every sum, and the only defence is
//!   that the caller can see what was summed.
//!
//! Error cells are counted and reported rather than skipped silently: a total
//! over a column holding 46,386 `#REF!`s is a fact about the workbook before it
//! is an answer.

use eg_model::{CellValue, RangeRef, Workbook};
use eg_structure::{ColumnKind, Table, TableColumn};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::calc::shown;

/// Why a query could not be answered.
///
/// Every one of these is a case where an answer *could* have been produced and
/// would have been wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("this table has no column called {0:?}")]
    NoSuchColumn(String),
    #[error("{0:?} names more than one column of this table; say which by its range")]
    AmbiguousColumn(String),
    #[error("{column:?} is a {kind} column, so {what} of it is not a number")]
    NotNumeric {
        column: String,
        kind: &'static str,
        what: &'static str,
    },
    #[error("nothing to compute: give at least one aggregate")]
    NothingAsked,
}

/// A test one row must pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Test {
    Is(CellValue),
    IsNot(CellValue),
    /// Case-insensitive substring, for text.
    Contains(String),
    OneOf(Vec<CellValue>),
    Above(f64),
    AtLeast(f64),
    Below(f64),
    AtMost(f64),
    Blank,
    NotBlank,
    /// Holds an Excel error value.
    Failed,
}

/// One condition on one column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub column: String,
    pub test: Test,
}

/// Something to compute over the rows that pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Aggregate {
    /// Rows, not cells: a blank in the column is still a row.
    Count,
    /// Rows where the column holds something.
    CountValues(String),
    CountDistinct(String),
    Sum(String),
    Mean(String),
    Min(String),
    Max(String),
}

impl Aggregate {
    fn column(&self) -> Option<&str> {
        match self {
            Aggregate::Count => None,
            Aggregate::CountValues(c)
            | Aggregate::CountDistinct(c)
            | Aggregate::Sum(c)
            | Aggregate::Mean(c)
            | Aggregate::Min(c)
            | Aggregate::Max(c) => Some(c),
        }
    }

    /// Whether it needs the column to be numeric, and what to call it if not.
    fn needs_numbers(&self) -> Option<&'static str> {
        match self {
            Aggregate::Sum(_) => Some("the sum"),
            Aggregate::Mean(_) => Some("the mean"),
            Aggregate::Min(_) => Some("the minimum"),
            Aggregate::Max(_) => Some("the maximum"),
            _ => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Aggregate::Count => "count".to_string(),
            Aggregate::CountValues(c) => format!("count of {c}"),
            Aggregate::CountDistinct(c) => format!("distinct {c}"),
            Aggregate::Sum(c) => format!("sum of {c}"),
            Aggregate::Mean(c) => format!("mean of {c}"),
            Aggregate::Min(c) => format!("min of {c}"),
            Aggregate::Max(c) => format!("max of {c}"),
        }
    }
}

/// What to ask of a table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    /// All must pass. An empty list keeps every row.
    pub filters: Vec<Filter>,
    /// Columns to group by, outermost first. Empty means one group of
    /// everything.
    pub group_by: Vec<String>,
    pub aggregates: Vec<Aggregate>,
    /// Groups returned. The counts in the report are exact regardless.
    pub limit: usize,
}

/// One group's answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// One value per entry of `group_by`.
    pub key: Vec<CellValue>,
    pub rows: u64,
    /// One per entry of `aggregates`, in order. `None` where the group had no
    /// number to compute from — which is a different answer from zero.
    pub values: Vec<Option<f64>>,
    /// Counts, which are never fractional and would read wrong as floats.
    pub counts: Vec<Option<u64>>,
}

/// An answer, and everything needed to check it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// The cells this was computed over. Region boundaries are inferred, so an
    /// answer that cannot be checked against its range cannot be trusted.
    pub over: RangeRef,
    pub groups: Vec<Group>,
    /// Groups past `limit`, which are counted and not returned.
    pub groups_not_listed: u64,
    pub rows_scanned: u64,
    pub rows_matched: u64,
    /// Rows dropped by a filter because the column held an error value.
    pub rows_with_errors: u64,
    /// Error cells seen in an aggregated column. A total over a column that is
    /// mostly `#REF!` is a finding before it is an answer.
    pub errors_in_aggregates: u64,
}

impl Answer {
    /// The single group, for a query that did not group.
    pub fn one(&self) -> Option<&Group> {
        (self.groups.len() == 1).then(|| &self.groups[0])
    }
}

/// Run a query against one table.
pub fn query(workbook: &Workbook, table: &Table, query: &Query) -> Result<Answer, QueryError> {
    if query.aggregates.is_empty() {
        return Err(QueryError::NothingAsked);
    }
    let sheet = workbook
        .sheet(table.body.sheet)
        .ok_or_else(|| QueryError::NoSuchColumn(String::new()))?;

    // Resolved up front so a query naming a column this table does not have
    // fails before it reads a cell, rather than returning an empty answer that
    // looks like "nothing matched".
    let filters: Vec<(usize, &Test)> = query
        .filters
        .iter()
        .map(|f| Ok((index_of(table, &f.column)?, &f.test)))
        .collect::<Result<_, _>>()?;
    let groups: Vec<usize> = query
        .group_by
        .iter()
        .map(|c| index_of(table, c))
        .collect::<Result<_, _>>()?;
    let aggregates: Vec<(Option<usize>, &Aggregate)> = query
        .aggregates
        .iter()
        .map(|a| {
            let Some(name) = a.column() else {
                return Ok((None, a));
            };
            let i = index_of(table, name)?;
            if let Some(what) = a.needs_numbers() {
                let column = &table.columns[i];
                if !column.kind.is_numeric() {
                    return Err(QueryError::NotNumeric {
                        column: name.to_string(),
                        kind: column.kind.as_str(),
                        what,
                    });
                }
            }
            Ok((Some(i), a))
        })
        .collect::<Result<_, _>>()?;

    let mut answer = Answer {
        over: table.body,
        groups: Vec::new(),
        groups_not_listed: 0,
        rows_scanned: 0,
        rows_matched: 0,
        rows_with_errors: 0,
        errors_in_aggregates: 0,
    };
    // Grouped by the rendered key so two spellings of one number are one group,
    // with the first key seen kept for reporting.
    let mut buckets: FxHashMap<String, (Vec<CellValue>, Accumulators)> = FxHashMap::default();
    let mut order: Vec<String> = Vec::new();

    for row in table.read(sheet) {
        answer.rows_scanned += 1;
        let mut kept = true;
        for (i, test) in &filters {
            let value = &row[*i];
            if matches!(value, CellValue::Error(_)) && !matches!(test, Test::Failed) {
                answer.rows_with_errors += 1;
                kept = false;
                break;
            }
            if !passes(value, test) {
                kept = false;
                break;
            }
        }
        if !kept {
            continue;
        }
        answer.rows_matched += 1;

        let key: Vec<CellValue> = groups.iter().map(|&i| row[i].clone()).collect();
        let id = key.iter().map(render).collect::<Vec<_>>().join("\u{1f}");
        let slot = match buckets.get_mut(&id) {
            Some(slot) => slot,
            None => {
                order.push(id.clone());
                buckets
                    .entry(id)
                    .or_insert_with(|| (key, Accumulators::new(aggregates.len())))
            }
        };
        slot.1.rows += 1;
        for (n, (column, aggregate)) in aggregates.iter().enumerate() {
            let Some(i) = column else {
                continue;
            };
            let value = &row[*i];
            if matches!(value, CellValue::Error(_)) {
                answer.errors_in_aggregates += 1;
                continue;
            }
            slot.1.see(n, aggregate, value);
        }
    }

    answer.groups_not_listed = (order.len().saturating_sub(query.limit.max(1))) as u64;
    for id in order.into_iter().take(query.limit.max(1)) {
        let (key, acc) = buckets.remove(&id).expect("keyed by what was inserted");
        answer.groups.push(acc.finish(key, &aggregates));
    }
    Ok(answer)
}

/// The column index for a header, refusing what cannot be resolved.
fn index_of(table: &Table, header: &str) -> Result<usize, QueryError> {
    let mut found = None;
    for (i, column) in table.columns.iter().enumerate() {
        if column.header.eq_ignore_ascii_case(header) {
            if found.is_some() {
                return Err(QueryError::AmbiguousColumn(header.to_string()));
            }
            found = Some(i);
        }
    }
    found.ok_or_else(|| QueryError::NoSuchColumn(header.to_string()))
}

fn passes(value: &CellValue, test: &Test) -> bool {
    match test {
        Test::Is(want) => equal(value, want),
        Test::IsNot(want) => !equal(value, want),
        Test::OneOf(wants) => wants.iter().any(|w| equal(value, w)),
        Test::Contains(needle) => match value {
            CellValue::Text(t) => t.to_lowercase().contains(&needle.to_lowercase()),
            _ => false,
        },
        Test::Above(n) => number(value).is_some_and(|v| shown(v) > shown(*n)),
        Test::AtLeast(n) => number(value).is_some_and(|v| shown(v) >= shown(*n)),
        Test::Below(n) => number(value).is_some_and(|v| shown(v) < shown(*n)),
        Test::AtMost(n) => number(value).is_some_and(|v| shown(v) <= shown(*n)),
        Test::Blank => value.is_empty(),
        Test::NotBlank => !value.is_empty(),
        Test::Failed => matches!(value, CellValue::Error(_)),
    }
}

/// Equality as a sheet sees it: text is case-insensitive, numbers are compared
/// at the 15 digits a sheet carries.
fn equal(value: &CellValue, want: &CellValue) -> bool {
    match (value, want) {
        (CellValue::Text(a), CellValue::Text(b)) => a.eq_ignore_ascii_case(b),
        (CellValue::Number(a), CellValue::Number(b)) => shown(*a) == shown(*b),
        (a, b) => a == b,
    }
}

fn number(value: &CellValue) -> Option<f64> {
    match value {
        CellValue::Number(n) => Some(*n),
        CellValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn render(value: &CellValue) -> String {
    match value {
        CellValue::Empty => String::new(),
        CellValue::Number(n) => format!("{}", shown(*n)),
        CellValue::Text(t) => t.to_lowercase(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::Error(e) => e.as_str().to_string(),
    }
}

/// One group's running totals.
struct Accumulators {
    rows: u64,
    sums: Vec<f64>,
    counts: Vec<u64>,
    mins: Vec<Option<f64>>,
    maxes: Vec<Option<f64>>,
    distinct: Vec<Option<std::collections::HashSet<String>>>,
}

impl Accumulators {
    fn new(n: usize) -> Self {
        Accumulators {
            rows: 0,
            sums: vec![0.0; n],
            counts: vec![0; n],
            mins: vec![None; n],
            maxes: vec![None; n],
            distinct: vec![None; n],
        }
    }

    fn see(&mut self, n: usize, aggregate: &Aggregate, value: &CellValue) {
        match aggregate {
            Aggregate::CountValues(_) => {
                if !value.is_empty() {
                    self.counts[n] += 1;
                }
            }
            Aggregate::CountDistinct(_) => {
                if !value.is_empty() {
                    self.distinct[n]
                        .get_or_insert_with(Default::default)
                        .insert(render(value));
                }
            }
            _ => {
                let Some(v) = number(value) else {
                    return;
                };
                self.counts[n] += 1;
                // Accumulated raw and rounded once at the end. Rounding every
                // step to fifteen digits would be a regime Excel does not have,
                // and over a column of 115,004 rows it would drift from what
                // the sheet's own `SUM()` produces rather than towards it.
                self.sums[n] += v;
                self.mins[n] = Some(self.mins[n].map_or(v, |m| m.min(v)));
                self.maxes[n] = Some(self.maxes[n].map_or(v, |m| m.max(v)));
            }
        }
    }

    fn finish(self, key: Vec<CellValue>, aggregates: &[(Option<usize>, &Aggregate)]) -> Group {
        let mut values = Vec::with_capacity(aggregates.len());
        let mut counts = Vec::with_capacity(aggregates.len());
        for (n, (_, aggregate)) in aggregates.iter().enumerate() {
            match aggregate {
                Aggregate::Count => {
                    values.push(None);
                    counts.push(Some(self.rows));
                }
                Aggregate::CountValues(_) => {
                    values.push(None);
                    counts.push(Some(self.counts[n]));
                }
                Aggregate::CountDistinct(_) => {
                    values.push(None);
                    counts.push(Some(
                        self.distinct[n].as_ref().map_or(0, |d| d.len() as u64),
                    ));
                }
                Aggregate::Sum(_) => {
                    // Rounded to the fifteen significant digits a sheet
                    // carries, once, so the answer is the number a sheet would
                    // show rather than the last bits of an `f64`.
                    values.push((self.counts[n] > 0).then(|| shown(self.sums[n])));
                    counts.push(None);
                }
                Aggregate::Mean(_) => {
                    values.push(
                        (self.counts[n] > 0).then(|| shown(self.sums[n] / self.counts[n] as f64)),
                    );
                    counts.push(None);
                }
                Aggregate::Min(_) => {
                    values.push(self.mins[n]);
                    counts.push(None);
                }
                Aggregate::Max(_) => {
                    values.push(self.maxes[n]);
                    counts.push(None);
                }
            }
        }
        Group {
            key,
            rows: self.rows,
            values,
            counts,
        }
    }
}

/// The columns a query may name, for a caller building one.
pub fn columns(table: &Table) -> Vec<&TableColumn> {
    table.columns.iter().collect()
}

/// Whether a column can be summed.
pub fn is_summable(column: &TableColumn) -> bool {
    column.kind == ColumnKind::Number
}
