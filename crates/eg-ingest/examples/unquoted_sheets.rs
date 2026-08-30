//! How often a formula names a sheet without the quotes Excel requires.
//!
//! A sheet name containing a space, a hyphen or other punctuation must be
//! written `'My Sheet'!A1`. calamine writes it bare, so `BP136-6-WORK DOC!A1`
//! reaches us as text no parser can read back: the reference appears to name a
//! sheet called `DOC`.
//!
//! Two outcomes, and the second is the dangerous one:
//!
//! - The fragment matches no sheet, so the reference is counted as broken. Bad,
//!   but visible.
//! - The fragment matches a *different real sheet*, and the dependency is
//!   silently attributed to the wrong place. Invisible, and wrong.
//!
//! Reports counts and sheet names only.

use eg_ingest::{load_with, LoadOptions};

fn needs_quotes(name: &str) -> bool {
    !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        || name.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let opts = LoadOptions {
        max_cells: None,
        ..Default::default()
    };
    let loaded = load_with(&path, &opts).unwrap();
    let wb = &loaded.workbook;

    let quoted: Vec<&str> = wb
        .sheets
        .iter()
        .map(|s| s.name.as_str())
        .filter(|n| needs_quotes(n))
        .collect();
    println!(
        "sheets whose name must be quoted in a formula: {}",
        quoted.len()
    );

    let mut bare = vec![0u64; quoted.len()];
    let mut formulas = 0u64;
    for sheet in &wb.sheets {
        for (_, cell) in sheet.iter() {
            let Some(f) = cell.formula.as_deref() else {
                continue;
            };
            formulas += 1;
            for (i, name) in quoted.iter().enumerate() {
                let mut from = 0;
                while let Some(p) = f[from..].find(name) {
                    let at = from + p;
                    let after = at + name.len();
                    let quoted_here = at > 0 && f.as_bytes()[at - 1] == b'\'';
                    if !quoted_here && f.as_bytes().get(after) == Some(&b'!') {
                        bare[i] += 1;
                    }
                    from = after.max(at + 1);
                }
            }
        }
    }

    println!("formula cells scanned: {formulas}\n");
    println!("{:<32} {:>12}  fragment", "sheet", "bare uses");
    let mut total = 0u64;
    for (i, name) in quoted.iter().enumerate() {
        if bare[i] == 0 {
            continue;
        }
        total += bare[i];
        // What a scanner sees instead: the trailing run of name characters.
        let fragment: String = name
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let collides = wb
            .sheets
            .iter()
            .any(|s| s.name.eq_ignore_ascii_case(&fragment));
        println!(
            "{name:<32} {:>12}  {fragment}{}",
            bare[i],
            if collides {
                "   <- COLLIDES with a real sheet"
            } else {
                ""
            }
        );
    }
    println!("\ntotal bare uses: {total}");
}
