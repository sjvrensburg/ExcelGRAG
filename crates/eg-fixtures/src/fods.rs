//! A minimal writer for flat ODF spreadsheets (`.fods`).
//!
//! Flat ODS is a single XML file, which is why it is the authoring format here:
//! no zip, no manifest, no dependency, and a diff of the generator's output is
//! readable.
//!
//! The point of writing ODF rather than xlsx directly is what it lets us leave
//! *out*. A formula cell here carries no value, and LibreOffice must therefore
//! compute one when it loads the file — so the cached values in the converted
//! workbook come from an engine that is not ours. Writing xlsx directly would
//! mean stating those values ourselves, and `eg check`, whose whole job is to
//! compare our arithmetic against the sheet's, would be grading its own
//! homework.
//!
//! Formulas are given in ODF syntax, which is not Excel's: references are
//! bracketed (`[.H2]`, `[Rates.$A$3]`), arguments are separated by `;`, and the
//! boolean literals are calls (`FALSE()`). The helpers below build the
//! reference forms; the rest is close enough to Excel to write by hand.

use std::fmt::Write as _;

/// One cell of the sheet.
pub enum Cell {
    /// Nothing here. A sheet's blank runs are what region detection reads, so
    /// these are load-bearing.
    Empty,
    Number(f64),
    Text(String),
    /// ODF formula source, without the leading `of:=`.
    Formula(String),
}

impl Cell {
    pub fn text(s: impl Into<String>) -> Cell {
        Cell::Text(s.into())
    }

    pub fn formula(s: impl Into<String>) -> Cell {
        Cell::Formula(s.into())
    }
}

/// One sheet.
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<Cell>>,
}

impl Sheet {
    pub fn new(name: impl Into<String>) -> Sheet {
        Sheet {
            name: name.into(),
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, row: Vec<Cell>) {
        self.rows.push(row);
    }

    /// A row with nothing in it — the gap region detection splits on.
    pub fn blank(&mut self) {
        self.rows.push(Vec::new());
    }
}

/// A workbook, as the generator describes it.
#[derive(Default)]
pub struct Book {
    pub sheets: Vec<Sheet>,
    /// `(name, scope, target)`, where a `None` scope is workbook-wide and the
    /// target is an ODF cell-range address (`$Rates.$B$12`).
    pub names: Vec<(String, Option<String>, String)>,
}

impl Book {
    pub fn push(&mut self, sheet: Sheet) {
        self.sheets.push(sheet);
    }

    pub fn name(&mut self, name: &str, scope: Option<&str>, target: &str) {
        self.names.push((
            name.to_string(),
            scope.map(str::to_string),
            target.to_string(),
        ));
    }

    pub fn to_fods(&self) -> String {
        let mut out = String::with_capacity(1 << 20);
        out.push_str(HEADER);
        for sheet in &self.sheets {
            let _ = write!(out, "<table:table table:name=\"{}\">", esc(&sheet.name));
            // One column declaration is enough: nothing here depends on widths,
            // and a sheet with none loads with Calc's defaults.
            out.push_str("<table:table-column table:number-columns-repeated=\"64\"/>");
            for row in &sheet.rows {
                if row.is_empty() {
                    out.push_str("<table:table-row><table:table-cell/></table:table-row>");
                    continue;
                }
                out.push_str("<table:table-row>");
                for cell in row {
                    write_cell(&mut out, cell);
                }
                out.push_str("</table:table-row>");
            }
            out.push_str("</table:table>");
        }
        if !self.names.is_empty() {
            out.push_str("<table:named-expressions>");
            for (name, scope, target) in &self.names {
                // `base-cell-address` is what a relative name would be relative
                // to. Every name here is absolute, so it only has to be a cell
                // that exists.
                let base = match scope {
                    Some(sheet) => format!("${}.$A$1", esc(sheet)),
                    None => format!("${}.$A$1", esc(&self.sheets[0].name)),
                };
                let _ = write!(
                    out,
                    "<table:named-range table:name=\"{}\" table:base-cell-address=\"{}\" \
                     table:cell-range-address=\"{}\"/>",
                    esc(name),
                    base,
                    esc(target)
                );
            }
            out.push_str("</table:named-expressions>");
        }
        out.push_str(FOOTER);
        out
    }
}

fn write_cell(out: &mut String, cell: &Cell) {
    match cell {
        Cell::Empty => out.push_str("<table:table-cell/>"),
        Cell::Number(n) => {
            // `{}` rather than a fixed precision: these are the inputs the
            // whole fixture is derived from, and a rounded input would make
            // every total downstream of it disagree by a rounding error.
            let _ = write!(
                out,
                "<table:table-cell office:value-type=\"float\" office:value=\"{n}\"/>"
            );
        }
        Cell::Text(s) => {
            let _ = write!(
                out,
                "<table:table-cell office:value-type=\"string\"><text:p>{}</text:p>\
                 </table:table-cell>",
                esc(s)
            );
        }
        Cell::Formula(f) => {
            // No `office:value`: that absence is what makes LibreOffice
            // calculate rather than believe us.
            let _ = write!(out, "<table:table-cell table:formula=\"of:={}\"/>", esc(f));
        }
    }
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

// ---- reference helpers -----------------------------------------------------

/// A cell on the same sheet: `[.H2]`.
pub fn here(a1: &str) -> String {
    format!("[.{a1}]")
}

/// A range on another sheet: `[Rates.$A$3:.$B$6]`.
pub fn on_range(sheet: &str, from: &str, to: &str) -> String {
    format!("[{sheet}.{from}:.{to}]")
}

/// The same cell across a span of sheets — a 3-D reference,
/// `[Jan.B2:Mar.B2]`, which becomes `Jan:Mar!B2` in Excel's syntax.
pub fn across(first: &str, last: &str, a1: &str) -> String {
    format!("[{first}.{a1}:{last}.{a1}]")
}

/// A quoted string literal inside a formula.
pub fn lit(s: &str) -> String {
    format!("\"{s}\"")
}

const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document
 xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
 xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2"
 office:version="1.3"
 office:mimetype="application/vnd.oasis.opendocument.spreadsheet">
<office:body><office:spreadsheet>
"#;

const FOOTER: &str = "</office:spreadsheet></office:body></office:document>\n";
