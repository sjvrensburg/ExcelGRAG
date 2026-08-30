//! Detect regions across a workbook and report what was found.
//!
//! Region detection is heuristic, so this exists to make its output inspectable
//! on real sheets rather than only on fixtures.
//!
//! Prints counts, A1 ranges and region kinds. Header text is workbook content
//! and is withheld unless `--show-headers` is passed.

use std::time::Instant;

use eg_ingest::{load_with, LoadOptions};
use eg_structure::{detect_regions, RegionKind, RegionSource};

fn main() {
    let mut path = None;
    let mut show = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--show-headers" => show = true,
            other => path = Some(other.to_string()),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: regions [--show-headers] <workbook>");
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
    let (mut tables, mut blocks, mut notes, mut declared) = (0u64, 0u64, 0u64, 0u64);
    let mut with_headers = 0u64;
    let mut with_titles = 0u64;
    let mut total = 0u64;
    let mut per_sheet = Vec::new();
    let mut largest: Vec<(u64, String, RegionKind, usize)> = Vec::new();

    for sheet in &loaded.workbook.sheets {
        let regions = detect_regions(sheet);
        total += regions.len() as u64;
        for r in &regions {
            match r.kind {
                RegionKind::Table => tables += 1,
                RegionKind::Block => blocks += 1,
                RegionKind::Note => notes += 1,
            }
            if r.source == RegionSource::Declared {
                declared += 1;
            }
            if r.has_header() {
                with_headers += 1;
            }
            if r.title.is_some() {
                with_titles += 1;
            }
            largest.push((
                r.cell_count,
                loaded.workbook.cite_range(r.range),
                r.kind,
                r.headers.len(),
            ));
        }
        per_sheet.push((sheet.name.clone(), sheet.len(), regions.len()));
    }
    let elapsed = started.elapsed();

    println!("regions:        {total}");
    println!("  tables:       {tables}");
    println!("  blocks:       {blocks}");
    println!("  notes:        {notes}");
    println!("  declared:     {declared}");
    println!("  with headers: {with_headers}");
    println!("  with titles:  {with_titles}");
    println!("detection time: {:.2}s", elapsed.as_secs_f64());

    println!();
    println!("per sheet (cells -> regions):");
    per_sheet.sort_by_key(|(_, c, _)| std::cmp::Reverse(*c));
    for (name, cells, regions) in per_sheet.iter().take(10) {
        println!("   {name:32} {cells:>9} -> {regions:>6}");
    }

    println!();
    println!("largest regions:");
    largest.sort_by_key(|(n, ..)| std::cmp::Reverse(*n));
    for (n, range, kind, headers) in largest.iter().take(8) {
        println!("   {n:>9} cells  {kind:?}  {headers} headers  {range}");
    }

    if show {
        println!();
        println!("sample titles and headers:");
        for sheet in &loaded.workbook.sheets {
            for r in detect_regions(sheet)
                .iter()
                .filter(|r| !r.headers.is_empty() || r.title.is_some())
            {
                println!(
                    "   {}  title={:?}  {:?}",
                    loaded.workbook.cite_range(r.range),
                    r.title,
                    &r.headers[..r.headers.len().min(6)]
                );
            }
        }
    }
}
