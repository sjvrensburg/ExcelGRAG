//! Check the cells the shared-formula fix is responsible for, and only those.
//!
//! A whole-sheet check is muddied by columns that genuinely change formula at
//! subtotals and section breaks. Shared-formula *members* admit no such
//! ambiguity: by construction every cell in a group is the same formula shifted
//! by its offset, so two vertically adjacent members must differ by exactly one
//! row. Any deviation is a decoding bug.
//!
//! Members are identified by running the single-pass reader, which yields an
//! empty string for exactly those cells, and the resolved text comes from the
//! two-pass reader.
//!
//! Prints counts and A1 addresses only — never formula text.

use std::collections::{HashMap, HashSet};

use calamine::{open_workbook, Reader, Xlsb};

#[path = "shared_formula_check.rs"]
mod check;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: shared_group_check <workbook.xlsb>");
        std::process::exit(2);
    });

    let mut wb: Xlsb<_> = match open_workbook(&path) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("open failed: {e}");
            std::process::exit(1);
        }
    };

    let (mut members_total, mut pairs, mut ok, mut bad, mut unresolved) = (0, 0usize, 0, 0, 0);
    let mut examples: Vec<String> = Vec::new();

    for name in wb.sheet_names() {
        // Pass 1: the single-pass reader returns an empty formula for exactly
        // the shared and array formula members.
        let mut members: HashSet<(u32, u32)> = HashSet::new();
        if let Ok(mut r) = wb.worksheet_cells_reader(&name) {
            while let Ok(Some(cell)) = r.next_formula() {
                if cell.get_value().is_empty() {
                    members.insert(cell.get_position());
                }
            }
        }
        if members.is_empty() {
            continue;
        }
        members_total += members.len();

        // Pass 2: the two-pass reader resolves them.
        let resolved: HashMap<(u32, u32), String> = match wb.worksheet_cells_reader(&name) {
            Ok(mut r) => match r.formulas() {
                Ok(cells) => cells
                    .into_iter()
                    .map(|c| (c.get_position(), c.get_value().clone()))
                    .collect(),
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        for &(row, col) in &members {
            if !resolved.contains_key(&(row, col)) {
                unresolved += 1;
                continue;
            }
            // Only compare against the cell above when it is also a member of a
            // group, so group boundaries are not counted as disagreements.
            if row == 0 || !members.contains(&(row - 1, col)) {
                continue;
            }
            let (Some(above), Some(here)) =
                (resolved.get(&(row - 1, col)), resolved.get(&(row, col)))
            else {
                continue;
            };
            pairs += 1;
            if check::shift_relative_rows(above, 1) == *here {
                ok += 1;
            } else {
                bad += 1;
                if examples.len() < 8 {
                    examples.push(format!(
                        "{}!{}",
                        name,
                        eg_model::CellRef::new(eg_model::SheetId(0), row, col as u16).to_a1()
                    ));
                }
            }
        }
    }

    println!("shared/array formula members found:  {members_total}");
    println!("  left unresolved by the two-pass read: {unresolved}");
    println!("adjacent member pairs compared:      {pairs}");
    println!("  advance by exactly one row:        {ok}");
    println!("  WRONG:                             {bad}");
    if pairs > 0 {
        println!("  correctness: {:.4}%", 100.0 * ok as f64 / pairs as f64);
    }
    if !examples.is_empty() {
        println!("  first wrong cells: {}", examples.join(", "));
    }
}
