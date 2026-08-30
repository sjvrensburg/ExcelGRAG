//! Why a reference resolved to no region.
//!
//! When the graph reports a reference as landing outside every region, this
//! says whether that is the workbook's doing or ours: it lists a sheet's
//! regions, probes named cells against them, and — given a third argument —
//! shows how each reference of one formula resolves.
//!
//! Usage: `why_unpopulated <workbook> [sheet] [cell]`
//!
//! Prints counts, sheet names and A1 addresses. Never cell contents.

use eg_ingest::{load_with, LoadOptions};
use eg_structure::detect_regions;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let want = std::env::args().nth(2).unwrap_or_default();
    let opts = LoadOptions {
        max_cells: None,
        ..Default::default()
    };
    let loaded = load_with(&path, &opts).unwrap();
    for sheet in &loaded.workbook.sheets {
        if !want.is_empty() && sheet.name != want {
            continue;
        }
        let regions = detect_regions(sheet);
        println!(
            "{} — {} regions, {} cells",
            sheet.name,
            regions.len(),
            sheet.len()
        );
        for r in &regions {
            println!(
                "   {:<24} {:?} {} cells",
                r.range.to_a1(),
                r.kind,
                r.cell_count
            );
        }
        if let Some(cell) = std::env::args().nth(3) {
            let at = eg_model::CellRef::parse_local(&cell, sheet.id).unwrap();
            if let Some(c) = sheet.get_ref(at) {
                if let Some(f) = c.formula.as_deref() {
                    println!("   {cell}: {} refs", eg_model::scan_references(f).len());
                    for r in eg_model::scan_references(f) {
                        let target = r.parsed.resolve(sheet.id);
                        let hit = regions.iter().find(|g| g.range.intersects(&target));
                        println!(
                            "     {:<20} sheet={:?} -> {} region={:?}",
                            r.text(f),
                            r.parsed.sheet_name,
                            target.to_a1(),
                            hit.map(|g| g.range.to_a1())
                        );
                    }
                }
            }
        }
        for probe in ["D21", "D22", "CT1", "AE2"] {
            let cell = eg_model::CellRef::parse_local(probe, sheet.id).unwrap();
            let hit = regions.iter().find(|r| r.range.contains(cell));
            println!(
                "   probe {probe:<5} populated={} region={:?}",
                sheet.is_populated(cell.row, cell.col),
                hit.map(|r| r.range.to_a1())
            );
        }
    }
}
