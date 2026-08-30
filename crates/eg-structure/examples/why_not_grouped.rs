//! Diagnose why vertically adjacent formulas failed to share a group.
//!
//! Two adjacent cells that are the *same formula filled down* must produce the
//! same R1C1 shape. If they do not, the shape computation missed a reference.
//! If the formulas are not fill-related in the first place, they genuinely
//! differ and belong in separate groups.
//!
//! Distinguishing the two needs no formula text: take the upper formula, add one
//! to every relative row reference, and see whether it equals the lower one.
//!
//! Prints counts and A1 addresses only.

use eg_ingest::{load_with, LoadOptions};
use eg_model::{to_r1c1_shape, CellRef};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: why_not_grouped <workbook>");
        std::process::exit(2);
    });

    let opts = LoadOptions {
        max_cells: None,
        ..Default::default()
    };
    let loaded = load_with(&path, &opts).expect("load");

    let (mut pairs, mut same_shape, mut bug, mut genuine) = (0u64, 0u64, 0u64, 0u64);
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
                let sa = to_r1c1_shape(fa, CellRef::new(sheet.id, row, col));
                let sb = to_r1c1_shape(fb, CellRef::new(sheet.id, row + 1, col));
                if sa == sb {
                    same_shape += 1;
                } else if shift_rows(fa, 1) == fb {
                    // Fill-related, yet different shapes: the shape computation
                    // failed to rewrite something.
                    bug += 1;
                    if examples.len() < 6 {
                        examples.push(format!(
                            "{} vs {}",
                            loaded.workbook.cite(CellRef::new(sheet.id, row, col)),
                            CellRef::new(sheet.id, row + 1, col).to_a1()
                        ));
                    }
                } else {
                    genuine += 1;
                }
            }
        }
    }

    println!("adjacent formula pairs:       {pairs}");
    println!("  same shape (grouped):       {same_shape}");
    println!("  fill-related but NOT same shape (bug): {bug}");
    println!("  genuinely different:        {genuine}");
    for e in &examples {
        println!("     {e}");
    }
}

/// Add `delta` to every relative row reference in an A1 formula.
fn shift_rows(formula: &str, delta: i64) -> String {
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
                if b[i] == b'$' {
                    i += 1;
                }
                let ls = i;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let letters = i - ls;
                let row_abs = i < b.len() && b[i] == b'$';
                if row_abs {
                    i += 1;
                }
                let ds = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let is_call = i < b.len() && b[i] == b'(';
                let preceded = start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_');
                if (1..=3).contains(&letters) && i > ds && !row_abs && !is_call && !preceded {
                    let n: i64 = std::str::from_utf8(&b[ds..i]).unwrap().parse().unwrap_or(0);
                    out.extend_from_slice(&b[start..ds]);
                    out.extend_from_slice((n + delta).to_string().as_bytes());
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
    String::from_utf8(out).expect("ascii digits and original bytes only")
}
