//! Verify that filled-down formula columns decode correctly, row by row.
//!
//! Recovering the right *number* of formulas from a shared-formula group proves
//! nothing on its own: a mis-applied anchor would yield the same count with
//! every row pointing at the wrong cells. The invariant that actually matters is
//! that consecutive rows of a filled-down column differ only by their relative
//! row references advancing by one.
//!
//! So for each vertically adjacent pair of formula cells in a column, this takes
//! the upper formula, adds one to every *relative* row reference, and requires
//! the result to equal the lower formula exactly.
//!
//! Prints counts and A1 addresses only — never formula text or cell contents.

#![allow(dead_code)] // also included as a module by shared_group_check

use eg_ingest::load_with;
use eg_ingest::LoadOptions;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: shared_formula_check <workbook>");
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

    let (mut matched, mut differed, mut pairs) = (0usize, 0usize, 0usize);
    let mut examples: Vec<String> = Vec::new();

    for sheet in &loaded.workbook.sheets {
        let Some(used) = sheet.used_range() else {
            continue;
        };
        for col in used.left..=used.right {
            for row in used.top..used.bottom {
                let (Some(a), Some(b)) = (sheet.get(row, col), sheet.get(row + 1, col)) else {
                    continue;
                };
                let (Some(fa), Some(fb)) = (a.formula.as_deref(), b.formula.as_deref()) else {
                    continue;
                };
                pairs += 1;
                if shift_relative_rows(fa, 1) == fb {
                    matched += 1;
                } else {
                    differed += 1;
                    if examples.len() < 8 {
                        examples.push(
                            eg_model::CellRef::new(sheet.id, row + 1, col).to_a1(),
                        );
                    }
                }
            }
        }
    }

    println!("vertically adjacent formula pairs: {pairs}");
    println!("  advance by exactly one row: {matched}");
    println!("  differ otherwise:           {differed}");
    if pairs > 0 {
        println!("  agreement: {:.1}%", 100.0 * matched as f64 / pairs as f64);
    }
    if !examples.is_empty() {
        println!("  first differing cells: {}", examples.join(", "));
    }
    println!();
    println!(
        "Note: honest disagreement is expected wherever a column genuinely\n\
         changes formula, such as a subtotal row or the first row under a header."
    );
}

/// Add `delta` to every relative row reference in an A1 formula.
///
/// A row is relative when no `$` immediately precedes its digits. Text inside
/// string literals and quoted sheet names is left alone.
pub fn shift_relative_rows(formula: &str, delta: i64) -> String {
    let b = formula.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len() + 8);
    let mut i = 0;

    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'') => {
                out.push(q);
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            out.extend_from_slice(&[q, q]);
                            i += 2;
                            continue;
                        }
                        out.push(q);
                        i += 1;
                        break;
                    }
                    out.push(b[i]);
                    i += 1;
                }
            }
            c if c.is_ascii_alphabetic() || c == b'$' => {
                let start = i;
                // Optional $ then column letters.
                if b[i] == b'$' {
                    i += 1;
                }
                let letters_start = i;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let has_letters = i > letters_start && i - letters_start <= 3;
                // Optional $ then row digits.
                let row_abs = i < b.len() && b[i] == b'$';
                if row_abs {
                    i += 1;
                }
                let digits_start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let has_digits = i > digits_start;

                // A trailing '(' means this was a function name, not a reference.
                let is_call = i < b.len() && b[i] == b'(';

                if has_letters && has_digits && !row_abs && !is_call {
                    let row: i64 = std::str::from_utf8(&b[digits_start..i])
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                    out.extend_from_slice(&b[start..digits_start]);
                    out.extend_from_slice((row + delta).to_string().as_bytes());
                } else {
                    out.extend_from_slice(&b[start..i]);
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).expect("only original bytes and ASCII digits are emitted")
}
