//! Loading workbooks from disk into the [`eg_model`] representation.
//!
//! One entry point, [`load`], handles every supported format. Callers should not
//! branch on format themselves — but they *do* need to know what a given format
//! could not provide, which is what [`Capabilities`] reports.
//!
//! # Format capability differences
//!
//! calamine reads values and formulas from all five formats, but the richer
//! metadata is not uniformly available, and none of it can be invented:
//!
//! | Signal          | xlsx/xlsm | xlsb | xls | ods |
//! |-----------------|-----------|------|-----|-----|
//! | values          | yes       | yes  | yes | yes |
//! | formulas        | yes       | yes  | yes | yes |
//! | merged cells    | yes       | no   | yes | no  |
//! | declared tables | yes       | no   | no  | no  |
//! | cell styling    | no        | no   | no  | no  |
//!
//! calamine exposes no cell styling at all — no bold, fill, or border — for any
//! format. Structural analysis therefore cannot depend on presentation, and is
//! built on format-portable signals instead: value kinds, blank runs, and
//! formula-shape homogeneity. Merges and tables, where available, are used as
//! *additional* evidence, never as a prerequisite. This is what keeps XLSB —
//! the reason the project exists — a first-class input rather than a degraded one.

mod convert;
mod odf;

use std::path::{Path, PathBuf};

use calamine::{Data, Reader, Sheets};
use eg_model::{
    Cell, DefinedName, ExcelTable, RangeRef, Sheet, SheetId, Visibility, Workbook, WorkbookFormat,
};

pub use convert::{convert_error, convert_value};

/// Errors raised while loading a workbook.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported file format: {0:?} (expected xlsx, xlsm, xlsb, xls or ods)")]
    UnsupportedFormat(PathBuf),
    #[error("failed to open workbook {path}: {message}")]
    Open { path: PathBuf, message: String },
    // `eg` and `eg-mcp` both call in with no limit, so this is only reachable
    // through a caller that set `LoadOptions::max_cells` itself — there is no
    // CLI flag to name here, so the message stays limited to what happened.
    #[error("stopped at {found} populated cells, over the configured limit of {limit}")]
    TooLarge { found: u64, limit: u64 },
    #[error("sheet count {0} exceeds the addressable maximum of 65536")]
    TooManySheets(usize),
}

/// What a given source format was actually able to supply.
///
/// Consumers use this to distinguish "this workbook has no merged cells" from
/// "this format cannot tell us about merged cells", which are very different
/// claims to make to an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub formulas: bool,
    pub merged_cells: bool,
    pub declared_tables: bool,
    pub cell_styling: bool,
    pub writable: bool,
    /// Whether `Workbook::external_links` is actually populated. calamine
    /// exposes no external-link data for any format today, so this is
    /// `false` everywhere — a gap that should be said out loud rather than
    /// left for a caller to discover as an always-empty list that looks like
    /// "this workbook links to nothing."
    pub external_links: bool,
}

impl Capabilities {
    pub fn for_format(format: WorkbookFormat) -> Self {
        use WorkbookFormat::*;
        Self {
            formulas: true,
            merged_cells: matches!(format, Xlsx | Xlsm | Xls),
            declared_tables: matches!(format, Xlsx | Xlsm),
            // calamine exposes no styling for any format.
            cell_styling: false,
            writable: format.is_writable(),
            external_links: false,
        }
    }

    /// Human-readable notes on what is missing, for inclusion in tool output.
    pub fn limitations(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.merged_cells {
            out.push("merged cells are not readable in this format");
        }
        if !self.declared_tables {
            out.push("declared Excel tables are not readable in this format");
        }
        if !self.cell_styling {
            out.push("cell styling (bold/fill/borders) is not available");
        }
        if !self.writable {
            out.push("this format cannot be written back; edits stay in memory");
        }
        if !self.external_links {
            out.push("links to other workbooks are not readable; external_links is always empty");
        }
        out
    }
}

/// Tuning knobs for a load.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Refuse workbooks with more populated cells than this, rather than
    /// exhausting memory on a file nobody meant to open.
    pub max_cells: Option<u64>,
    /// Load sheets Excel marks hidden or very hidden. They are often scratch
    /// space or lookup tables, so they are included by default but flagged.
    pub include_hidden_sheets: bool,
    /// Read formulas. Disabling roughly halves load time when only values matter.
    pub read_formulas: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            max_cells: Some(20_000_000),
            include_hidden_sheets: true,
            read_formulas: true,
        }
    }
}

/// A loaded workbook plus what the format could tell us about it.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub workbook: Workbook,
    pub capabilities: Capabilities,
    /// Non-fatal problems encountered, e.g. a sheet that failed to parse.
    pub warnings: Vec<String>,
}

/// Load a workbook from disk with default options.
pub fn load(path: impl AsRef<Path>) -> Result<Loaded, IngestError> {
    load_with(path, &LoadOptions::default())
}

/// Load a workbook from disk.
pub fn load_with(path: impl AsRef<Path>, opts: &LoadOptions) -> Result<Loaded, IngestError> {
    let path = path.as_ref();
    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(WorkbookFormat::from_extension)
        .ok_or_else(|| IngestError::UnsupportedFormat(path.to_path_buf()))?;

    // Hash and parse the same open file. Reopening the path here allowed a
    // concurrent save/sync rename to put workbook B under workbook A's hash.
    use std::io::{BufReader, Seek};
    let mut source = std::fs::File::open(path).map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let content_hash = hash_reader(&mut source, path)?;
    source.rewind().map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = BufReader::new(source);
    let opened = match format {
        WorkbookFormat::Xls => calamine::open_workbook_from_rs::<calamine::Xls<_>, _>(reader)
            .map(calamine::Sheets::Xls)
            .map_err(calamine::Error::Xls),
        WorkbookFormat::Xlsx | WorkbookFormat::Xlsm => {
            calamine::open_workbook_from_rs::<calamine::Xlsx<_>, _>(reader)
                .map(calamine::Sheets::Xlsx)
                .map_err(calamine::Error::Xlsx)
        }
        WorkbookFormat::Xlsb => calamine::open_workbook_from_rs::<calamine::Xlsb<_>, _>(reader)
            .map(calamine::Sheets::Xlsb)
            .map_err(calamine::Error::Xlsb),
        WorkbookFormat::Ods => calamine::open_workbook_from_rs::<calamine::Ods<_>, _>(reader)
            .map(calamine::Sheets::Ods)
            .map_err(calamine::Error::Ods),
    };
    let mut sheets = opened.map_err(|e| IngestError::Open {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut warnings = Vec::new();
    let capabilities = Capabilities::for_format(format);

    let metadata: Vec<(String, Visibility)> = sheets
        .sheets_metadata()
        .iter()
        .map(|s| (s.name.clone(), convert::visibility(s.visible)))
        .collect();

    if metadata.len() > u16::MAX as usize + 1 {
        return Err(IngestError::TooManySheets(metadata.len()));
    }

    let defined_names = convert_defined_names(
        sheets.defined_names(),
        sheets.defined_name_scopes(),
        format,
        &mut warnings,
    );

    // Merges and tables come from format-specific APIs, so they are gathered up
    // front while we can still match on the concrete reader.
    let merges = collect_merges(&mut sheets, &metadata, &mut warnings);
    let tables = collect_tables(&mut sheets, &metadata, &mut warnings);

    let mut loaded_sheets = Vec::with_capacity(metadata.len());
    let mut total_cells: u64 = 0;

    for (index, (name, visibility)) in metadata.iter().enumerate() {
        let id = SheetId(index as u16);

        if !opts.include_hidden_sheets && !visibility.is_visible() {
            // Keep the tab in place so sheet ids stay aligned with workbook order.
            let mut sheet = Sheet::new(id, name.clone());
            sheet.visibility = *visibility;
            loaded_sheets.push(sheet);
            continue;
        }

        let values = match sheets.worksheet_range(name) {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("skipped sheet {name:?}: {e}"));
                let mut sheet = Sheet::new(id, name.clone());
                sheet.visibility = *visibility;
                loaded_sheets.push(sheet);
                continue;
            }
        };

        let mut sheet = Sheet::new(id, name.clone());
        sheet.visibility = *visibility;

        // calamine reports used-cell coordinates *relative to the range origin*,
        // not as absolute sheet positions, so the range start must be added
        // back. Omitting this silently misplaces every cell on any sheet whose
        // content does not begin at A1 — which is most real sheets.
        let (value_row0, value_col0) = values.start().unwrap_or((0, 0));
        let mut corrupt_coords = 0u32;
        for (row, col, value) in values.used_cells() {
            let (Some(row), Some(col)) = (
                fit_row(row + value_row0 as usize),
                fit_col(col + value_col0 as usize),
            ) else {
                corrupt_coords += 1;
                continue;
            };
            let (value, format) = convert::convert_value(value);
            let cell = Cell {
                value,
                formula: None,
                format,
            };
            if !cell.is_vacant() {
                sheet.set(row, col, cell);
                // Checked per cell, not per sheet: a single sheet can hold
                // tens of millions of cells, and a limit that only fires
                // between sheets does not stop us exhausting memory on one.
                // `found` is therefore where we stopped, not the file's total.
                if let Some(limit) = opts.max_cells {
                    let found = total_cells + sheet.len() as u64;
                    if found > limit {
                        return Err(IngestError::TooLarge { found, limit });
                    }
                }
            }
        }

        if opts.read_formulas {
            let mut untranslated = 0u32;
            match sheets.worksheet_formula(name) {
                Ok(formulas) => {
                    corrupt_coords +=
                        attach_formulas(&mut sheet, &formulas, format, &mut untranslated)
                }
                Err(e) => warnings.push(format!("no formulas for sheet {name:?}: {e}")),
            }
            // Said out loud for the same reason a corrupt coordinate is: a
            // reference left in OpenDocument syntax is one the graph will not
            // have an edge for, and a caller must not have to infer that from
            // an edge count.
            if untranslated > 0 {
                warnings.push(format!(
                    "sheet {name:?}: {untranslated} reference(s) could not be translated from \
                     OpenDocument formula syntax and were left as written"
                ));
            }
            // `attach_formulas` can add cells the value loop above never saw
            // — a formula whose cached value is blank creates a fresh entry
            // rather than being dropped, since it is still a node in the
            // dependency graph. Left unchecked, a sheet of such formulas
            // could grow past `max_cells` without the budget ever firing.
            if let Some(limit) = opts.max_cells {
                let found = total_cells + sheet.len() as u64;
                if found > limit {
                    return Err(IngestError::TooLarge { found, limit });
                }
            }
        }

        // Corrupt-coordinate cells are dropped rather than loaded (see
        // `fit_row`/`fit_col`), but silently is not this crate's contract for
        // a non-fatal problem — said once per sheet, not once per cell, since
        // a truncated or malformed file can produce a great many of them.
        if corrupt_coords > 0 {
            warnings.push(format!(
                "sheet {name:?}: {corrupt_coords} cell(s) had a coordinate past Excel's \
                 addressable range and were dropped"
            ));
        }

        sheet.merges = merges.get(index).cloned().unwrap_or_default();
        sheet.tables = tables.get(index).cloned().unwrap_or_default();

        total_cells += sheet.len() as u64;

        loaded_sheets.push(sheet);
    }

    Ok(Loaded {
        workbook: Workbook {
            path: path.to_string_lossy().into_owned(),
            format: Some(format),
            content_hash,
            sheets: loaded_sheets,
            defined_names,
            external_links: Vec::new(),
        },
        capabilities,
        warnings,
    })
}

fn convert_defined_names(
    raw: &[(String, String)],
    scopes: &[Option<usize>],
    format: WorkbookFormat,
    warnings: &mut Vec<String>,
) -> Vec<DefinedName> {
    let mut defined_names = Vec::with_capacity(raw.len());
    for (index, (name, refers_to)) in raw.iter().enumerate() {
        let target = if matches!(format, WorkbookFormat::Ods) {
            match odf::address_to_a1(refers_to) {
                Some(target) => target,
                None => {
                    warnings.push(format!(
                        "defined name {name:?} has an ODF target that could not be translated: {refers_to}"
                    ));
                    refers_to.clone()
                }
            }
        } else {
            refers_to.clone()
        };
        defined_names.push(DefinedName {
            name: name.clone(),
            refers_to: target,
            scope: scopes
                .get(index)
                .and_then(|scope| scope.map(|sheet| SheetId(sheet as u16))),
        });
    }
    defined_names
}

/// Overlay formula text onto cells whose cached value we already read.
///
/// A formula cell always has a cached value in the file, but the two ranges are
/// read independently, so a formula may land on a coordinate we skipped as
/// blank — a formula evaluating to the empty string, for instance. Those cells
/// are created here rather than dropped, because a blank-valued formula is still
/// a node in the dependency graph.
///
/// ODS formula text arrives in OpenDocument syntax and is translated to A1 here
/// (see [`odf`]), so that no other crate ever sees a second formula dialect;
/// `untranslated` accumulates the references that translation had to leave as
/// written.
///
/// Returns how many formula cells had to be dropped for a corrupt coordinate.
fn attach_formulas(
    sheet: &mut Sheet,
    formulas: &calamine::Range<String>,
    format: WorkbookFormat,
    untranslated: &mut u32,
) -> u32 {
    // As with values, these coordinates are relative to the formula range's own
    // origin, which is generally *not* the same as the value range's origin.
    let (row0, col0) = formulas.start().unwrap_or((0, 0));
    let mut corrupt_coords = 0u32;
    let own_sheet = sheet.name.clone();
    for (row, col, text) in formulas.used_cells() {
        if text.is_empty() {
            continue;
        }
        let (Some(row), Some(col)) = (fit_row(row + row0 as usize), fit_col(col + col0 as usize))
        else {
            corrupt_coords += 1;
            continue;
        };
        // calamine strips the leading '='; normalise in case a backend keeps it.
        let text = text.strip_prefix('=').unwrap_or(text);
        let formula = if matches!(format, WorkbookFormat::Ods) {
            let (a1, left) = odf::to_a1(text, &own_sheet);
            *untranslated += left;
            a1
        } else {
            text.to_string()
        };
        match sheet.get(row, col) {
            Some(existing) => {
                let mut cell = existing.clone();
                cell.formula = Some(formula);
                sheet.set(row, col, cell);
            }
            None => {
                let mut cell = Cell::literal(eg_model::CellValue::Empty);
                cell.formula = Some(formula);
                sheet.set(row, col, cell);
            }
        }
    }
    corrupt_coords
}

/// Narrow a calamine column index to `u16`, dropping anything past XFD.
///
/// calamine indexes cells with `usize`; Excel cannot address a column beyond
/// 16383, so a wider index means corrupt input, and skipping it is safer than
/// panicking mid-load.
fn fit_col(col: usize) -> Option<u16> {
    (col <= eg_model::MAX_COL as usize).then_some(col as u16)
}

/// Narrow a calamine row index to `u32`, dropping anything past Excel's limit.
fn fit_row(row: usize) -> Option<u32> {
    (row <= eg_model::MAX_ROW as usize).then_some(row as u32)
}

/// Gather merged ranges per sheet index, where the format supports them.
fn collect_merges<RS>(
    sheets: &mut Sheets<RS>,
    metadata: &[(String, Visibility)],
    warnings: &mut Vec<String>,
) -> Vec<Vec<RangeRef>>
where
    RS: std::io::Read + std::io::Seek,
{
    let mut out = vec![Vec::new(); metadata.len()];
    for (i, (name, _)) in metadata.iter().enumerate() {
        let sheet_id = SheetId(i as u16);
        let dims = match sheets {
            Sheets::Xlsx(xlsx) => xlsx
                .merge_cells_by_sheet_name(name)
                .map_err(|e| e.to_string()),
            Sheets::Xls(xls) => xls
                .merge_cells_by_sheet_name(name)
                .map_err(|e| e.to_string()),
            // Xlsb and Ods expose no merge information through calamine.
            Sheets::Xlsb(_) | Sheets::Ods(_) => continue,
        };
        match dims {
            Ok(dims) => {
                out[i] = dims
                    .iter()
                    .filter_map(|d| convert::dimensions_to_range(d, sheet_id))
                    .collect();
            }
            Err(e) => warnings.push(format!("could not read merged regions for {name:?}: {e}")),
        }
    }
    out
}

/// Gather declared Excel tables per sheet index, where supported.
fn collect_tables<RS>(
    sheets: &mut Sheets<RS>,
    metadata: &[(String, Visibility)],
    warnings: &mut Vec<String>,
) -> Vec<Vec<ExcelTable>>
where
    RS: std::io::Read + std::io::Seek,
{
    let mut out = vec![Vec::new(); metadata.len()];
    let Sheets::Xlsx(xlsx) = sheets else {
        return out;
    };
    if let Err(e) = xlsx.load_tables() {
        warnings.push(format!("could not read tables: {e}"));
        return out;
    }
    let names: Vec<String> = xlsx.table_names().into_iter().cloned().collect();
    for table_name in names {
        let table = match xlsx.table_by_name(&table_name) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("could not read table {table_name:?}: {e}"));
                continue;
            }
        };
        let Some(i) = index_of(metadata, table.sheet_name()) else {
            continue;
        };
        let data: &calamine::Range<Data> = table.data();
        let (Some(start), Some(end)) = (data.start(), data.end()) else {
            continue;
        };
        let (Some(left), Some(right)) = (fit_col(start.1 as usize), fit_col(end.1 as usize)) else {
            continue;
        };
        let (Some(body_top), Some(bottom)) = (fit_row(start.0 as usize), fit_row(end.0 as usize))
        else {
            continue;
        };
        let columns = table.columns().to_vec();
        // `tableColumn` names exist even for a table declared with "My table
        // has headers" unchecked — Excel auto-names them Column1, Column2, …
        // — so their presence says nothing about whether there is an actual
        // header row. `headerRowCount`/`totalsRowCount`, the table's own
        // declaration, are what calamine's fork now exposes for this.
        let has_header_row = table.has_header_row();
        let has_totals_row = table.has_totals_row();
        // calamine's table range covers only the data body; the header and
        // totals rows sit directly above and below it, one row each when
        // declared, and are folded back in so `range` is the table's whole
        // declared extent — the shape `Region::body()` expects, the same way
        // it already excludes a detected title or header from its range.
        let top = if has_header_row {
            body_top.saturating_sub(1)
        } else {
            body_top
        };
        let bottom = if has_totals_row {
            bottom.saturating_add(1).min(eg_model::MAX_ROW)
        } else {
            bottom
        };
        out[i].push(ExcelTable {
            name: table.name().to_string(),
            range: RangeRef::new(SheetId(i as u16), top, left, bottom, right),
            columns,
            has_header_row,
            has_totals_row,
        });
    }
    out
}

fn index_of(metadata: &[(String, Visibility)], name: &str) -> Option<usize> {
    metadata
        .iter()
        .position(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// Content hash of the source file, used to skip re-ingesting unchanged input.
fn hash_reader(file: &mut std::fs::File, path: &Path) -> Result<String, IngestError> {
    use std::io::Read;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|source| IngestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_reflect_format_reality() {
        let xlsx = Capabilities::for_format(WorkbookFormat::Xlsx);
        assert!(xlsx.formulas && xlsx.merged_cells && xlsx.declared_tables && xlsx.writable);

        // XLSB is the format the project exists for; its gaps must be explicit.
        let xlsb = Capabilities::for_format(WorkbookFormat::Xlsb);
        assert!(xlsb.formulas, "xlsb formulas are the whole premise");
        assert!(!xlsb.merged_cells);
        assert!(!xlsb.declared_tables);
        assert!(!xlsb.writable);

        // Styling is unavailable everywhere, so nothing may depend on it.
        for f in [
            WorkbookFormat::Xlsx,
            WorkbookFormat::Xlsm,
            WorkbookFormat::Xlsb,
            WorkbookFormat::Xls,
            WorkbookFormat::Ods,
        ] {
            assert!(!Capabilities::for_format(f).cell_styling, "{f}");
        }
    }

    #[test]
    fn an_untranslatable_ods_name_target_is_warned_about() {
        let mut warnings = Vec::new();
        let names = convert_defined_names(
            &[("Rate".to_string(), ".B2:$Mar.B2".to_string())],
            &[None],
            WorkbookFormat::Ods,
            &mut warnings,
        );
        assert_eq!(names[0].refers_to, ".B2:$Mar.B2");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Rate") && warnings[0].contains("could not be translated"));
    }

    #[test]
    fn limitations_are_reported_for_xlsb() {
        let notes = Capabilities::for_format(WorkbookFormat::Xlsb).limitations();
        assert!(notes.iter().any(|n| n.contains("merged")));
        assert!(notes.iter().any(|n| n.contains("written back")));
    }

    #[test]
    fn external_links_are_disclosed_as_a_gap_not_left_silent() {
        // L22: `Workbook::external_links` is always empty — calamine never
        // populates it — and that used to be undisclosed, reading exactly
        // like "this workbook links to nothing" instead of "this reader
        // cannot tell you."
        for f in [
            WorkbookFormat::Xlsx,
            WorkbookFormat::Xlsm,
            WorkbookFormat::Xlsb,
            WorkbookFormat::Xls,
            WorkbookFormat::Ods,
        ] {
            let caps = Capabilities::for_format(f);
            assert!(!caps.external_links, "{f}");
            assert!(
                caps.limitations().iter().any(|n| n.contains("external")),
                "{f}"
            );
        }
    }

    #[test]
    fn unsupported_extensions_are_rejected() {
        let err = load("nope.csv").unwrap_err();
        assert!(matches!(err, IngestError::UnsupportedFormat(_)));
    }

    #[test]
    fn corrupt_formula_coordinates_are_counted_not_silently_dropped() {
        // L22: a formula landing past Excel's addressable range (calamine's
        // own coordinates are wider than that, so this is reachable without
        // a hand-corrupted file) used to vanish with no trace at all.
        let cells = vec![
            calamine::Cell::new((0u32, 0u32), "A1".to_string()),
            calamine::Cell::new((0u32, 100_000u32), "OUT".to_string()),
        ];
        let formulas = calamine::Range::from_sparse(cells);
        let mut sheet = Sheet::new(SheetId(0), "Sheet1");
        let mut untranslated = 0;
        let dropped = attach_formulas(
            &mut sheet,
            &formulas,
            WorkbookFormat::Xlsx,
            &mut untranslated,
        );
        assert_eq!(dropped, 1, "the out-of-range formula cell is counted");
        assert_eq!(
            sheet.get(0, 0).and_then(|c| c.formula.as_deref()),
            Some("A1"),
            "the in-range one still lands"
        );
    }

    #[test]
    fn columns_beyond_xfd_are_dropped_not_panicked() {
        assert_eq!(fit_col(0), Some(0));
        assert_eq!(fit_col(16_383), Some(16_383));
        assert_eq!(fit_col(16_384), None);
        assert_eq!(fit_col(usize::MAX), None);
        assert_eq!(fit_row(1_048_575), Some(1_048_575));
        assert_eq!(fit_row(1_048_576), None);
    }
}
