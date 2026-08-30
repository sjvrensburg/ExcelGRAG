//! Diagnose how many XLSB formula cells calamine decodes versus how many it drops.
//!
//! `Xlsb::worksheet_formula` discards any cell whose decoded formula text is
//! empty. This walks the same cell reader directly so the two populations can be
//! counted separately, which is the difference between "this workbook has few
//! formulas" and "we are losing most of them".
//!
//! Prints counts and A1 addresses only — never cell contents or formula text.

use calamine::{open_workbook, Reader, Xlsb};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: xlsb_formula_probe <workbook.xlsb> [sheet-name]");
        std::process::exit(2);
    });
    let only = std::env::args().nth(2);

    let mut wb: Xlsb<_> = match open_workbook(&path) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("open failed: {e}");
            std::process::exit(1);
        }
    };

    let names = wb.sheet_names();
    let mut grand_ok = 0usize;
    let mut grand_empty = 0usize;

    for (i, name) in names.iter().enumerate() {
        if only.as_deref().is_some_and(|s| s != name) {
            continue;
        }
        let mut reader = match wb.worksheet_cells_reader(name) {
            Ok(r) => r,
            Err(e) => {
                println!("#{i}: cells_reader failed: {e}");
                continue;
            }
        };

        let (mut ok, mut empty) = (0usize, 0usize);
        let mut first_empty = Vec::new();
        let mut aborted = None;
        loop {
            match reader.next_formula() {
                Ok(Some(cell)) => {
                    if cell.get_value().is_empty() {
                        empty += 1;
                        if first_empty.len() < 5 {
                            let (r, c) = cell.get_position();
                            first_empty.push(format!("{}{}", eg_model::col_to_letters(c), r + 1));
                        }
                    } else {
                        ok += 1;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    aborted = Some(e.to_string());
                    break;
                }
            }
        }

        grand_ok += ok;
        grand_empty += empty;
        let total = ok + empty;
        if total > 0 || aborted.is_some() {
            println!(
                "#{i}: decoded={ok} dropped_empty={empty} total={total} ({:.1}% recovered)",
                if total > 0 {
                    100.0 * ok as f64 / total as f64
                } else {
                    0.0
                }
            );
            if !first_empty.is_empty() {
                println!("      first dropped: {}", first_empty.join(", "));
            }
            if let Some(e) = aborted {
                println!("      ABORTED: {e}");
            }
        }
    }

    let total = grand_ok + grand_empty;
    println!();
    println!("TOTAL decoded={grand_ok} dropped_empty={grand_empty} total={total}");
    if total > 0 {
        println!("recovery: {:.1}%", 100.0 * grand_ok as f64 / total as f64);
    }
}
