//! Look for regions where a header was probably missed.
//!
//! A tuning aid for [`eg_structure::detect_regions`], not a test. It flags
//! headerless regions whose first row is mostly text over a mostly non-text row
//! below — the shape a header has. Prints counts and A1 ranges only.
use eg_ingest::{load_with, LoadOptions};
use eg_model::ValueKind;
use eg_structure::{detect_regions, RegionKind};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let opts = LoadOptions { max_cells: None, ..Default::default() };
    let loaded = load_with(&path, &opts).unwrap();
    let (mut suspect, mut checked) = (0u64, 0u64);
    let mut examples = Vec::new();
    for sheet in &loaded.workbook.sheets {
        for r in detect_regions(sheet) {
            if r.header_rows > 0 || r.kind == RegionKind::Note || r.range.rows() < 3 {
                continue;
            }
            checked += 1;
            // First row mostly text while the row below is mostly not?
            let count = |row: u32, want_text: bool| {
                (r.range.left..=r.range.right)
                    .filter(|&c| {
                        let k = sheet.kind(row, c);
                        k != ValueKind::Empty && (k == ValueKind::Text) == want_text
                    })
                    .count()
            };
            let head_text = count(r.range.top, true);
            let head_any = head_text + count(r.range.top, false);
            let body_nontext = count(r.range.top + 1, false);
            let body_any = body_nontext + count(r.range.top + 1, true);
            // At least two populated cells: a single one is a title, not a
            // header row, and treating it as a miss just produces noise.
            if head_any >= 2 && body_any > 0
                && head_text as f64 / head_any as f64 > 0.6
                && body_nontext as f64 / body_any as f64 > 0.6
            {
                suspect += 1;
                if examples.len() < 10 {
                    examples.push(format!(
                        "{}  ({} cols, first row {}/{} text, next row {}/{} non-text)",
                        loaded.workbook.cite_range(r.range),
                        r.range.cols(), head_text, head_any, body_nontext, body_any
                    ));
                }
            }
        }
    }
    println!("headerless multi-row regions checked: {checked}");
    println!("probably-missed headers:              {suspect}");
    for e in examples { println!("   {e}") }
}
