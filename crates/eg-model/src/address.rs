//! Cell and range addressing.
//!
//! Everything in ExcelGRAG is anchored to a cell address, so this module is the
//! load-bearing wall of the whole system. Two representations matter:
//!
//! - **A1** (`Sheet1!$B$7`) is what users, formulas and citations speak.
//! - **R1C1** (`R[-1]C2`) is what formula *shapes* are normalised to, so that a
//!   column of ten thousand structurally identical formulas collapses into a
//!   single graph node.
//!
//! Rows and columns are stored 0-based internally and rendered 1-based, which is
//! the usual source of off-by-one bugs; the conversion happens only at the
//! parse/format boundary and nowhere else.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Maximum row index (0-based) in a modern Excel worksheet: 1,048,576 rows.
pub const MAX_ROW: u32 = 1_048_575;
/// Maximum column index (0-based) in a modern Excel worksheet: 16,384 columns (XFD).
pub const MAX_COL: u32 = 16_383;

/// Index of a sheet within its workbook, in workbook (tab) order.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct SheetId(pub u16);

impl fmt::Display for SheetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Errors produced when parsing an address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error("empty address")]
    Empty,
    #[error("malformed address: {0}")]
    Malformed(String),
    #[error("column {0:?} is out of range (max XFD)")]
    ColumnOutOfRange(String),
    #[error("row {0} is out of range (max 1048576)")]
    RowOutOfRange(u64),
    #[error("unterminated quoted sheet name in {0:?}")]
    UnterminatedSheetName(String),
}

/// A cell address within a known sheet.
///
/// Deliberately 8 bytes so that dense vectors of these stay cache-friendly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CellRef {
    pub sheet: SheetId,
    pub row: u32,
    pub col: u16,
}

impl CellRef {
    pub fn new(sheet: SheetId, row: u32, col: u16) -> Self {
        Self { sheet, row, col }
    }

    /// Render just the local part, e.g. `B7`. Use [`CellRef::to_a1_with_sheet`]
    /// when the address is going into a citation, which must be unambiguous.
    pub fn to_a1(&self) -> String {
        format!("{}{}", col_to_letters(self.col as u32), self.row + 1)
    }

    /// Render a fully-qualified citation, e.g. `'Q3 Sales'!B7`.
    pub fn to_a1_with_sheet(&self, sheet_name: &str) -> String {
        format!("{}!{}", quote_sheet_name(sheet_name), self.to_a1())
    }

    /// Parse a sheet-local address such as `B7` or `$B$7`.
    pub fn parse_local(s: &str, sheet: SheetId) -> Result<Self, AddressError> {
        let (col, row, _, _) = parse_local_parts(s)?;
        Ok(Self::new(sheet, row, col))
    }

    /// Offset this reference, returning `None` if it leaves the sheet.
    pub fn offset(&self, d_row: i64, d_col: i64) -> Option<Self> {
        let row = (self.row as i64).checked_add(d_row)?;
        let col = (self.col as i64).checked_add(d_col)?;
        if row < 0 || col < 0 || row > MAX_ROW as i64 || col > MAX_COL as i64 {
            return None;
        }
        Some(Self::new(self.sheet, row as u32, col as u16))
    }
}

/// An inclusive rectangular range within a single sheet.
///
/// The constructor normalises corners, so `C3:A1` and `A1:C3` are the same range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RangeRef {
    pub sheet: SheetId,
    pub top: u32,
    pub left: u16,
    pub bottom: u32,
    pub right: u16,
}

impl RangeRef {
    pub fn new(sheet: SheetId, top: u32, left: u16, bottom: u32, right: u16) -> Self {
        Self {
            sheet,
            top: top.min(bottom),
            left: left.min(right),
            bottom: top.max(bottom),
            right: left.max(right),
        }
    }

    pub fn single(cell: CellRef) -> Self {
        Self {
            sheet: cell.sheet,
            top: cell.row,
            left: cell.col,
            bottom: cell.row,
            right: cell.col,
        }
    }

    pub fn rows(&self) -> u32 {
        self.bottom - self.top + 1
    }

    pub fn cols(&self) -> u16 {
        self.right - self.left + 1
    }

    /// Cell count, widened to `u64` because a full-sheet range overflows `u32`.
    pub fn cell_count(&self) -> u64 {
        self.rows() as u64 * self.cols() as u64
    }

    pub fn contains(&self, cell: CellRef) -> bool {
        cell.sheet == self.sheet
            && cell.row >= self.top
            && cell.row <= self.bottom
            && cell.col >= self.left
            && cell.col <= self.right
    }

    pub fn intersects(&self, other: &RangeRef) -> bool {
        self.sheet == other.sheet
            && self.top <= other.bottom
            && other.top <= self.bottom
            && self.left <= other.right
            && other.left <= self.right
    }

    /// Smallest range covering both operands. Panics if they are on different sheets.
    pub fn union(&self, other: &RangeRef) -> RangeRef {
        debug_assert_eq!(self.sheet, other.sheet, "cannot union ranges across sheets");
        RangeRef {
            sheet: self.sheet,
            top: self.top.min(other.top),
            left: self.left.min(other.left),
            bottom: self.bottom.max(other.bottom),
            right: self.right.max(other.right),
        }
    }

    pub fn top_left(&self) -> CellRef {
        CellRef::new(self.sheet, self.top, self.left)
    }

    pub fn bottom_right(&self) -> CellRef {
        CellRef::new(self.sheet, self.bottom, self.right)
    }

    /// Iterate cells in row-major order.
    pub fn iter_cells(&self) -> impl Iterator<Item = CellRef> + '_ {
        (self.top..=self.bottom).flat_map(move |r| {
            (self.left..=self.right).map(move |c| CellRef::new(self.sheet, r, c))
        })
    }

    pub fn to_a1(&self) -> String {
        if self.top == self.bottom && self.left == self.right {
            self.top_left().to_a1()
        } else {
            format!(
                "{}:{}",
                self.top_left().to_a1(),
                self.bottom_right().to_a1()
            )
        }
    }

    pub fn to_a1_with_sheet(&self, sheet_name: &str) -> String {
        format!("{}!{}", quote_sheet_name(sheet_name), self.to_a1())
    }

    /// Parse a sheet-local range such as `A1:C3`, `$A$1:$C$3` or a bare `B7`.
    pub fn parse_local(s: &str, sheet: SheetId) -> Result<Self, AddressError> {
        let s = s.trim();
        match s.split_once(':') {
            Some((a, b)) => {
                let (c1, r1, _, _) = parse_local_parts(a)?;
                let (c2, r2, _, _) = parse_local_parts(b)?;
                Ok(Self::new(sheet, r1, c1, r2, c2))
            }
            None => {
                let (c, r, _, _) = parse_local_parts(s)?;
                Ok(Self::new(sheet, r, c, r, c))
            }
        }
    }
}

impl fmt::Display for RangeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_a1())
    }
}

/// A reference as it appeared in a formula, before sheet resolution.
///
/// Formula text names sheets by string, and the referenced sheet may not exist
/// (a broken reference) or may live in another workbook. Resolution to a
/// [`RangeRef`] therefore happens separately, against the workbook's sheet table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRef {
    /// `None` means "the sheet the formula lives on".
    pub sheet_name: Option<String>,
    /// Present for 3-D references spanning sheets, e.g. `Jan:Dec!A1`.
    pub end_sheet_name: Option<String>,
    /// Present for external-workbook references, e.g. `[1]Sheet1!A1`.
    pub workbook: Option<String>,
    pub top: u32,
    pub left: u16,
    pub bottom: u32,
    pub right: u16,
    pub abs_top: bool,
    pub abs_left: bool,
    pub abs_bottom: bool,
    pub abs_right: bool,
}

impl ParsedRef {
    /// Bind to a concrete sheet, yielding an addressable range.
    pub fn resolve(&self, sheet: SheetId) -> RangeRef {
        RangeRef::new(sheet, self.top, self.left, self.bottom, self.right)
    }

    pub fn is_single_cell(&self) -> bool {
        self.top == self.bottom && self.left == self.right
    }
}

/// Parse a full A1 reference, optionally sheet- and workbook-qualified.
///
/// Handles `A1`, `$A$1`, `A1:B2`, `Sheet1!A1`, `'Q3 Sales'!A1:B2`,
/// `[1]Sheet1!A1` and 3-D `Jan:Dec!A1`.
pub fn parse_a1(s: &str) -> Result<ParsedRef, AddressError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(AddressError::Empty);
    }

    let (prefix, local) = split_sheet_prefix(s)?;
    let (workbook, sheet_name, end_sheet_name) = match prefix {
        Some(p) => parse_sheet_prefix(&p)?,
        None => (None, None, None),
    };

    let (left, top, abs_left, abs_top, right, bottom, abs_right, abs_bottom) =
        match local.split_once(':') {
            Some((a, b)) => {
                let (c1, r1, ac1, ar1) = parse_local_parts(a)?;
                let (c2, r2, ac2, ar2) = parse_local_parts(b)?;
                (c1, r1, ac1, ar1, c2, r2, ac2, ar2)
            }
            None => {
                let (c, r, ac, ar) = parse_local_parts(local)?;
                (c, r, ac, ar, c, r, ac, ar)
            }
        };

    // Normalise corners, keeping each corner's absoluteness attached to it.
    let (top, bottom, abs_top, abs_bottom) = if top <= bottom {
        (top, bottom, abs_top, abs_bottom)
    } else {
        (bottom, top, abs_bottom, abs_top)
    };
    let (left, right, abs_left, abs_right) = if left <= right {
        (left, right, abs_left, abs_right)
    } else {
        (right, left, abs_right, abs_left)
    };

    Ok(ParsedRef {
        sheet_name,
        end_sheet_name,
        workbook,
        top,
        left,
        bottom,
        right,
        abs_top,
        abs_left,
        abs_bottom,
        abs_right,
    })
}

/// Split `Sheet!A1` into its sheet prefix and local part, respecting quoting.
///
/// A quoted sheet name may itself contain `!`, so we cannot simply split on the
/// last `!`; we scan and track quote state instead.
fn split_sheet_prefix(s: &str) -> Result<(Option<String>, &str), AddressError> {
    if !s.starts_with('\'') {
        // Unquoted: the local part never contains '!', so the last one wins.
        return Ok(match s.rfind('!') {
            Some(i) => (Some(s[..i].to_string()), &s[i + 1..]),
            None => (None, s),
        });
    }

    // Quoted: find the closing quote, honouring '' as an escaped quote.
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            break;
        }
        i += 1;
    }
    if i >= bytes.len() {
        return Err(AddressError::UnterminatedSheetName(s.to_string()));
    }
    let rest = &s[i + 1..];
    let local = rest
        .strip_prefix('!')
        .ok_or_else(|| AddressError::Malformed(s.to_string()))?;
    Ok((Some(s[..=i].to_string()), local))
}

/// A decomposed sheet prefix: workbook, sheet, and (for 3-D refs) end sheet.
type SheetPrefix = (Option<String>, Option<String>, Option<String>);

/// Decompose a sheet prefix into workbook, sheet, and (for 3-D refs) end sheet.
fn parse_sheet_prefix(prefix: &str) -> Result<SheetPrefix, AddressError> {
    let mut p = prefix.trim();

    // Strip the surrounding quotes first; the workbook marker lives inside them
    // when the path contains spaces, e.g. '[My Book.xlsx]Sheet 1'.
    let quoted = p.starts_with('\'') && p.ends_with('\'') && p.len() >= 2;
    let unquoted_owned;
    if quoted {
        unquoted_owned = p[1..p.len() - 1].replace("''", "'");
        p = &unquoted_owned;
    }

    let (workbook, sheets) = if let Some(rest) = p.strip_prefix('[') {
        match rest.split_once(']') {
            Some((wb, sheets)) => (Some(wb.to_string()), sheets),
            None => return Err(AddressError::Malformed(prefix.to_string())),
        }
    } else {
        (None, p)
    };

    if sheets.is_empty() {
        return Ok((workbook, None, None));
    }

    Ok(match sheets.split_once(':') {
        Some((a, b)) => (workbook, Some(a.to_string()), Some(b.to_string())),
        None => (workbook, Some(sheets.to_string()), None),
    })
}

/// Parse the `[$]COL[$]ROW` core of an A1 address.
///
/// Returns `(col, row, col_is_absolute, row_is_absolute)`.
fn parse_local_parts(s: &str) -> Result<(u16, u32, bool, bool), AddressError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(AddressError::Empty);
    }

    let mut chars = s.char_indices().peekable();

    let abs_col = matches!(chars.peek(), Some((_, '$')));
    if abs_col {
        chars.next();
    }

    let letters_start = chars.peek().map(|(i, _)| *i).unwrap_or(s.len());
    let mut letters_end = letters_start;
    while let Some(&(i, c)) = chars.peek() {
        if c.is_ascii_alphabetic() {
            letters_end = i + c.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    let letters = &s[letters_start..letters_end];
    if letters.is_empty() {
        return Err(AddressError::Malformed(s.to_string()));
    }

    let abs_row = matches!(chars.peek(), Some((_, '$')));
    if abs_row {
        chars.next();
    }

    let digits_start = chars.peek().map(|(i, _)| *i).unwrap_or(s.len());
    let digits = &s[digits_start..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AddressError::Malformed(s.to_string()));
    }

    let col = letters_to_col(letters)?;
    let row_1based: u64 = digits
        .parse()
        .map_err(|_| AddressError::Malformed(s.to_string()))?;
    if row_1based == 0 || row_1based > MAX_ROW as u64 + 1 {
        return Err(AddressError::RowOutOfRange(row_1based));
    }

    Ok((col, (row_1based - 1) as u32, abs_col, abs_row))
}

/// Convert column letters to a 0-based index. `A` -> 0, `Z` -> 25, `AA` -> 26.
pub fn letters_to_col(letters: &str) -> Result<u16, AddressError> {
    if letters.is_empty() || letters.len() > 3 {
        return Err(AddressError::ColumnOutOfRange(letters.to_string()));
    }
    let mut n: u32 = 0;
    for c in letters.chars() {
        let d = match c {
            'A'..='Z' => c as u32 - 'A' as u32 + 1,
            'a'..='z' => c as u32 - 'a' as u32 + 1,
            _ => return Err(AddressError::ColumnOutOfRange(letters.to_string())),
        };
        n = n * 26 + d;
    }
    let col = n - 1;
    if col > MAX_COL {
        return Err(AddressError::ColumnOutOfRange(letters.to_string()));
    }
    Ok(col as u16)
}

/// Convert a 0-based column index to letters. 0 -> `A`, 26 -> `AA`.
///
/// A valid column needs at most three letters (`XFD`), but the buffer is sized
/// for any `u32` so that an out-of-range index — which reaches here from
/// diagnostics and from `CellRef`s built by callers rather than by the parsers —
/// renders nonsense instead of panicking.
pub fn col_to_letters(col: u32) -> String {
    // Widened so that `col == u32::MAX` does not overflow the +1.
    let mut n = u64::from(col) + 1;
    let mut buf = [0u8; 7];
    let mut len = 0;
    while n > 0 {
        let rem = (n - 1) % 26;
        buf[len] = b'A' + rem as u8;
        len += 1;
        n = (n - 1) / 26;
    }
    buf[..len].reverse();
    // Safe: every byte written is an ASCII uppercase letter.
    String::from_utf8(buf[..len].to_vec()).expect("ASCII letters")
}

/// Quote a sheet name for inclusion in an A1 reference, if Excel would.
///
/// Excel quotes when the name is not a bare identifier: any character outside
/// `[A-Za-z0-9_.]`, or a leading digit, forces quotes. Embedded quotes double.
pub fn quote_sheet_name(name: &str) -> String {
    let needs_quotes = name.is_empty()
        || name.chars().next().is_some_and(|c| c.is_ascii_digit())
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.');
    if needs_quotes {
        format!("'{}'", name.replace('\'', "''"))
    } else {
        name.to_string()
    }
}

/// One coordinate of an R1C1 reference: absolute index or relative offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum R1C1Coord {
    /// A fixed 0-based index, written `R5` / `C3` (rendered 1-based).
    Absolute(u32),
    /// An offset from the formula's own position, written `R[-1]` / `C[2]`.
    Relative(i32),
}

impl R1C1Coord {
    fn render(&self, tag: char) -> String {
        match self {
            R1C1Coord::Absolute(i) => format!("{tag}{}", i + 1),
            R1C1Coord::Relative(0) => tag.to_string(),
            R1C1Coord::Relative(d) => format!("{tag}[{d}]"),
        }
    }
}

/// A reference expressed relative to an anchor cell.
///
/// This is the canonical form used to decide whether two formulas share a
/// *shape*: `=A1*2` in B1 and `=A2*2` in B2 both normalise to `=RC[-1]*2`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct R1C1Ref {
    pub sheet_name: Option<String>,
    pub workbook: Option<String>,
    pub top: R1C1Coord,
    pub left: R1C1Coord,
    pub bottom: R1C1Coord,
    pub right: R1C1Coord,
}

impl R1C1Ref {
    /// Normalise a parsed A1 reference against the cell whose formula contains it.
    ///
    /// Absolute components (`$A$1`) stay absolute; relative ones become offsets.
    pub fn from_parsed(parsed: &ParsedRef, anchor: CellRef) -> Self {
        let row_coord = |v: u32, abs: bool| {
            if abs {
                R1C1Coord::Absolute(v)
            } else {
                R1C1Coord::Relative(v as i64 as i32 - anchor.row as i32)
            }
        };
        let col_coord = |v: u16, abs: bool| {
            if abs {
                R1C1Coord::Absolute(v as u32)
            } else {
                R1C1Coord::Relative(v as i32 - anchor.col as i32)
            }
        };
        Self {
            sheet_name: parsed.sheet_name.clone(),
            workbook: parsed.workbook.clone(),
            top: row_coord(parsed.top, parsed.abs_top),
            left: col_coord(parsed.left, parsed.abs_left),
            bottom: row_coord(parsed.bottom, parsed.abs_bottom),
            right: col_coord(parsed.right, parsed.abs_right),
        }
    }

    pub fn is_single_cell(&self) -> bool {
        self.top == self.bottom && self.left == self.right
    }

    pub fn to_r1c1(&self) -> String {
        let mut out = String::new();
        if let Some(wb) = &self.workbook {
            out.push('[');
            out.push_str(wb);
            out.push(']');
        }
        if let Some(sheet) = &self.sheet_name {
            out.push_str(&quote_sheet_name(sheet));
            out.push('!');
        }
        out.push_str(&self.top.render('R'));
        out.push_str(&self.left.render('C'));
        if !self.is_single_cell() {
            out.push(':');
            out.push_str(&self.bottom.render('R'));
            out.push_str(&self.right.render('C'));
        }
        out
    }
}

impl fmt::Display for R1C1Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_r1c1())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S0: SheetId = SheetId(0);

    #[test]
    fn column_letters_round_trip() {
        for &(col, letters) in &[
            (0u32, "A"),
            (25, "Z"),
            (26, "AA"),
            (27, "AB"),
            (51, "AZ"),
            (52, "BA"),
            (701, "ZZ"),
            (702, "AAA"),
            (16383, "XFD"),
        ] {
            assert_eq!(col_to_letters(col), letters, "col {col}");
            assert_eq!(
                letters_to_col(letters).unwrap() as u32,
                col,
                "letters {letters}"
            );
        }
    }

    #[test]
    fn column_letters_reject_out_of_range() {
        assert!(letters_to_col("XFE").is_err());
        assert!(letters_to_col("AAAA").is_err());
        assert!(letters_to_col("").is_err());
        assert!(letters_to_col("A1").is_err());
    }

    #[test]
    fn out_of_range_columns_render_rather_than_panic() {
        // `col` is only bounded at the parse boundary, so a caller-built index
        // past XFD must not blow up the renderer.
        assert_eq!(col_to_letters(18_277), "ZZZ");
        assert_eq!(col_to_letters(18_278), "AAAA");
        assert!(!col_to_letters(u32::MAX).is_empty());
    }

    #[test]
    fn letters_are_case_insensitive() {
        assert_eq!(letters_to_col("aa").unwrap(), letters_to_col("AA").unwrap());
    }

    #[test]
    fn cell_a1_round_trip() {
        let c = CellRef::new(S0, 6, 1);
        assert_eq!(c.to_a1(), "B7");
        assert_eq!(CellRef::parse_local("B7", S0).unwrap(), c);
        assert_eq!(CellRef::parse_local("$B$7", S0).unwrap(), c);
        assert_eq!(CellRef::parse_local("b7", S0).unwrap(), c);
    }

    #[test]
    fn row_bounds_are_enforced() {
        assert!(CellRef::parse_local("A0", S0).is_err());
        assert!(CellRef::parse_local("A1048576", S0).is_ok());
        assert!(CellRef::parse_local("A1048577", S0).is_err());
    }

    #[test]
    fn malformed_addresses_are_rejected() {
        for bad in ["", "7", "$", "A", "A$", "AB!", "1A", "A 1"] {
            assert!(
                CellRef::parse_local(bad, S0).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn range_normalises_corners() {
        let a = RangeRef::parse_local("C3:A1", S0).unwrap();
        let b = RangeRef::parse_local("A1:C3", S0).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_a1(), "A1:C3");
        assert_eq!(a.rows(), 3);
        assert_eq!(a.cols(), 3);
        assert_eq!(a.cell_count(), 9);
    }

    #[test]
    fn single_cell_range_renders_without_colon() {
        let r = RangeRef::parse_local("B7", S0).unwrap();
        assert_eq!(r.to_a1(), "B7");
    }

    #[test]
    fn full_sheet_range_does_not_overflow() {
        let r = RangeRef::new(S0, 0, 0, MAX_ROW, MAX_COL as u16);
        assert_eq!(r.cell_count(), 1_048_576u64 * 16_384u64);
    }

    #[test]
    fn range_geometry() {
        let r = RangeRef::parse_local("B2:D4", S0).unwrap();
        assert!(r.contains(CellRef::parse_local("C3", S0).unwrap()));
        assert!(!r.contains(CellRef::parse_local("A1", S0).unwrap()));
        assert!(r.intersects(&RangeRef::parse_local("D4:F6", S0).unwrap()));
        assert!(!r.intersects(&RangeRef::parse_local("E5:F6", S0).unwrap()));
        assert_eq!(
            r.union(&RangeRef::parse_local("A1", S0).unwrap()).to_a1(),
            "A1:D4"
        );
        assert_eq!(r.iter_cells().count(), 9);
    }

    #[test]
    fn iter_cells_is_row_major() {
        let r = RangeRef::parse_local("A1:B2", S0).unwrap();
        let got: Vec<String> = r.iter_cells().map(|c| c.to_a1()).collect();
        assert_eq!(got, ["A1", "B1", "A2", "B2"]);
    }

    #[test]
    fn offset_clamps_at_sheet_edges() {
        let c = CellRef::new(S0, 0, 0);
        assert_eq!(c.offset(1, 1).unwrap().to_a1(), "B2");
        assert!(c.offset(-1, 0).is_none());
        assert!(c.offset(0, -1).is_none());
        assert!(CellRef::new(S0, MAX_ROW, 0).offset(1, 0).is_none());
    }

    #[test]
    fn parses_sheet_qualified_references() {
        let p = parse_a1("Sheet1!A1").unwrap();
        assert_eq!(p.sheet_name.as_deref(), Some("Sheet1"));
        assert!(p.is_single_cell());

        let p = parse_a1("'Q3 Sales'!A1:B2").unwrap();
        assert_eq!(p.sheet_name.as_deref(), Some("Q3 Sales"));
        assert_eq!((p.top, p.left, p.bottom, p.right), (0, 0, 1, 1));
    }

    #[test]
    fn parses_sheet_name_containing_bang() {
        // The '!' inside quotes must not be mistaken for the separator.
        let p = parse_a1("'Hey! Sales'!B2").unwrap();
        assert_eq!(p.sheet_name.as_deref(), Some("Hey! Sales"));
        assert_eq!((p.top, p.left), (1, 1));
    }

    #[test]
    fn parses_sheet_name_containing_escaped_quote() {
        let p = parse_a1("'Bob''s Sheet'!A1").unwrap();
        assert_eq!(p.sheet_name.as_deref(), Some("Bob's Sheet"));
    }

    #[test]
    fn parses_external_and_3d_references() {
        let p = parse_a1("[1]Sheet1!A1").unwrap();
        assert_eq!(p.workbook.as_deref(), Some("1"));
        assert_eq!(p.sheet_name.as_deref(), Some("Sheet1"));

        let p = parse_a1("'[My Book.xlsx]Q3 Sales'!A1").unwrap();
        assert_eq!(p.workbook.as_deref(), Some("My Book.xlsx"));
        assert_eq!(p.sheet_name.as_deref(), Some("Q3 Sales"));

        let p = parse_a1("Jan:Dec!A1").unwrap();
        assert_eq!(p.sheet_name.as_deref(), Some("Jan"));
        assert_eq!(p.end_sheet_name.as_deref(), Some("Dec"));
    }

    #[test]
    fn absoluteness_is_tracked_per_component() {
        let p = parse_a1("$B7").unwrap();
        assert!(p.abs_left && !p.abs_top);
        let p = parse_a1("B$7").unwrap();
        assert!(!p.abs_left && p.abs_top);
    }

    #[test]
    fn absoluteness_follows_corners_when_swapped() {
        // Written bottom-right first: the $ must stay attached to its own corner.
        let p = parse_a1("$C$3:A1").unwrap();
        assert_eq!((p.top, p.left, p.bottom, p.right), (0, 0, 2, 2));
        assert!(!p.abs_top && !p.abs_left);
        assert!(p.abs_bottom && p.abs_right);
    }

    #[test]
    fn sheet_names_are_quoted_only_when_needed() {
        assert_eq!(quote_sheet_name("Sheet1"), "Sheet1");
        assert_eq!(quote_sheet_name("Q3.Sales"), "Q3.Sales");
        assert_eq!(quote_sheet_name("Q3 Sales"), "'Q3 Sales'");
        assert_eq!(quote_sheet_name("2024"), "'2024'");
        assert_eq!(quote_sheet_name("Bob's"), "'Bob''s'");
        assert_eq!(quote_sheet_name(""), "''");
    }

    #[test]
    fn citation_round_trips_through_parser() {
        let cell = CellRef::new(S0, 6, 1);
        let cited = cell.to_a1_with_sheet("Q3 Sales");
        assert_eq!(cited, "'Q3 Sales'!B7");
        let parsed = parse_a1(&cited).unwrap();
        assert_eq!(parsed.sheet_name.as_deref(), Some("Q3 Sales"));
        assert_eq!(parsed.resolve(S0), RangeRef::single(cell));
    }

    #[test]
    fn r1c1_relative_refs_are_position_independent() {
        // =A1*2 at B1 and =A2*2 at B2 must produce the same shape.
        let a = R1C1Ref::from_parsed(&parse_a1("A1").unwrap(), CellRef::new(S0, 0, 1));
        let b = R1C1Ref::from_parsed(&parse_a1("A2").unwrap(), CellRef::new(S0, 1, 1));
        assert_eq!(a, b);
        assert_eq!(a.to_r1c1(), "RC[-1]");
    }

    #[test]
    fn r1c1_absolute_refs_stay_pinned() {
        // $A$1 is the same cell regardless of where the formula sits, so the
        // shapes must differ from row to row only if the ref is relative.
        let a = R1C1Ref::from_parsed(&parse_a1("$A$1").unwrap(), CellRef::new(S0, 0, 1));
        let b = R1C1Ref::from_parsed(&parse_a1("$A$1").unwrap(), CellRef::new(S0, 9, 1));
        assert_eq!(a, b);
        assert_eq!(a.to_r1c1(), "R1C1");
    }

    #[test]
    fn r1c1_mixed_absoluteness() {
        let r = R1C1Ref::from_parsed(&parse_a1("$A2").unwrap(), CellRef::new(S0, 4, 3));
        assert_eq!(r.to_r1c1(), "R[-3]C1");
    }

    #[test]
    fn r1c1_renders_ranges_and_sheets() {
        let r = R1C1Ref::from_parsed(
            &parse_a1("'Q3 Sales'!B2:C3").unwrap(),
            CellRef::new(S0, 1, 1),
        );
        assert_eq!(r.to_r1c1(), "'Q3 Sales'!RC:R[1]C[1]");
    }

    #[test]
    fn r1c1_distinguishes_genuinely_different_shapes() {
        let a = R1C1Ref::from_parsed(&parse_a1("A1").unwrap(), CellRef::new(S0, 0, 1));
        let b = R1C1Ref::from_parsed(&parse_a1("A1").unwrap(), CellRef::new(S0, 0, 2));
        assert_ne!(a, b, "different column offsets must not collapse");
    }
}
