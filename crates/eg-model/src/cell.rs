//! Cell values, types, and the presentation metadata that region detection needs.

use serde::{Deserialize, Serialize};

/// An Excel error value.
///
/// Kept as a distinct type rather than a string so that "the sheet contains an
/// error" and "we failed to compute" never get confused with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorKind {
    Null,
    Div0,
    Value,
    Ref,
    Name,
    Num,
    NA,
    GettingData,
    Spill,
    Calc,
}

impl ErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::Null => "#NULL!",
            ErrorKind::Div0 => "#DIV/0!",
            ErrorKind::Value => "#VALUE!",
            ErrorKind::Ref => "#REF!",
            ErrorKind::Name => "#NAME?",
            ErrorKind::Num => "#NUM!",
            ErrorKind::NA => "#N/A",
            ErrorKind::GettingData => "#GETTING_DATA",
            ErrorKind::Spill => "#SPILL!",
            ErrorKind::Calc => "#CALC!",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_uppercase().as_str() {
            "#NULL!" => ErrorKind::Null,
            "#DIV/0!" => ErrorKind::Div0,
            "#VALUE!" => ErrorKind::Value,
            "#REF!" => ErrorKind::Ref,
            "#NAME?" => ErrorKind::Name,
            "#NUM!" => ErrorKind::Num,
            "#N/A" => ErrorKind::NA,
            "#GETTING_DATA" => ErrorKind::GettingData,
            "#SPILL!" => ErrorKind::Spill,
            "#CALC!" => ErrorKind::Calc,
            _ => return None,
        })
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The value stored in a cell.
///
/// Dates are *not* a separate variant: Excel stores them as numbers and marks
/// them via the number format, so date-ness is a property of [`CellFormat`], not
/// of the value. Collapsing them here would lose the serial number that formulas
/// actually operate on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CellValue {
    Empty,
    Number(f64),
    Text(String),
    Bool(bool),
    Error(ErrorKind),
}

impl CellValue {
    pub fn is_empty(&self) -> bool {
        match self {
            CellValue::Empty => true,
            CellValue::Text(s) => s.is_empty(),
            _ => false,
        }
    }

    pub fn kind(&self) -> ValueKind {
        match self {
            CellValue::Empty => ValueKind::Empty,
            CellValue::Number(_) => ValueKind::Number,
            CellValue::Text(_) => ValueKind::Text,
            CellValue::Bool(_) => ValueKind::Bool,
            CellValue::Error(_) => ValueKind::Error,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            CellValue::Number(n) => Some(*n),
            CellValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            CellValue::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Plain rendering for indexing and table output. Empty cells render as `""`.
    pub fn to_display(&self) -> String {
        match self {
            CellValue::Empty => String::new(),
            CellValue::Number(n) => format_number(*n),
            CellValue::Text(s) => s.clone(),
            CellValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            CellValue::Error(e) => e.to_string(),
        }
    }
}

/// A number as a spreadsheet carries it: 15 significant decimal digits.
///
/// Comparisons are made on this rather than on the raw double, because Excel
/// does and workbooks depend on it. `10.13+6.75=16.88` is false in binary
/// floating point and true in every spreadsheet ever written. The same
/// rounding is what display code should render through, too — the shortest
/// round-trip form of an unrounded double shows the arithmetic noise past
/// the fifteenth digit (`0.30000000000000004`) that a sheet never would.
pub fn shown(n: f64) -> f64 {
    if n.is_finite() {
        format!("{n:.14e}").parse().unwrap_or(n)
    } else {
        n
    }
}

/// Render a float the way a spreadsheet would: no trailing `.0` on integers.
fn format_number(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n}");
        s
    }
}

/// Coarse value classification, used by region detection to spot the boundary
/// between a text header row and a numeric body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ValueKind {
    Empty,
    Number,
    Text,
    Bool,
    Error,
}

impl ValueKind {
    pub fn is_blank(&self) -> bool {
        matches!(self, ValueKind::Empty)
    }

    /// The kind as a word, for naming a cell without disclosing what is in it.
    pub fn as_str(&self) -> &'static str {
        match self {
            ValueKind::Empty => "empty",
            ValueKind::Number => "number",
            ValueKind::Text => "text",
            ValueKind::Bool => "bool",
            ValueKind::Error => "error",
        }
    }
}

/// Presentation attributes that carry structural signal.
///
/// Only the attributes that help delimit regions are kept; fonts, colours and
/// the rest of the styling surface are deliberately dropped to keep ingest cheap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CellFormat {
    pub bold: bool,
    pub italic: bool,
    /// Any fill other than "none" — a strong header signal.
    pub has_fill: bool,
    /// A border on the underlying edge, which often terminates a table.
    pub border_bottom: bool,
    pub border_top: bool,
    pub border_left: bool,
    pub border_right: bool,
    /// The number format implies a date/time rather than a plain number.
    pub is_date: bool,
    /// The number format is a percentage or currency — useful for column typing.
    pub is_percent: bool,
    pub is_currency: bool,
    /// Indentation level, which encodes hierarchy in many financial models.
    pub indent: u8,
}

impl CellFormat {
    /// Whether this formatting looks like a header rather than a data cell.
    pub fn looks_like_header(&self) -> bool {
        self.bold || self.has_fill || self.border_bottom
    }
}

/// A single populated cell as read from the workbook.
///
/// Cells that are empty *and* unformatted are never materialised; a sheet is a
/// sparse collection, because a nominally 1M-cell sheet is usually 99% blank.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub value: CellValue,
    /// The formula in A1 form, without the leading `=`. `None` for literals.
    pub formula: Option<String>,
    pub format: CellFormat,
}

impl Cell {
    pub fn literal(value: CellValue) -> Self {
        Self {
            value,
            formula: None,
            format: CellFormat::default(),
        }
    }

    pub fn is_formula(&self) -> bool {
        self.formula.is_some()
    }

    /// Whether the cell contributes nothing at all and can be treated as absent.
    pub fn is_vacant(&self) -> bool {
        self.value.is_empty() && self.formula.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kinds_round_trip() {
        for e in [
            ErrorKind::Null,
            ErrorKind::Div0,
            ErrorKind::Value,
            ErrorKind::Ref,
            ErrorKind::Name,
            ErrorKind::Num,
            ErrorKind::NA,
            ErrorKind::Spill,
            ErrorKind::Calc,
            ErrorKind::GettingData,
        ] {
            assert_eq!(ErrorKind::parse(e.as_str()), Some(e), "{e}");
        }
        assert_eq!(ErrorKind::parse("#NOPE!"), None);
        assert_eq!(ErrorKind::parse("#n/a"), Some(ErrorKind::NA));
    }

    #[test]
    fn empty_text_counts_as_empty() {
        assert!(CellValue::Text(String::new()).is_empty());
        assert!(CellValue::Empty.is_empty());
        assert!(!CellValue::Number(0.0).is_empty());
        assert!(!CellValue::Bool(false).is_empty());
    }

    #[test]
    fn numbers_render_without_spurious_decimals() {
        assert_eq!(CellValue::Number(42.0).to_display(), "42");
        assert_eq!(CellValue::Number(-7.0).to_display(), "-7");
        assert_eq!(CellValue::Number(1.5).to_display(), "1.5");
        assert_eq!(CellValue::Number(0.0).to_display(), "0");
    }

    #[test]
    fn bools_coerce_to_numbers_like_excel() {
        assert_eq!(CellValue::Bool(true).as_number(), Some(1.0));
        assert_eq!(CellValue::Bool(false).as_number(), Some(0.0));
        assert_eq!(CellValue::Text("5".into()).as_number(), None);
    }

    #[test]
    fn errors_display_as_excel_writes_them() {
        assert_eq!(CellValue::Error(ErrorKind::Div0).to_display(), "#DIV/0!");
    }

    #[test]
    fn vacant_cells_are_recognised() {
        assert!(Cell::literal(CellValue::Empty).is_vacant());
        assert!(!Cell::literal(CellValue::Number(1.0)).is_vacant());
        let f = Cell {
            value: CellValue::Empty,
            formula: Some("SUM(A1:A2)".into()),
            format: CellFormat::default(),
        };
        // A formula that currently evaluates to blank is still meaningful.
        assert!(!f.is_vacant());
        assert!(f.is_formula());
    }

    #[test]
    fn header_formatting_heuristic() {
        assert!(CellFormat {
            bold: true,
            ..Default::default()
        }
        .looks_like_header());
        assert!(CellFormat {
            has_fill: true,
            ..Default::default()
        }
        .looks_like_header());
        assert!(!CellFormat::default().looks_like_header());
        assert!(!CellFormat {
            italic: true,
            ..Default::default()
        }
        .looks_like_header());
    }
}
