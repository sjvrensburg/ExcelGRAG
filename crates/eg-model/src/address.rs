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
        format!(
            "{}{}",
            col_to_letters(self.col as u32),
            u64::from(self.row) + 1
        )
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

    /// Height, widened to `u64` because a range spanning every addressable
    /// row is one taller than `u32` can hold. Saturating the count into `u32`
    /// instead made `rows() * cols()` disagree with [`cell_count`], and the
    /// two are read as the same number: a column node's size is indexed as
    /// `rows()` while a budget is checked against `cell_count()`.
    ///
    /// [`cell_count`]: RangeRef::cell_count
    pub fn rows(&self) -> u64 {
        debug_assert!(self.top <= self.bottom, "range corners are not normalised");
        u64::from(self.bottom.saturating_sub(self.top)) + 1
    }

    /// Width, widened to `u32` for the reason [`rows`] is widened to `u64`.
    ///
    /// [`rows`]: RangeRef::rows
    pub fn cols(&self) -> u32 {
        debug_assert!(self.left <= self.right, "range corners are not normalised");
        u32::from(self.right.saturating_sub(self.left)) + 1
    }

    /// Cell count. At most `2^32 * 2^16`, so it cannot overflow `u64`.
    ///
    /// The subtractions saturate rather than wrapping because the fields are
    /// public: `new` normalises the corners, a struct literal does not, and an
    /// inverted range used to underflow here — a panic in debug and a wild
    /// number in release. It is still a caller's bug, and the debug assertions
    /// in [`rows`] and [`cols`] still say so.
    ///
    /// [`rows`]: RangeRef::rows
    /// [`cols`]: RangeRef::cols
    pub fn cell_count(&self) -> u64 {
        self.rows() * u64::from(self.cols())
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

    /// The overlap of two ranges, or `None` when they do not intersect (or
    /// sit on different sheets). Unlike [`RangeRef::intersects`], this hands
    /// back the overlap itself — the shape a caller needs to clip a
    /// geometrically huge reference (a whole column, say) down to the part
    /// worth iterating.
    pub fn intersection(&self, other: &RangeRef) -> Option<RangeRef> {
        if self.sheet != other.sheet {
            return None;
        }
        let top = self.top.max(other.top);
        let left = self.left.max(other.left);
        let bottom = self.bottom.min(other.bottom);
        let right = self.right.min(other.right);
        (top <= bottom && left <= right)
            .then(|| RangeRef::new(self.sheet, top, left, bottom, right))
    }

    /// Smallest range covering both operands.
    ///
    /// Debug-asserts that both are on one sheet — a union across sheets is
    /// meaningless — but does *not* panic in release, where the result simply
    /// takes `self`'s sheet. The doc comment used to promise a panic the code
    /// never delivered outside a debug build, which is the worse of the two
    /// mistakes: a caller reading it would skip a check of its own.
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

    /// Whether this reference spans every row of the sheet — Excel's `A:A`
    /// shorthand, or an explicit reference that amounts to the same thing
    /// (`A1:A1048576`). Excel does not distinguish the two, and neither does
    /// this: both mean "every row of these columns", and evaluating either
    /// safely requires the same clipping to what the sheet actually uses.
    pub fn is_whole_column(&self) -> bool {
        self.top == 0 && self.bottom == MAX_ROW
    }

    /// Whether this reference spans every column of the sheet — `3:3`, or its
    /// explicit equivalent `A3:XFD3`.
    pub fn is_whole_row(&self) -> bool {
        self.left == 0 && self.right == MAX_COL as u16
    }

    /// Every sheet this reference names.
    ///
    /// One sheet for an ordinary reference; every sheet between the two named
    /// ones, inclusive, for a 3-D reference (`Jan:Dec!A1`). `from` is the
    /// sheet the formula lives on, which is what an unqualified reference
    /// names.
    ///
    /// This lives on the parsed reference itself because which sheets a 3-D
    /// reference names has to be decided in exactly one place. The graph's
    /// lifting and the audit that checks it already shared one function for
    /// it; the cell layer quietly kept a second answer — the start sheet, and
    /// nothing else — which is how a what-if came to report the rest of a
    /// span as unaffected.
    ///
    /// `lookup` maps a sheet name to its id, and is a closure rather than a
    /// `&Workbook` because every caller runs this per reference over millions
    /// of formulas and each already keeps the sheet index it wants to use.
    /// Excel matches sheet names without regard to case, so a lookup that
    /// does not is a phantom broken reference.
    ///
    /// `Err` names the one sheet — start or end — that `lookup` did not know.
    pub fn spanned_sheets(
        &self,
        from: SheetId,
        lookup: impl Fn(&str) -> Option<SheetId>,
    ) -> Result<SheetSpan, &str> {
        let start = match &self.sheet_name {
            None => from,
            Some(name) => lookup(name).ok_or(name.as_str())?,
        };
        match &self.end_sheet_name {
            None => Ok(SheetSpan::one(start)),
            Some(name) => Ok(SheetSpan::between(
                start,
                lookup(name).ok_or(name.as_str())?,
            )),
        }
    }
}

/// The sheets one reference names, as a span of workbook (tab) order.
///
/// Two bounds rather than a list: [`SheetId`] *is* tab order, so the sheets
/// between the two a 3-D reference names are a contiguous range whichever end
/// was written first — and a scan that resolves every reference in a workbook
/// must not allocate merely to say that one names a single sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SheetSpan {
    first: SheetId,
    last: SheetId,
}

impl SheetSpan {
    /// What an ordinary reference names: one sheet.
    pub fn one(sheet: SheetId) -> Self {
        Self {
            first: sheet,
            last: sheet,
        }
    }

    /// Both ends of a 3-D reference, inclusive, written in either order.
    pub fn between(a: SheetId, b: SheetId) -> Self {
        Self {
            first: SheetId(a.0.min(b.0)),
            last: SheetId(a.0.max(b.0)),
        }
    }

    /// The first sheet in tab order — the only one, unless this is 3-D.
    pub fn first(&self) -> SheetId {
        self.first
    }

    /// The last sheet in tab order.
    pub fn last(&self) -> SheetId {
        self.last
    }

    /// Whether more than one sheet is named, i.e. whether this came from a
    /// 3-D reference.
    pub fn is_multi_sheet(&self) -> bool {
        self.first != self.last
    }

    /// How many sheets are named. Never zero.
    pub fn count(&self) -> usize {
        (self.last.0 - self.first.0) as usize + 1
    }

    /// Every sheet named, in tab order.
    pub fn iter(&self) -> impl Iterator<Item = SheetId> {
        (self.first.0..=self.last.0).map(SheetId)
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
                if let Some(cols) = parse_whole_columns(a, b) {
                    cols
                } else if let Some(rows) = parse_whole_rows(a, b) {
                    rows
                } else {
                    let (c1, r1, ac1, ar1) = parse_local_parts(a)?;
                    let (c2, r2, ac2, ar2) = parse_local_parts(b)?;
                    (c1, r1, ac1, ar1, c2, r2, ac2, ar2)
                }
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

    // A workbook marker, `[Book.xlsx]`, sits at the very start for the common
    // in-session form (`[1]Sheet1`), but Excel writes a *closed* linked
    // workbook with a full path before it — `C:\Reports\[Book1.xlsx]Sheet1`
    // or `/data/[Book1.xlsx]Sheet1` — so the bracket pair must be found
    // wherever it is, not assumed to open at position zero. Once a pair is
    // found, everything before it is a directory path, never a sheet name (a
    // path cannot itself be a 3-D range endpoint), so `:` is only a 3-D
    // separator when there is no bracket pair at all.
    let (workbook, sheets) = match p.find('[') {
        Some(open) => match p[open..].find(']') {
            Some(rel_close) => {
                let close = open + rel_close;
                (Some(p[open + 1..close].to_string()), &p[close + 1..])
            }
            None => return Err(AddressError::Malformed(prefix.to_string())),
        },
        None => (None, p),
    };

    // This function only ever runs when a `!` sheet separator was found in
    // the text — `parse_a1` calls it with `Some(prefix)` exactly then — so an
    // empty sheet name here is not "no sheet was named", it is one written
    // and left blank: `''!A1` (an explicit empty quoted name) or `!A1` (no
    // name at all before the bang). Excel accepts neither, and silently
    // reading either as an unqualified reference would answer a different
    // question from the one written.
    if sheets.is_empty() {
        return Err(AddressError::Malformed(prefix.to_string()));
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

/// `(left, top, abs_left, abs_top, right, bottom, abs_right, abs_bottom)` —
/// the same shape [`parse_a1`]'s local-part resolution builds every corner
/// tuple in, named once so the whole-axis helpers below don't each spell it
/// out.
type Corners = (u16, u32, bool, bool, u16, u32, bool, bool);

/// Parse `a:b` as Excel's `A:A` whole-column shorthand — every row of these
/// columns — if both sides are a bare column (optional `$`, letters, no row
/// digits). `None` when either side is not that shape, so the caller falls
/// through to ordinary corner parsing.
///
/// The row bound is synthesised as the sheet's full height and marked
/// absolute unconditionally: it does not depend on where a formula
/// referencing it sits, so `SUM(A:A)` normalises to the same R1C1 shape
/// wherever it is filled down, the same way an explicit `$A$1:$A$1048576`
/// would.
fn parse_whole_columns(a: &str, b: &str) -> Option<Corners> {
    let (c1, ac1) = parse_column_only(a)?;
    let (c2, ac2) = parse_column_only(b)?;
    Some((c1, 0, ac1, true, c2, MAX_ROW, ac2, true))
}

/// As [`parse_whole_columns`], for `3:3` — every column of these rows.
fn parse_whole_rows(a: &str, b: &str) -> Option<Corners> {
    let (r1, ar1) = parse_row_only(a)?;
    let (r2, ar2) = parse_row_only(b)?;
    Some((0, r1, true, ar1, MAX_COL as u16, r2, true, ar2))
}

/// Parse a bare column token: optional `$`, then one to three letters, and
/// nothing else — no row digits, which is what tells it apart from an
/// ordinary cell reference.
fn parse_column_only(s: &str) -> Option<(u16, bool)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let abs = bytes.first() == Some(&b'$');
    if abs {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    if i == start || i != bytes.len() {
        return None;
    }
    Some((letters_to_col(&s[start..i]).ok()?, abs))
}

/// Parse a bare row token: optional `$`, then digits, and nothing else — no
/// column letters.
fn parse_row_only(s: &str) -> Option<(u32, bool)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let abs = bytes.first() == Some(&b'$');
    if abs {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start || i != bytes.len() {
        return None;
    }
    let row_1based: u64 = s[start..i].parse().ok()?;
    if row_1based == 0 || row_1based > MAX_ROW as u64 + 1 {
        return None;
    }
    Some(((row_1based - 1) as u32, abs))
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
/// Excel quotes when the name is not a bare identifier: a character that is
/// not alphanumeric, `_` or `.` forces quotes — Unicode alphanumeric, not
/// only ASCII `[A-Za-z0-9]`, since Excel itself accepts an unquoted sheet
/// name like `Café`. So does a leading digit, or the name being shaped
/// exactly like a cell reference (`A1`, `$B$2`, …) — Excel disallows naming a
/// sheet that ambiguously in its own UI, but a workbook can still arrive with
/// one (renamed via another tool, or a corrupted file), and citing it bare
/// would read as the cell, not the sheet. Embedded quotes double.
pub fn quote_sheet_name(name: &str) -> String {
    let needs_quotes = name.is_empty()
        || name.chars().next().is_some_and(|c| c.is_ascii_digit())
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        || parse_local_parts(name).is_ok();
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
            R1C1Coord::Absolute(i) => format!("{tag}{}", u64::from(*i) + 1),
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
    /// Last sheet of a 3-D reference such as `Jan:Dec!A1`.
    #[serde(default)]
    pub end_sheet_name: Option<String>,
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
            end_sheet_name: parsed.end_sheet_name.clone(),
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
            match &self.end_sheet_name {
                Some(end_sheet) => {
                    let start = quote_sheet_name(sheet);
                    let end = quote_sheet_name(end_sheet);
                    if start == *sheet && end == *end_sheet {
                        out.push_str(sheet);
                        out.push(':');
                        out.push_str(end_sheet);
                    } else {
                        out.push('\'');
                        out.push_str(&sheet.replace('\'', "''"));
                        out.push(':');
                        out.push_str(&end_sheet.replace('\'', "''"));
                        out.push('\'');
                    }
                }
                None => out.push_str(&quote_sheet_name(sheet)),
            }
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
    fn geometry_of_an_un_normalised_range_does_not_underflow() {
        // Only reachable by bypassing `new`, which the public fields allow.
        // Debug builds assert; release must not wrap to four billion rows.
        let inverted = RangeRef {
            sheet: S0,
            top: 9,
            left: 4,
            bottom: 2,
            right: 1,
        };
        if cfg!(debug_assertions) {
            let hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let caught = std::panic::catch_unwind(|| inverted.cell_count());
            std::panic::set_hook(hook);
            assert!(caught.is_err(), "a debug build must say so, loudly");
        } else {
            assert_eq!(inverted.cell_count(), 1);
        }
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
    fn intersection_clips_a_huge_range_to_a_small_one() {
        let whole_column = RangeRef::new(S0, 0, 1, MAX_ROW, 1);
        let used = RangeRef::parse_local("A1:D4", S0).unwrap();
        let clipped = whole_column.intersection(&used).unwrap();
        assert_eq!(clipped.to_a1(), "B1:B4");

        assert!(RangeRef::parse_local("A1:B2", S0)
            .unwrap()
            .intersection(&RangeRef::parse_local("D4:F6", S0).unwrap())
            .is_none());

        let other_sheet = RangeRef::new(SheetId(1), 0, 0, 0, 0);
        assert!(used.intersection(&other_sheet).is_none());
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
    fn a_closed_workbook_link_carries_its_directory_path() {
        // Excel writes the full path before the bracket when the linked
        // workbook is closed — this used to split on the drive letter's `:`
        // as if it were a 3-D range, lifting to a bogus sheet named "C".
        let p = parse_a1("'C:\\Reports\\[Book1.xlsx]Sheet1'!A1").unwrap();
        assert_eq!(p.workbook.as_deref(), Some("Book1.xlsx"));
        assert_eq!(p.sheet_name.as_deref(), Some("Sheet1"));
        assert_eq!(p.end_sheet_name, None);

        let p = parse_a1("'/data/[Book1.xlsx]Sheet1'!A1").unwrap();
        assert_eq!(p.workbook.as_deref(), Some("Book1.xlsx"));
        assert_eq!(p.sheet_name.as_deref(), Some("Sheet1"));
        assert_eq!(p.end_sheet_name, None);

        // A path containing more than one `:` (a UNC-style or oddly quoted
        // one) must not confuse which colon, if any, is a 3-D separator —
        // there is a bracket pair here, so none of them are.
        let p = parse_a1("'C:\\Reports\\2024:Archive\\[Book1.xlsx]Sheet1'!A1").unwrap();
        assert_eq!(p.workbook.as_deref(), Some("Book1.xlsx"));
        assert_eq!(p.sheet_name.as_deref(), Some("Sheet1"));
        assert_eq!(p.end_sheet_name, None);
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
    fn a_sheet_named_like_a_cell_reference_is_quoted() {
        // L21: Excel itself refuses to name a sheet "A1" in its UI, but a
        // workbook can still arrive with one — renamed through another tool,
        // or a corrupted file — and citing it bare would read as the cell,
        // not the sheet.
        assert_eq!(quote_sheet_name("A1"), "'A1'");
        assert_eq!(quote_sheet_name("XFD1048576"), "'XFD1048576'");
        assert_eq!(quote_sheet_name("$B$2"), "'$B$2'");
        // A name that merely starts like a column but is not a valid cell
        // shape (too many letters, or the row out of range) is unaffected.
        assert_eq!(quote_sheet_name("Sheet1"), "Sheet1");
        assert_eq!(quote_sheet_name("XFE1"), "XFE1", "past the last column");
    }

    #[test]
    fn a_non_ascii_alphanumeric_sheet_name_still_needs_no_quotes() {
        // The doc comment used to describe an ASCII-only rule the code never
        // implemented — `char::is_alphanumeric` is Unicode-aware, and so is
        // Excel's own bare-identifier rule.
        assert_eq!(quote_sheet_name("Café"), "Café");
    }

    #[test]
    fn an_empty_quoted_sheet_name_is_rejected_not_read_as_unqualified() {
        // L21: `''!A1` names an empty sheet, not no sheet at all — Excel
        // rejects it, and reading it as a plain `A1` would silently answer a
        // different reference from the one written.
        assert!(parse_a1("''!A1").is_err());
        assert!(parse_a1("!A1").is_err(), "no sheet name before the bang");
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
    fn r1c1_preserves_a_3d_sheet_qualifier() {
        let r = R1C1Ref::from_parsed(
            &parse_a1("'Jan:Year End'!B2").unwrap(),
            CellRef::new(S0, 1, 1),
        );
        assert_eq!(r.end_sheet_name.as_deref(), Some("Year End"));
        assert_eq!(r.to_r1c1(), "'Jan:Year End'!RC");
        let reparsed = parse_a1(&r.to_r1c1().replace("RC", "A1")).unwrap();
        assert_eq!(reparsed.sheet_name.as_deref(), Some("Jan"));
        assert_eq!(reparsed.end_sheet_name.as_deref(), Some("Year End"));
    }

    #[test]
    fn public_geometry_rendering_does_not_overflow() {
        assert_eq!(CellRef::new(S0, u32::MAX, 0).to_a1(), "A4294967296");
        let range = RangeRef::new(S0, 0, 0, u32::MAX, u16::MAX);
        assert_eq!(range.rows(), u32::MAX as u64 + 1);
        assert_eq!(range.cols(), u16::MAX as u32 + 1);
        assert_eq!(range.cell_count(), (u32::MAX as u64 + 1) * 65_536);
        // The two ways of asking must agree: a saturating `rows()` made the
        // product 65,536 cells short of `cell_count()`.
        assert_eq!(range.rows() * u64::from(range.cols()), range.cell_count());
        assert_eq!(R1C1Coord::Absolute(u32::MAX).render('R'), "R4294967296");
    }

    #[test]
    fn whole_column_references_parse() {
        let p = parse_a1("A:A").unwrap();
        assert!(p.is_whole_column());
        assert!(!p.is_whole_row());
        assert_eq!((p.left, p.right), (0, 0));
        assert_eq!((p.top, p.bottom), (0, MAX_ROW));
        // Absolute so that shape normalisation does not depend on the
        // formula's own row.
        assert!(p.abs_top && p.abs_bottom);

        let p = parse_a1("$B:$D").unwrap();
        assert_eq!((p.left, p.right), (1, 3));
        assert!(p.abs_left && p.abs_right);

        let p = parse_a1("Sheet1!C:C").unwrap();
        assert_eq!(p.sheet_name.as_deref(), Some("Sheet1"));
        assert!(p.is_whole_column());
    }

    #[test]
    fn whole_row_references_parse() {
        let p = parse_a1("3:3").unwrap();
        assert!(p.is_whole_row());
        assert!(!p.is_whole_column());
        assert_eq!((p.top, p.bottom), (2, 2));
        assert_eq!((p.left, p.right), (0, MAX_COL as u16));
        assert!(p.abs_left && p.abs_right);

        let p = parse_a1("$5:$9").unwrap();
        assert_eq!((p.top, p.bottom), (4, 8));
        assert!(p.abs_top && p.abs_bottom);

        let p = parse_a1("'Q3 Sales'!3:5").unwrap();
        assert_eq!(p.sheet_name.as_deref(), Some("Q3 Sales"));
        assert_eq!((p.top, p.bottom), (2, 4));
    }

    #[test]
    fn mismatched_axis_shorthand_is_rejected() {
        // `A:3` is not valid Excel syntax in either direction, and must not
        // be quietly accepted as a truncated something.
        assert!(parse_a1("A:3").is_err());
        assert!(parse_a1("3:A").is_err());
    }

    #[test]
    fn r1c1_distinguishes_genuinely_different_shapes() {
        let a = R1C1Ref::from_parsed(&parse_a1("A1").unwrap(), CellRef::new(S0, 0, 1));
        let b = R1C1Ref::from_parsed(&parse_a1("A1").unwrap(), CellRef::new(S0, 0, 2));
        assert_ne!(a, b, "different column offsets must not collapse");
    }

    #[test]
    fn a_reference_names_every_sheet_it_spans() {
        // Tab order, by name, as a workbook would hand them over.
        let tabs = ["Jan", "Feb", "Mar", "Summary"];
        let lookup = |name: &str| {
            tabs.iter()
                .position(|t| t.eq_ignore_ascii_case(name))
                .map(|i| SheetId(i as u16))
        };
        let here = SheetId(3);

        // An unqualified reference names the sheet it was written on.
        let span = parse_a1("B2")
            .unwrap()
            .spanned_sheets(here, lookup)
            .unwrap();
        assert_eq!(span.first(), here);
        assert!(!span.is_multi_sheet());
        assert_eq!(span.iter().collect::<Vec<_>>(), vec![here]);

        // A qualified one names the sheet it names.
        let span = parse_a1("Feb!B2")
            .unwrap()
            .spanned_sheets(here, lookup)
            .unwrap();
        assert_eq!(span.iter().collect::<Vec<_>>(), vec![SheetId(1)]);

        // A 3-D one names every sheet between its ends, inclusive.
        let span = parse_a1("Jan:Mar!B2")
            .unwrap()
            .spanned_sheets(here, lookup)
            .unwrap();
        assert!(span.is_multi_sheet());
        assert_eq!(span.count(), 3);
        assert_eq!(
            span.iter().collect::<Vec<_>>(),
            vec![SheetId(0), SheetId(1), SheetId(2)]
        );
        assert_eq!((span.first(), span.last()), (SheetId(0), SheetId(2)));

        // Written back to front, it is the same span: tab order decides, not
        // which end the formula happened to put first.
        let span = parse_a1("Mar:Jan!B2")
            .unwrap()
            .spanned_sheets(here, lookup)
            .unwrap();
        assert_eq!(span.count(), 3);
        assert_eq!(span.first(), SheetId(0));

        // Either end going missing breaks the whole span, and says which end.
        assert_eq!(
            parse_a1("Jan:Gone!B2")
                .unwrap()
                .spanned_sheets(here, lookup),
            Err("Gone")
        );
        assert_eq!(
            parse_a1("Gone:Mar!B2")
                .unwrap()
                .spanned_sheets(here, lookup),
            Err("Gone")
        );
    }
}
