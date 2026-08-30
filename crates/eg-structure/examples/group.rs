//! Group a workbook's formulas by shape and report how far it compresses.
//!
//! This is the measurement behind the two-tier graph: the persisted graph is
//! built over groups, not cells, so the ratio here is roughly how much smaller
//! the graph is than the sheet.
//!
//! Prints counts and A1 ranges only. Formula text and shapes are withheld unless
//! `--show-formulas` is passed, since they are workbook content.

use std::time::Instant;

use eg_ingest::{load_with, LoadOptions};
use eg_structure::{find_shape_exceptions, group_formulas, GroupingStats};

fn main() {
    let mut path = None;
    let mut show = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--show-formulas" => show = true,
            other => path = Some(other.to_string()),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: group [--show-formulas] <workbook>");
        std::process::exit(2);
    };

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

    let started = Instant::now();
    let mut total = GroupingStats::default();
    let mut all_exceptions = 0usize;
    let mut examples: Vec<String> = Vec::new();
    let mut biggest: Option<(String, u64)> = None;
    // Size histogram, by power-of-ten bucket, plus how many singletons sit
    // directly below another formula (a neighbour they failed to group with).
    let mut buckets = [0u64; 8];
    let mut singleton_with_formula_above = 0u64;
    let mut per_sheet: Vec<(String, u64, u64)> = Vec::new();

    for sheet in &loaded.workbook.sheets {
        let (groups, stats) = group_formulas(sheet);
        total.formula_cells += stats.formula_cells;
        total.groups += stats.groups;
        total.singletons += stats.singletons;
        total.largest_group = total.largest_group.max(stats.largest_group);

        for g in &groups {
            if biggest.as_ref().is_none_or(|(_, n)| g.cell_count > *n) {
                biggest = Some((loaded.workbook.cite_range(g.range), g.cell_count));
            }
            let b = (g.cell_count as f64).log10().floor() as usize;
            buckets[b.min(7)] += 1;
            if g.is_singleton() {
                let a = g.range.top;
                if a > 0
                    && sheet
                        .get(a - 1, g.range.left)
                        .is_some_and(|c| c.is_formula())
                {
                    singleton_with_formula_above += 1;
                }
            }
        }
        if stats.groups > 0 {
            per_sheet.push((sheet.name.clone(), stats.formula_cells, stats.groups));
        }

        let exceptions = find_shape_exceptions(sheet);
        all_exceptions += exceptions.len();
        for e in exceptions.iter().take(3) {
            if examples.len() < 10 {
                examples.push(if show {
                    format!(
                        "{}  has {:?}, neighbours have {:?}",
                        loaded.workbook.cite(e.cell),
                        e.formula,
                        e.expected_shape
                    )
                } else {
                    loaded.workbook.cite(e.cell)
                });
            }
        }
    }
    let elapsed = started.elapsed();

    println!("formula cells:   {}", total.formula_cells);
    println!("formula groups:  {}", total.groups);
    println!("  of which one-off: {}", total.singletons);
    println!("compression:     {:.1}x", total.compression());
    if let Some((range, n)) = &biggest {
        println!("largest group:   {n} cells at {range}");
    }
    println!("grouping time:   {:.2}s", elapsed.as_secs_f64());
    println!();
    println!("group size distribution:");
    for (i, n) in buckets.iter().enumerate() {
        if *n > 0 {
            let lo = 10u64.pow(i as u32);
            println!("   {:>9}+ cells: {n} groups", lo);
        }
    }
    println!(
        "one-off formulas sitting directly below another formula: {singleton_with_formula_above}"
    );
    println!();
    println!("per sheet (cells -> groups):");
    per_sheet.sort_by_key(|(_, c, _)| std::cmp::Reverse(*c));
    for (name, cells, groups) in per_sheet.iter().take(8) {
        println!("   {name:32} {cells:>9} -> {groups:>7}");
    }
    println!();
    println!("pattern breaks:  {all_exceptions}");
    for e in &examples {
        println!("   {e}");
    }
    if all_exceptions > 0 && !show {
        println!("\n   (pass --show-formulas to see the formulas; withheld by default)");
    }
}
