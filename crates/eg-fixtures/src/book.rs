//! The demonstration workbook: a fictional municipality's debtor impairment.
//!
//! The shape is deliberately the shape of the workbook this project was built
//! against — a long working table whose columns are filled-down formulas, small
//! lookup tables it keys into, and a summary sheet that reads across all of it.
//! None of the data is real; all of it is generated from a fixed seed, so
//! regenerating produces the same file byte for byte.
//!
//! Every feature the stack has is meant to have something here to find:
//!
//! - **Region detection** — several regions per sheet, separated by blank runs
//!   in both directions (the two lookup tables on `Rates` are side by side with
//!   an empty column between them), plus title and note blocks that are not
//!   tables at all.
//! - **Formula grouping** — each formula column is one filled-down shape, with
//!   one hand-edited row breaking the pattern the way a real workbook does.
//! - **Dependency lifting** — cross-sheet references, a range spanning two
//!   regions, and a defined name.
//! - **Schema inference** — an *exact* `VLOOKUP` (a foreign key), an
//!   *approximate* one (a banding over thresholds, which is not a key), and the
//!   same relation written as `INDEX`/`MATCH`.
//! - **Profiling** — categorical columns with few distinct values, a key column
//!   with as many distinct values as rows, and a column that is mostly numbers
//!   with genuine `#DIV/0!` errors in it.
//! - **Recompute** — everything above is checked against the values LibreOffice
//!   computed, which is the point of the fixture.
//! - **The refusals** — a 3-D reference and an unmodelled function, both of
//!   which must come back refused *by name* rather than guessed, and a cell
//!   reading the 3-D one, which must come back blocked rather than unchanged.
//!
//! There is deliberately no volatile function (`TODAY()`): its cached value
//! would change every time the fixture was regenerated, and a fixture whose
//! diff is never empty is one nobody re-runs. The volatile refusal is covered
//! by unit tests in `eg-eval`.

use crate::fods::{across, here, lit, on_range, Book, Cell, Sheet};

/// Sheet names, in tab order. `Jan`/`Feb`/`Mar` are adjacent on purpose: a 3-D
/// reference spans a *range of tabs*, so the order is part of the fixture.
const DEBTORS: &str = "Debtors";
const RATES: &str = "Rates";
const MONTHS: [&str; 3] = ["Jan", "Feb", "Mar"];
const SUMMARY: &str = "Summary";
const NOTES: &str = "Notes";

/// The row of `Debtors` whose provision was typed over by hand. Region
/// detection and grouping should both survive it, and grouping should report it
/// as the one cell that breaks its column's shape.
const HAND_EDITED_ROW: usize = 7;

/// How often an account is left with a zero balance, producing a real
/// `#DIV/0!` further along its row. Every reader of this fixture should meet an
/// error value somewhere, because a workbook of clean numbers is not one.
const ZERO_BALANCE_EVERY: usize = 47;

pub fn build(rows: usize) -> Book {
    let mut book = Book::default();
    book.push(debtors(rows));
    book.push(rates());
    for (i, month) in MONTHS.iter().enumerate() {
        book.push(month_sheet(month, i));
    }
    book.push(summary(rows));
    book.push(notes());
    // Workbook-scoped, which is the scope a real workbook uses for a rate
    // everything refers to.
    book.name("Tax_Rate", None, "$Rates.$B$11");
    book
}

/// The working table: one row per account, most columns filled down.
fn debtors(rows: usize) -> Sheet {
    let mut sheet = Sheet::new(DEBTORS);
    sheet.push(vec![
        Cell::text("Account"),
        Cell::text("Account Holder"),
        Cell::text("Suburb"),
        Cell::text("Debt Type"),
        Cell::text("Balance"),
        Cell::text("Days Overdue"),
        Cell::text("Ageing Bucket"),
        Cell::text("Discount Rate"),
        Cell::text("Provision Percent"),
        Cell::text("Provision"),
        Cell::text("Present Value"),
        Cell::text("Impairment"),
        Cell::text("Days per Rand"),
    ]);

    let mut rng = Rng::new(0x5EED_1234_ABCD_0001);
    for i in 0..rows {
        let row = i + 2;
        let category = CATEGORIES[rng.below(CATEGORIES.len())];
        let suburb = SUBURBS[rng.below(SUBURBS.len())];
        // A zero balance every so often, so the last column has real errors in
        // it rather than a uniform column of numbers.
        let balance = if i % ZERO_BALANCE_EVERY == 0 {
            0.0
        } else {
            round2(50.0 + rng.unit() * 45_000.0)
        };
        let days = rng.below(400) as f64;

        sheet.push(vec![
            Cell::text(format!("RB-{:06}", 100_000 + i)),
            Cell::text(holder(&mut rng, category, suburb)),
            Cell::text(suburb),
            Cell::text(category),
            Cell::Number(balance),
            Cell::Number(days),
            // Nested IF over the thresholds the bands are written at. The
            // comparison is on the number the sheet shows, which is the whole
            // of the 15-digit rule.
            Cell::formula(format!(
                "IF({f}<=30;{c};IF({f}<=60;{d30};IF({f}<=90;{d60};IF({f}<=120;{d90};{d120}))))",
                f = here(&a1("F", row)),
                c = lit("Current"),
                d30 = lit("31 to 60 days"),
                d60 = lit("61 to 90 days"),
                d90 = lit("91 to 120 days"),
                d120 = lit("Over 120 days"),
            )),
            // An exact lookup: this column's values are keys into the rates
            // table. That is a foreign key, and `eg schema` should say so.
            Cell::formula(format!(
                "VLOOKUP({key};{table};2;FALSE())",
                key = here(&a1("D", row)),
                table = on_range(RATES, "$A$4", "$B$7"),
            )),
            // An approximate lookup: the first column is a set of thresholds,
            // so this is a *banding* and joining it on equality would be wrong.
            Cell::formula(format!(
                "VLOOKUP({key};{table};2)",
                key = here(&a1("F", row)),
                table = on_range(RATES, "$D$4", "$E$8"),
            )),
            if i + 2 == HAND_EDITED_ROW {
                // The classic hand-edited row: a number typed over the formula
                // that every other row of the column still holds.
                Cell::Number(round2(balance * 0.5))
            } else {
                Cell::formula(format!(
                    "ROUND({bal}*{pct};2)",
                    bal = here(&a1("E", row)),
                    pct = here(&a1("I", row)),
                ))
            },
            // What the debt is worth now, discounted over the months it has
            // been outstanding. This is the arithmetic the real workbook
            // exists to do.
            Cell::formula(format!(
                "PV({rate}/12;{days}/30;0;-{bal})",
                rate = here(&a1("H", row)),
                days = here(&a1("F", row)),
                bal = here(&a1("E", row)),
            )),
            Cell::formula(format!(
                "ROUND({bal}-{pv};2)",
                bal = here(&a1("E", row)),
                pv = here(&a1("K", row)),
            )),
            // Divides by the balance, so a zero-balance account produces a real
            // `#DIV/0!` — an error the reader must carry and the recompute must
            // agree about, rather than skip.
            Cell::formula(format!(
                "{days}/{bal}",
                days = here(&a1("F", row)),
                bal = here(&a1("E", row)),
            )),
        ]);
    }
    sheet
}

/// Two lookup tables side by side, an empty column apart, under a title.
///
/// The gap is the point: region detection has no styling to read, so what
/// separates these two tables is the blank column between them and the blank
/// rows around them.
fn rates() -> Sheet {
    let mut sheet = Sheet::new(RATES);
    sheet.push(vec![Cell::text(
        "Riverbend Municipality — impairment rates and bands",
    )]);
    sheet.blank();
    sheet.push(vec![
        Cell::text("Debt Type"),
        Cell::text("Discount Rate"),
        Cell::Empty,
        Cell::text("Days Overdue From"),
        Cell::text("Provision Percent"),
    ]);
    let bands = [
        (0.0, 0.02),
        (31.0, 0.10),
        (61.0, 0.25),
        (91.0, 0.50),
        (121.0, 0.85),
    ];
    let rates = [
        ("Residential", 0.085),
        ("Business", 0.115),
        ("Indigent", 0.020),
        ("Municipal", 0.050),
    ];
    for (i, (from, pct)) in bands.iter().enumerate() {
        let mut row = Vec::new();
        match rates.get(i) {
            Some((name, rate)) => {
                row.push(Cell::text(*name));
                row.push(Cell::Number(*rate));
            }
            // The left table ends a row before the right one does, so the two
            // are not even the same height.
            None => {
                row.push(Cell::Empty);
                row.push(Cell::Empty);
            }
        }
        row.push(Cell::Empty);
        row.push(Cell::Number(*from));
        row.push(Cell::Number(*pct));
        sheet.push(row);
    }
    sheet.blank();
    sheet.blank();
    sheet.push(vec![Cell::text("Tax Rate"), Cell::Number(0.15)]);
    sheet
}

/// One month's billing, the same shape on every month sheet — which is what
/// makes a 3-D reference across them meaningful.
fn month_sheet(name: &str, index: usize) -> Sheet {
    let mut sheet = Sheet::new(name);
    sheet.push(vec![Cell::text("Debt Type"), Cell::text("Billed")]);
    let mut rng = Rng::new(0xB111_0000 + index as u64);
    for category in CATEGORIES {
        sheet.push(vec![
            Cell::text(category),
            Cell::Number(round2(10_000.0 + rng.unit() * 90_000.0)),
        ]);
    }
    sheet
}

/// The sheet that reads across everything else.
fn summary(rows: usize) -> Sheet {
    let last = rows + 1;
    let col = |c: &str| on_range(DEBTORS, &format!("${c}$2"), &format!("${c}${last}"));

    let mut sheet = Sheet::new(SUMMARY);
    sheet.push(vec![Cell::text(
        "Riverbend Municipality — debtor impairment summary",
    )]);
    sheet.blank();
    sheet.push(vec![Cell::text("Measure"), Cell::text("Value")]);

    let mut measure = |label: &str, formula: String| {
        sheet.push(vec![Cell::text(label), Cell::formula(formula)]);
    };
    measure("Total balance", format!("SUM({})", col("E")));
    measure("Total provision", format!("SUM({})", col("J")));
    measure("Total impairment", format!("SUM({})", col("L")));
    measure("Average balance", format!("AVERAGE({})", col("E")));
    measure("Largest balance", format!("MAX({})", col("E")));
    measure("Accounts", format!("COUNT({})", col("E")));
    // The same relation as the `VLOOKUP` on `Debtors`, written the way a
    // workbook writes it when a column might get inserted. `eg schema` should
    // recover one relation from it, not a table with no key.
    measure(
        "Business discount rate",
        format!(
            "INDEX({vals};MATCH({key};{keys};0))",
            vals = on_range(RATES, "$B$4", "$B$7"),
            key = lit("Business"),
            keys = on_range(RATES, "$A$4", "$A$7"),
        ),
    );
    // Through a defined name, which is otherwise invisible to a dependency walk.
    measure(
        "Provision after tax",
        format!("ROUND({b}*Tax_Rate;2)", b = here("B5")),
    );
    // A 3-D reference. The evaluator refuses it by name rather than guessing,
    // so this cell is *unsupported* in a check and *blocked* in a what-if —
    // and the cell below, which reads it, must be blocked in turn rather than
    // reported unchanged.
    measure(
        "Billed, January to March",
        format!("SUM({})", across(MONTHS[0], MONTHS[2], "B2")),
    );
    measure(
        "Billed less provision",
        format!("{a}-{b}", a = here("B12"), b = here("B5")),
    );
    // A function this evaluator does not model. Refused by name, which is a
    // different outcome from a wrong answer.
    measure(
        "Balance-weighted rate",
        format!(
            "SUMPRODUCT({bal};{rate})/{total}",
            bal = col("E"),
            rate = col("H"),
            total = here("B4"),
        ),
    );
    sheet
}

/// Prose, which is what a question is made of.
fn notes() -> Sheet {
    let mut sheet = Sheet::new(NOTES);
    sheet.push(vec![Cell::text("Notes to the impairment calculation")]);
    sheet.blank();
    for note in [
        "Provision for debtors with balances outstanding over 120 days is raised at 85 percent.",
        "Indigent accounts are discounted at the reduced rate approved by council.",
        "The discount rate is applied monthly over the period the account has been overdue.",
        "The rates and bands table is reviewed at each financial year end.",
        "Accounts with a zero balance are retained for audit and excluded from the ratio columns.",
    ] {
        sheet.push(vec![Cell::text(note)]);
    }
    sheet
}

// ---- generated data --------------------------------------------------------

const CATEGORIES: [&str; 4] = ["Residential", "Business", "Indigent", "Municipal"];
const SUBURBS: [&str; 6] = [
    "Kloofview",
    "Marula Park",
    "Stonebridge",
    "Weavers Nek",
    "Palm Grove",
    "Ironwood",
];
const TRADES: [&str; 8] = [
    "Holdings",
    "Trading",
    "Bakery",
    "Motors",
    "Properties",
    "Farms",
    "Workshop",
    "Butchery",
];

/// An account holder's name.
///
/// Invented entities rather than invented people: a fixture that ships in a
/// public repository should not carry anything that reads like a person's
/// details, even fictional ones.
fn holder(rng: &mut Rng, category: &str, suburb: &str) -> String {
    match category {
        "Business" => format!("{} {}", suburb, TRADES[rng.below(TRADES.len())]),
        "Municipal" => format!("{suburb} Municipal Depot"),
        _ => format!("Unit {} {}", 1 + rng.below(400), suburb),
    }
}

fn round2(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

/// An A1 address from a column letter and a row number.
fn a1(col: &str, row: usize) -> String {
    format!("{col}{row}")
}

/// xorshift64*, so the fixture is the same on every machine and every run.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A float in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}
