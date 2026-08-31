//! What is actually in a column, summarised small enough to store.
//!
//! The graph says where a table is and [`crate::table`] says what shape it has.
//! Neither says what is *in* it, and that is the gap a person's question falls
//! into: nothing in the index knows that the `Debt Type` column holds
//! `Residential`, `Business` and `Indigent`, so "which debtors are indigent"
//! cannot match anything. A workbook is asked about in the vocabulary of its
//! values at least as often as in the vocabulary of its headers.
//!
//! A profile is that vocabulary plus enough arithmetic to answer a question
//! without opening the workbook: how many rows, how many blank, the range and
//! total of a numeric column, and — for a column with few enough of them — the
//! distinct values and their counts.
//!
//! **Two things a profile is not.** It is not a copy of the column: the
//! distinct list is abandoned above [`ProfileOptions::max_distinct`], so a key
//! column of 195,366 customer numbers profiles to a count and nothing else. And
//! it is not free of the workbook's data — a distinct list *is* cell values, and
//! a sum is a number the workbook never wrote down anywhere. Everything else in
//! a corpus is structure; this is the exception, and [`ProfileOptions::values`]
//! is how a caller declines it.

use eg_model::{CellValue, RangeRef, Sheet};
use serde::{Deserialize, Serialize};

use crate::table::{ColumnKind, Table, TableColumn};

/// How much of a column to keep.
#[derive(Debug, Clone)]
pub struct ProfileOptions {
    /// Keep the distinct values of a column that has at most this many.
    ///
    /// The point is the categorical column — a debt type, a status, a
    /// classification — which is what a question names. A column with hundreds
    /// of distinct values is an identifier or a measurement, and listing it
    /// would be storing the column.
    pub max_distinct: usize,
    /// Stop counting distinct values once this many have been seen.
    ///
    /// Separate from `max_distinct` and larger: the counter has to know it went
    /// past the keeping threshold, and it must not hold a set the size of the
    /// column to find that out.
    pub abandon_distinct_after: usize,
    /// Longest text value kept, in characters. A cell can hold a paragraph.
    pub max_value_chars: usize,
    /// Whether to record anything derived from the values themselves — the
    /// distinct lists and the numeric summaries.
    ///
    /// `false` leaves the counts and the types, which are structure, and drops
    /// everything that is data. For a corpus that must not hold the contents of
    /// the workbook it indexes.
    pub values: bool,
}

impl Default for ProfileOptions {
    fn default() -> Self {
        ProfileOptions {
            max_distinct: 64,
            abandon_distinct_after: 1024,
            max_value_chars: 64,
            values: true,
        }
    }
}

/// One distinct value and how often it occurs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueCount {
    pub value: String,
    pub count: u64,
    /// Whether `value` was cut to [`ProfileOptions::max_value_chars`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// The arithmetic of a numeric column.
///
/// `sum` is a number the workbook may never have written down, which is the
/// point: "what is the total debt outstanding" is answerable from here without
/// opening a 170 MB file.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NumericSummary {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub mean: f64,
}

/// What one column holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnProfile {
    pub header: String,
    pub range: RangeRef,
    pub kind: ColumnKind,
    /// Populated cells.
    pub populated: u64,
    /// Cells of the column's range holding nothing.
    pub empty: u64,
    /// Cells holding an Excel error value, counted separately because a column
    /// that is 3% `#REF!` is a finding rather than a type.
    pub errors: u64,
    /// Distinct values and their counts, most frequent first.
    ///
    /// `None` when the column had too many to be worth keeping, or when values
    /// were not collected at all. The distinction matters and is in
    /// `distinct_count`.
    pub distinct: Option<Vec<ValueCount>>,
    /// How many distinct values the column has. `None` once counting was
    /// abandoned — "more than we were willing to count", not "unknown".
    pub distinct_count: Option<u64>,
    pub numeric: Option<NumericSummary>,
}

impl ColumnProfile {
    /// Whether this column reads as a category — few values, each repeated.
    ///
    /// The columns a question names by value rather than by header. Repetition
    /// is the test that separates them from an identifier: kept distinct values
    /// are already few, and few values over few rows is a lookup table's key,
    /// not a category. Average repetition of two is the line.
    pub fn is_categorical(&self) -> bool {
        match (&self.distinct, self.distinct_count) {
            (Some(values), Some(n)) => n > 0 && !values.is_empty() && self.populated >= n * 2,
            _ => false,
        }
    }
}

/// Every profiled column of one workbook.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profiles {
    pub columns: Vec<ColumnProfile>,
    /// Whether the values themselves were collected. A profile set built with
    /// [`ProfileOptions::values`] off carries counts and types only, and a
    /// reader must not take a missing `numeric` for a column with no numbers.
    pub values: bool,
}

impl Profiles {
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Columns whose values a question is likely to name.
    pub fn categorical(&self) -> impl Iterator<Item = &ColumnProfile> {
        self.columns.iter().filter(|c| c.is_categorical())
    }
}

/// Profile every column of a table.
///
/// One row-major pass over the region, for the reason [`crate::table::read_table`]
/// takes one: a sheet is an ordered map keyed by (row, column), so asking it
/// for a single column costs a probe per row, and a table 136 columns wide pays
/// that 136 times over.
pub fn profile_table(sheet: &Sheet, table: &Table, opts: &ProfileOptions) -> Vec<ColumnProfile> {
    let body = table.body;
    let width = table.columns.len();
    let mut state: Vec<ColumnState> = (0..width).map(|_| ColumnState::default()).collect();

    for (at, cell) in sheet.iter_range(body) {
        let Some(slot) = state.get_mut(usize::from(at.col - body.left)) else {
            continue;
        };
        slot.see(&cell.value, opts);
    }

    table
        .columns
        .iter()
        .zip(state)
        .map(|(column, state)| state.finish(column, opts))
        .collect()
}

/// What one column has been seen to hold, so far.
#[derive(Default)]
struct ColumnState {
    populated: u64,
    errors: u64,
    numbers: u64,
    min: f64,
    max: f64,
    sum: f64,
    /// Dropped whole the moment it grows past what we were willing to count: a
    /// key column has as many distinct values as rows, and holding that map is
    /// holding the column.
    counts: std::collections::HashMap<String, u64>,
    abandoned: bool,
}

impl ColumnState {
    fn see(&mut self, value: &CellValue, opts: &ProfileOptions) {
        match value {
            CellValue::Empty => return,
            CellValue::Error(_) => self.errors += 1,
            CellValue::Number(n) => {
                if self.numbers == 0 {
                    self.min = *n;
                    self.max = *n;
                } else {
                    self.min = self.min.min(*n);
                    self.max = self.max.max(*n);
                }
                self.numbers += 1;
                self.sum += *n;
            }
            _ => {}
        }
        self.populated += 1;
        if !opts.values || self.abandoned {
            return;
        }
        let key = render(value, opts.max_value_chars);
        if self.counts.len() >= opts.abandon_distinct_after && !self.counts.contains_key(&key) {
            self.abandoned = true;
            self.counts = std::collections::HashMap::new();
            return;
        }
        *self.counts.entry(key).or_default() += 1;
    }

    fn finish(self, column: &TableColumn, opts: &ProfileOptions) -> ColumnProfile {
        let distinct_count = (!self.abandoned && opts.values).then_some(self.counts.len() as u64);
        let keep = opts.values && !self.abandoned && self.counts.len() <= opts.max_distinct;
        let distinct = keep.then(|| {
            let mut values: Vec<ValueCount> = self
                .counts
                .into_iter()
                .map(|(value, count)| ValueCount {
                    truncated: value.chars().count() >= opts.max_value_chars,
                    value,
                    count,
                })
                .collect();
            // Most frequent first, then by value, so two runs over one workbook
            // write the same file. A hash map's order is not an order.
            values.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
            values
        });
        let numeric = (opts.values && self.numbers > 0).then(|| NumericSummary {
            min: self.min,
            max: self.max,
            sum: self.sum,
            mean: self.sum / self.numbers as f64,
        });

        ColumnProfile {
            header: column.header.clone(),
            range: column.range,
            kind: column.kind,
            populated: self.populated,
            empty: column.range.cell_count().saturating_sub(self.populated),
            errors: self.errors,
            distinct,
            distinct_count,
            numeric,
        }
    }
}

/// One value, as the string a profile stores it under.
///
/// A number is written the way a sheet shows it rather than the way Rust prints
/// it, so `1` and `1.0` are one value and not two.
fn render(value: &CellValue, max_chars: usize) -> String {
    let full = match value {
        CellValue::Empty => String::new(),
        CellValue::Number(n) => format!("{n}"),
        CellValue::Text(t) => t.clone(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::Error(e) => e.as_str().to_string(),
    };
    if full.chars().count() <= max_chars {
        return full;
    }
    full.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_model::{Cell, RangeRef, SheetId};

    use crate::region::{Region, RegionKind, RegionSource};
    use crate::table::read_table;

    fn grid(rows: &[&str]) -> Sheet {
        let mut sheet = Sheet::new(SheetId(0), "Sheet1");
        for (r, line) in rows.iter().enumerate() {
            for (c, tok) in line.split_whitespace().enumerate() {
                if tok == "." {
                    continue;
                }
                let value = match tok.parse::<f64>() {
                    Ok(n) => CellValue::Number(n),
                    Err(_) => CellValue::Text(tok.to_string()),
                };
                sheet.set(r as u32, c as u16, Cell::literal(value));
            }
        }
        sheet
    }

    /// A region over the whole grid, one header row and one label column.
    fn region(bottom: u32, right: u16, headers: &[&str]) -> Region {
        Region {
            range: RangeRef::new(SheetId(0), 0, 0, bottom, right),
            kind: RegionKind::Table,
            source: RegionSource::Declared,
            title: None,
            header_rows: 1,
            header_cols: 1,
            headers: headers.iter().map(|h| h.to_string()).collect(),
            cell_count: 0,
        }
    }

    fn profiles(sheet: &Sheet, region: &Region, opts: &ProfileOptions) -> Vec<ColumnProfile> {
        let table = read_table(sheet, region).expect("the region has a body");
        profile_table(sheet, &table, opts)
    }

    #[test]
    fn a_categorical_column_keeps_its_values_and_their_counts() {
        // The whole point: nothing in the index knew that this column holds
        // "Residential" until now, so a question naming it could match nothing.
        let sheet = grid(&[
            "Customer Type Debt",
            "North Residential 1200",
            "South Business 3400",
            "East Residential 900",
            "West Residential 700",
        ]);
        let found = profiles(
            &sheet,
            &region(4, 2, &["Type", "Debt"]),
            &Default::default(),
        );
        let types = &found[0];

        assert_eq!(types.header, "Type");
        assert_eq!(types.distinct_count, Some(2));
        let values = types.distinct.as_ref().expect("few enough to keep");
        // Most frequent first, so a reader sees the shape of the column.
        assert_eq!(values[0].value, "Residential");
        assert_eq!(values[0].count, 3);
        assert_eq!(values[1].value, "Business");
        assert!(types.is_categorical(), "three rows over two values");
    }

    #[test]
    fn a_column_with_a_value_per_row_is_counted_and_not_copied() {
        // A key column has as many distinct values as rows. Listing it would be
        // storing the column, which is the thing a profile must not become.
        let rows: Vec<String> = (0..40)
            .map(|i| format!("cust{i} id{i} {}", i * 10))
            .collect();
        let mut lines = vec!["Customer Ref Debt".to_string()];
        lines.extend(rows);
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let sheet = grid(&refs);

        let opts = ProfileOptions {
            max_distinct: 8,
            ..Default::default()
        };
        let found = profiles(&sheet, &region(40, 2, &["Ref", "Debt"]), &opts);
        let ids = &found[0];
        assert!(ids.distinct.is_none(), "no list for 40 distinct values");
        assert_eq!(ids.distinct_count, Some(40), "but it still knows how many");
        assert!(!ids.is_categorical());
    }

    #[test]
    fn counting_gives_up_rather_than_holding_the_column() {
        let rows: Vec<String> = (0..40).map(|i| format!("c{i} id{i} 1")).collect();
        let mut lines = vec!["Customer Ref Debt".to_string()];
        lines.extend(rows);
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let sheet = grid(&refs);

        let opts = ProfileOptions {
            abandon_distinct_after: 10,
            ..Default::default()
        };
        let found = profiles(&sheet, &region(40, 2, &["Ref", "Debt"]), &opts);
        assert_eq!(
            found[0].distinct_count, None,
            "None means more than we were willing to count, not unknown"
        );
        assert_eq!(found[0].populated, 40, "the counting still happened");
    }

    #[test]
    fn a_numeric_column_carries_arithmetic_the_workbook_never_wrote_down() {
        let sheet = grid(&[
            "Customer Type Debt",
            "North Residential 1200",
            "South Business 3400",
            "East Residential 900",
        ]);
        let found = profiles(
            &sheet,
            &region(3, 2, &["Type", "Debt"]),
            &Default::default(),
        );
        let debt = found[1].numeric.expect("a numeric column");

        assert_eq!(debt.min, 900.0);
        assert_eq!(debt.max, 3400.0);
        assert_eq!(debt.sum, 5500.0);
        assert!(found[0].numeric.is_none(), "and a text column has none");
    }

    #[test]
    fn refusing_values_leaves_the_counts_and_takes_the_data() {
        // The corpus otherwise holds only structure. This is the switch that
        // keeps it that way, and what it must leave behind is everything a
        // reader needs to know the column exists.
        let sheet = grid(&[
            "Customer Type Debt",
            "North Residential 1200",
            "South Business 3400",
        ]);
        let opts = ProfileOptions {
            values: false,
            ..Default::default()
        };
        let found = profiles(&sheet, &region(2, 2, &["Type", "Debt"]), &opts);

        assert!(found[0].distinct.is_none());
        assert_eq!(found[0].distinct_count, None);
        assert!(found[1].numeric.is_none(), "a sum is a value too");
        assert_eq!(found[0].populated, 2, "the shape is still there");
        assert_eq!(found[1].kind, ColumnKind::Number);
        assert!(!found[0].is_categorical());
    }

    #[test]
    fn a_long_value_is_cut_and_says_that_it_was() {
        let long = "x".repeat(200);
        let sheet = grid(&[
            "Customer Note Debt",
            &format!("North {long} 1"),
            &format!("South {long} 2"),
        ]);
        let opts = ProfileOptions {
            max_value_chars: 10,
            ..Default::default()
        };
        let found = profiles(&sheet, &region(2, 2, &["Note", "Debt"]), &opts);
        let value = &found[0].distinct.as_ref().unwrap()[0];
        assert_eq!(value.value.chars().count(), 10);
        assert!(value.truncated);
    }

    #[test]
    fn errors_are_counted_apart_from_the_type() {
        // A column that is 3% #REF! is a finding, not a type.
        let mut sheet = grid(&["Customer Debt", "North 1", "South 2", "East 3"]);
        sheet.set(
            3,
            1,
            Cell::literal(CellValue::Error(eg_model::ErrorKind::Ref)),
        );
        let found = profiles(&sheet, &region(3, 1, &["Debt"]), &Default::default());
        assert_eq!(found[0].errors, 1);
        assert_eq!(
            found[0].kind,
            ColumnKind::Number,
            "two of three is a majority"
        );
        assert_eq!(
            found[0].numeric.unwrap().sum,
            3.0,
            "the error is not a zero"
        );
    }
}
