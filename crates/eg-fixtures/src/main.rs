//! Generate the demonstration workbook.
//!
//! Everything this project can do was measured against a confidential workbook
//! that cannot be shared, which left the examples in the README undemonstrable
//! and the test suite resting on fixtures borrowed from calamine. This writes a
//! workbook of our own: invented data in the same shape, deterministic, and
//! free to commit.
//!
//! ```sh
//! cargo run --release -p eg-fixtures -- --rows 2000 --out tests/fixtures/demo
//! cargo run --release -p eg-fixtures -- --rows 400000 --out demo --formats xlsx
//! ```
//!
//! # Why it goes through LibreOffice
//!
//! `eg check` recomputes every formula and compares the answer with the value
//! the sheet has cached — so a fixture is only worth having if something *else*
//! computed those values. The generator therefore writes flat ODS with formulas
//! and no values at all, and LibreOffice fills them in when it converts. The
//! comparison is then between two independent implementations, which is the
//! whole point; a fixture whose values we supplied would agree with us by
//! construction and prove nothing.
//!
//! LibreOffice is not Excel, and where the two disagree this fixture follows
//! LibreOffice. It agrees with Excel on the parts that matter most here — it
//! renders `10.13+6.75` as `16.88` and `ROUND(2.675,2)` as `2.68`, the
//! shown-number regime `eg-eval` implements — but a formula that came back
//! disagreeing would need checking against Excel before being believed.
//!
//! # What it cannot do
//!
//! LibreOffice has no XLSB export filter, so the format this project exists for
//! cannot be generated. Format parity for XLSB still rests on the vendor
//! fixtures under `tests/fixtures/vendor`, which real Excel wrote. Anyone with
//! Excel can open the generated `.xlsx` and save it as `.xlsb` beside it, and
//! the parity test will pick it up.
//!
//! # Why all three outputs are committed
//!
//! `.xlsx`, `.ods` and `.xls` from one run are the same spreadsheet by
//! construction rather than by someone remembering to save it three times,
//! which is what makes them worth comparing to each other. The `.xls` is
//! committed *because* calamine reads it wrongly — see issue 9 in
//! `docs/upstream-issues.md` — since a defect a bug report only describes is
//! one nobody else can confirm.

mod book;
mod fods;

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "eg-fixtures",
    about = "Generate the demonstration workbook",
    long_about = None
)]
struct Args {
    /// Rows in the working table. The committed fixture is small enough to
    /// read by hand; a large one is for showing what the scale claims rest on.
    #[arg(long, default_value_t = 2000)]
    rows: usize,

    /// Where to write. Created if it does not exist.
    #[arg(long, default_value = "tests/fixtures/demo")]
    out: PathBuf,

    /// Base name for the generated files.
    #[arg(long, default_value = "impairment")]
    name: String,

    /// Formats to convert to, comma-separated. Anything LibreOffice can write:
    /// `xlsx`, `ods`, `xls`. Not `xlsb` — it has no export filter for it.
    #[arg(long, default_value = "xlsx,ods,xls")]
    formats: String,

    /// Write the flat ODS and stop, without calling LibreOffice.
    #[arg(long)]
    fods_only: bool,

    /// The LibreOffice binary.
    #[arg(long, default_value = "soffice")]
    soffice: String,
}

fn main() -> Result<(), String> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)
        .map_err(|e| format!("could not create {}: {e}", args.out.display()))?;

    let source = args.out.join(format!("{}.fods", args.name));
    let book = book::build(args.rows);
    std::fs::write(&source, book.to_fods())
        .map_err(|e| format!("could not write {}: {e}", source.display()))?;
    println!(
        "{} — {} rows, {} sheets",
        source.display(),
        args.rows,
        book.sheets.len()
    );

    if args.fods_only {
        return Ok(());
    }

    for format in args
        .formats
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        if format == "xlsb" {
            return Err("LibreOffice cannot write xlsb — see this crate's own docs".into());
        }
        convert(&args.soffice, &source, format, &args.out)?;
        let written = args.out.join(format!("{}.{}", args.name, format));
        let size = std::fs::metadata(&written).map(|m| m.len()).unwrap_or(0);
        println!("{} — {} KB", written.display(), size / 1024);
    }

    // The flat ODS stays on disk beside the converted files, and is gitignored:
    // it is the one artifact with no cached values in it, which makes it worth
    // reading while regenerating and worthless to commit, since this generator
    // reproduces it exactly.
    Ok(())
}

fn convert(soffice: &str, source: &Path, format: &str, out: &Path) -> Result<(), String> {
    let result = Command::new(soffice)
        .args(["--headless", "--convert-to", format, "--outdir"])
        .arg(out)
        .arg(source)
        .output()
        .map_err(|e| format!("could not run {soffice}: {e} — is LibreOffice installed?"))?;
    if !result.status.success() {
        return Err(format!(
            "{soffice} failed converting to {format}: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    // LibreOffice reports a missing export filter on stdout and still exits 0.
    let said = String::from_utf8_lossy(&result.stdout);
    if said.contains("no export filter") {
        return Err(format!("LibreOffice has no export filter for {format}"));
    }
    Ok(())
}
