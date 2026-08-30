//! Verify the invariants region detection must satisfy.
//!
//! Output that looks plausible is not evidence of correctness. Three properties
//! must hold on every sheet, and each catches a different class of mistake:
//!
//! - **Coverage.** Every populated cell belongs to a region. A cell in no region
//!   is unreachable: nothing will ever retrieve or cite it.
//! - **Disjointness.** No cell belongs to two regions, or a retrieved answer
//!   could cite the same value under two different headers.
//! - **Header sanity.** A region called a table should have headers that are
//!   mostly non-empty and mostly distinct, since duplicated or blank headers
//!   mean the header row was misidentified.
//!
//! Prints counts and A1 addresses only.

use std::collections::HashSet;

use eg_ingest::{load_with, LoadOptions};
use eg_structure::{detect_regions, RegionKind};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: check_regions <workbook>");
        std::process::exit(2);
    });

    let opts = LoadOptions {
        max_cells: None,
        ..Default::default()
    };
    let loaded = load_with(&path, &opts).expect("load");

    let (mut uncovered, mut overlapping, mut cells) = (0u64, 0u64, 0u64);
    let mut uncovered_examples = Vec::new();
    let mut overlap_examples = Vec::new();

    let (mut tables, mut blank_headers, mut dup_headers, mut total_headers) =
        (0u64, 0u64, 0u64, 0u64);
    let mut suspect_tables = Vec::new();

    for sheet in &loaded.workbook.sheets {
        let regions = detect_regions(sheet);

        // Disjointness, checked pairwise on ranges rather than per cell.
        for (i, a) in regions.iter().enumerate() {
            for b in &regions[i + 1..] {
                if a.range.intersects(&b.range) {
                    overlapping += 1;
                    if overlap_examples.len() < 5 {
                        overlap_examples.push(format!(
                            "{} overlaps {}",
                            loaded.workbook.cite_range(a.range),
                            b.range.to_a1()
                        ));
                    }
                }
            }
        }

        // Coverage.
        for (addr, _) in sheet.iter() {
            cells += 1;
            if !regions.iter().any(|r| r.range.contains(addr)) {
                uncovered += 1;
                if uncovered_examples.len() < 5 {
                    uncovered_examples.push(loaded.workbook.cite(addr));
                }
            }
        }

        // Header sanity.
        for r in regions.iter().filter(|r| r.kind == RegionKind::Table) {
            tables += 1;
            let blanks = r.headers.iter().filter(|h| h.is_empty()).count() as u64;
            let distinct: HashSet<&String> = r.headers.iter().collect();
            let dups = r.headers.len() as u64 - distinct.len() as u64;
            blank_headers += blanks;
            dup_headers += dups;
            total_headers += r.headers.len() as u64;

            let n = r.headers.len().max(1) as f64;
            let bad = (blanks as f64 / n) > 0.5 || (dups as f64 / n) > 0.5;
            if bad && suspect_tables.len() < 8 {
                suspect_tables.push(format!(
                    "{} ({} headers, {blanks} blank, {dups} duplicated)",
                    loaded.workbook.cite_range(r.range),
                    r.headers.len()
                ));
            }
        }
    }

    println!("populated cells:      {cells}");
    println!("cells in no region:   {uncovered}");
    for e in &uncovered_examples {
        println!("   {e}");
    }
    println!("overlapping pairs:    {overlapping}");
    for e in &overlap_examples {
        println!("   {e}");
    }
    println!();
    println!("tables:               {tables}");
    println!("header cells:         {total_headers}");
    println!("  blank:              {blank_headers}");
    println!("  duplicated:         {dup_headers}");
    println!(
        "tables with mostly blank or duplicated headers: {}",
        suspect_tables.len()
    );
    for e in &suspect_tables {
        println!("   {e}");
    }

    let ok = uncovered == 0 && overlapping == 0;
    println!();
    println!(
        "{}",
        if ok {
            "coverage and disjointness hold"
        } else {
            "INVARIANT VIOLATED"
        }
    );
    if !ok {
        std::process::exit(1);
    }
}
