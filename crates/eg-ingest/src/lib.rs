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

use std::path::{Path, PathBuf};

use calamine::{Data, Reader, Sheets};
use eg_model::{
    Cell, DefinedName, ExcelTable, RangeRef, Sheet, SheetId, Visibility, Workbook, WorkbookFormat,
};

pub use convert::{convert_error, convert_value, fix_binary_comparison_operators};

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
    #[error("failed to read sheet {sheet:?}: {message}")]
    Sheet { sheet: String, message: String },
    #[error(
        "workbook has {found} populated cells, exceeding the configured limit of {limit}; \
         raise LoadOptions::max_cells to load it anyway"
    )]
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

    let content_hash = hash_file(path)?;

    let mut sheets = calamine::open_workbook_auto(path).map_err(|e| IngestError::Open {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut warnings = Vec::new();
    let capabilities = Capabilities::for_format(format);
    // `.xls` still needs the `>` / `>=` transposition repaired in this process.
    // `.xlsb` does not: our calamine fork fixes it at the source, and applying
    // the swap on top of that would invert the operators right back again.
    let needs_operator_fix = matches!(format, WorkbookFormat::Xls);

    let metadata: Vec<(String, Visibility)> = sheets
        .sheets_metadata()
        .iter()
        .map(|s| (s.name.clone(), convert::visibility(s.visible)))
        .collect();

    if metadata.len() > u16::MAX as usize + 1 {
        return Err(IngestError::TooManySheets(metadata.len()));
    }

    let defined_names: Vec<DefinedName> = sheets
        .defined_names()
        .iter()
        .map(|(name, refers_to)| DefinedName {
            name: name.clone(),
            refers_to: refers_to.clone(),
            // calamine flattens scope away; names are treated as workbook-scoped
            // and the sheet qualifier inside `refers_to` carries the location.
            scope: None,
        })
        .collect();

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
        for (row, col, value) in values.used_cells() {
            let (Some(row), Some(col)) = (
                fit_row(row + value_row0 as usize),
                fit_col(col + value_col0 as usize),
            ) else {
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
            }
        }

        if opts.read_formulas {
            match sheets.worksheet_formula(name) {
                // See `needs_operator_fix` above.
                Ok(formulas) => attach_formulas(&mut sheet, &formulas, needs_operator_fix),
                Err(e) => warnings.push(format!("no formulas for sheet {name:?}: {e}")),
            }
        }

        sheet.merges = merges.get(index).cloned().unwrap_or_default();
        sheet.tables = tables.get(index).cloned().unwrap_or_default();

        total_cells += sheet.len() as u64;
        if let Some(limit) = opts.max_cells {
            if total_cells > limit {
                return Err(IngestError::TooLarge {
                    found: total_cells,
                    limit,
                });
            }
        }

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

/// Overlay formula text onto cells whose cached value we already read.
///
/// A formula cell always has a cached value in the file, but the two ranges are
/// read independently, so a formula may land on a coordinate we skipped as
/// blank — a formula evaluating to the empty string, for instance. Those cells
/// are created here rather than dropped, because a blank-valued formula is still
/// a node in the dependency graph.
fn attach_formulas(sheet: &mut Sheet, formulas: &calamine::Range<String>, fix_operators: bool) {
    // As with values, these coordinates are relative to the formula range's own
    // origin, which is generally *not* the same as the value range's origin.
    let (row0, col0) = formulas.start().unwrap_or((0, 0));
    for (row, col, text) in formulas.used_cells() {
        if text.is_empty() {
            continue;
        }
        let (Some(row), Some(col)) = (
            fit_row(row + row0 as usize),
            fit_col(col + col0 as usize),
        ) else {
            continue;
        };
        // calamine strips the leading '='; normalise in case a backend keeps it.
        let text = text.strip_prefix('=').unwrap_or(text);
        let formula = if fix_operators {
            convert::fix_binary_comparison_operators(text)
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
            Sheets::Xlsx(xlsx) => xlsx.merge_cells_by_sheet_name(name).map_err(|e| e.to_string()),
            Sheets::Xls(xls) => xls.merge_cells_by_sheet_name(name).map_err(|e| e.to_string()),
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
        let (Some(left), Some(right)) = (fit_col(start.1 as usize), fit_col(end.1 as usize))
        else {
            continue;
        };
        let (Some(body_top), Some(bottom)) = (fit_row(start.0 as usize), fit_row(end.0 as usize))
        else {
            continue;
        };
        let columns = table.columns().to_vec();
        // calamine's table range covers the data body; the header row sits
        // directly above it when the table declares one.
        let has_header_row = !columns.is_empty();
        let top = if has_header_row {
            body_top.saturating_sub(1)
        } else {
            body_top
        };
        out[i].push(ExcelTable {
            name: table.name().to_string(),
            range: RangeRef::new(SheetId(i as u16), top, left, bottom, right),
            columns,
            has_header_row,
            has_totals_row: false,
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
fn hash_file(path: &Path) -> Result<String, IngestError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
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
    fn limitations_are_reported_for_xlsb() {
        let notes = Capabilities::for_format(WorkbookFormat::Xlsb).limitations();
        assert!(notes.iter().any(|n| n.contains("merged")));
        assert!(notes.iter().any(|n| n.contains("written back")));
    }

    #[test]
    fn unsupported_extensions_are_rejected() {
        let err = load("nope.csv").unwrap_err();
        assert!(matches!(err, IngestError::UnsupportedFormat(_)));
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
