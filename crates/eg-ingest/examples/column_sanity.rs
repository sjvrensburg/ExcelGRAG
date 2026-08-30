//! Check that decoded formulas reference columns that could actually exist.
//!
//! The row-advance invariant used by `shared_group_check` validates rows but is
//! blind to column errors: every member of a group gets the *same* wrong column,
//! so consecutive rows still differ by exactly one. A systematically mis-decoded
//! relative column offset therefore passes it unnoticed. That is precisely how a
//! real bug survived review — BIFF8 stores the column offset in 8 bits, and
//! reading it as 14 turned `SUM(B38:I38)` into `SUM(IX38:JE38)`.
//!
//! This checks the complementary property: a sheet-local reference should land
//! inside, or close to, the columns the sheet actually uses. References far
//! outside are the signature of a sign-extension or field-width mistake.
//!
//! Prints counts and A1 addresses only — never formula text.

use eg_ingest::{load_with, LoadOptions};

/// How far past the used range a local reference may sit before it is suspect.
/// Formulas do legitimately point just outside the populated area.
const SLACK: u32 = 64;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: column_sanity <workbook>");
        std::process::exit(2);
    });

    let opts = LoadOptions {
        max_cells: None,
        ..Default::default()
    };
    let loaded = match load_with(&path, &opts) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("load failed: {e}");
            std::process::exit(1);
        }
    };

    let (mut refs, mut beyond_sheet, mut beyond_used, mut names) = (0usize, 0usize, 0usize, 0usize);
    let mut examples: Vec<String> = Vec::new();

    for sheet in &loaded.workbook.sheets {
        let Some(used) = sheet.used_range() else {
            continue;
        };
        let limit = used.right as u32 + SLACK;

        for (addr, cell) in sheet.iter() {
            let Some(formula) = cell.formula.as_deref() else {
                continue;
            };
            for (col, qualified) in local_reference_columns(formula) {
                if col == u32::MAX {
                    // More letters than any column has: a defined name such as
                    // `Data2023`, not a reference.
                    names += 1;
                    continue;
                }
                refs += 1;
                if col > eg_model::MAX_COL {
                    beyond_sheet += 1;
                } else if !qualified && col > limit {
                    beyond_used += 1;
                    if examples.len() < 8 {
                        examples.push(format!(
                            "sheet #{} (used {}..{}): {} references column {}",
                            sheet.id.0,
                            eg_model::col_to_letters(used.left as u32),
                            eg_model::col_to_letters(used.right as u32),
                            addr.to_a1(),
                            eg_model::col_to_letters(col)
                        ));
                    }
                }
            }
        }
    }

    println!("references examined:            {refs}");
    println!("skipped as defined names:       {names}");
    println!("beyond the sheet's last column: {beyond_sheet}");
    println!("more than {SLACK} columns past the used range: {beyond_used}");
    for e in &examples {
        println!("   {e}");
    }
    if beyond_sheet == 0 && beyond_used == 0 {
        println!("\nAll references land within the addressable sheet.");
    }
}

/// The 0-based column index of one to three ASCII letters, unchecked.
fn bijective_base26(letters: &str) -> u32 {
    let mut n: u32 = 0;
    for c in letters.bytes() {
        n = n * 26 + u32::from(c.to_ascii_uppercase() - b'A') + 1;
    }
    n - 1
}

/// Extract the column index of each A1 reference in a formula.
///
/// The flag reports whether the reference was sheet-qualified, since a
/// cross-sheet reference says nothing about *this* sheet's used range. A
/// reference is qualified exactly when `!` immediately precedes it, which holds
/// however the sheet name is written — quoted, or unquoted with spaces.
fn local_reference_columns(formula: &str) -> Vec<(u32, bool)> {
    let b = formula.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < b.len() {
        match b[i] {
            // Skip quoted spans; a sheet name may contain anything.
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            c if c.is_ascii_alphabetic() || c == b'$' => {
                let start = i;
                if b[i] == b'$' {
                    i += 1;
                }
                let letters_start = i;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let letters = &formula[letters_start..i];
                if i < b.len() && b[i] == b'$' {
                    i += 1;
                }
                let digits_start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }

                let is_ref = i > digits_start && !letters.is_empty();
                let is_call = i < b.len() && b[i] == b'(';
                if is_ref && !is_call {
                    let qualified = start > 0 && b[start - 1] == b'!';
                    match eg_model::letters_to_col(letters) {
                        Ok(col) => out.push((col as u32, qualified)),
                        // More than three letters cannot be a column at all, so
                        // this is a defined name such as `Data2023`.
                        Err(_) if letters.len() > 3 => out.push((u32::MAX, qualified)),
                        // One to three letters that still overflow — `XFE`,
                        // `ZZZ` — are exactly the signature this tool looks
                        // for, so they must be reported, not discarded as
                        // names. `letters_to_col` rejects them, so the index is
                        // recomputed here without the range check.
                        Err(_) => out.push((bijective_base26(letters), qualified)),
                    }
                } else if i == start {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    out
}
