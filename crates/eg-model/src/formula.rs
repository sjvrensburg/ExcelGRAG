//! Finding and rewriting the cell references inside formula text.
//!
//! Two jobs, both built on one scanner:
//!
//! - **Locating** references, so a formula can be turned into graph edges.
//! - **Normalising** a formula to R1C1 against the cell it lives in, producing a
//!   *shape*. `=A1*2` in B1 and `=A2*2` in B2 both normalise to `=RC[-1]*2`, so a
//!   filled-down column of ten thousand formulas collapses to a single node.
//!
//! Formula text is not parsed into an expression tree here. Shapes only need the
//! references rewritten and everything else left byte-for-byte alone, which is
//! both cheaper and far less likely to be subtly wrong than a full parser.
//!
//! # Telling a reference from a name
//!
//! `A1` is a reference; `Data2023` is a defined name. The rule used here is that
//! a token is a reference exactly when it matches `$?[A-Za-z]{1,3}$?[0-9]+` and
//! addresses a cell within the sheet. That is safe because Excel *forbids*
//! defining a name that looks like a cell reference, so a token of that shape
//! cannot be anything else.
//!
//! Three cases need care, and each is a real source of false positives:
//!
//! - `SUM(A1)` — a name followed by `(` is a function, not a reference.
//! - `1E5` — scientific notation, where `E5` looks exactly like a reference. A
//!   reference is never preceded by an identifier character.
//! - `"A1"` — text inside a string literal, and inside a quoted sheet name, is
//!   never rewritten.

use std::ops::Range;

use crate::address::{parse_a1, CellRef, ParsedRef};

/// A cell or range reference found inside formula text.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceSpan {
    /// Byte range of the whole reference, including any sheet qualifier.
    pub span: Range<usize>,
    /// Byte range of the `A1` part alone, i.e. `span` minus any qualifier.
    /// Equal to `span` when the reference names no sheet.
    pub local: Range<usize>,
    /// The parsed reference.
    pub parsed: ParsedRef,
    /// Whether the reference named a sheet, e.g. `Sheet1!A1`.
    pub qualified: bool,
}

impl ReferenceSpan {
    /// The reference exactly as it was written.
    pub fn text<'a>(&self, formula: &'a str) -> &'a str {
        &formula[self.span.clone()]
    }
}

/// Whether `b` can appear inside an identifier or a defined name.
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
}

/// Find every cell reference in a formula, in order of appearance.
///
/// The input is formula text without a leading `=`, as stored in a workbook.
pub fn scan_references(formula: &str) -> Vec<ReferenceSpan> {
    let mut out = Vec::new();
    scan_references_into(formula, &mut out);
    out
}

/// [`scan_references`] into a caller-owned buffer.
///
/// Grouping scans millions of formulas, so reusing one buffer rather than
/// allocating a fresh `Vec` per cell is worth the slightly clumsier signature.
/// The buffer is cleared first.
pub fn scan_references_into(formula: &str, out: &mut Vec<ReferenceSpan>) {
    out.clear();
    let b = formula.as_bytes();
    let mut i = 0;

    while i < b.len() {
        match b[i] {
            // A double-quoted string literal. Nothing inside is a reference.
            b'"' => i = skip_quoted(b, i, b'"'),
            // A single-quoted span is a sheet name. If a reference follows it,
            // the whole thing — quotes included — is one qualified reference.
            b'\'' => {
                let start = i;
                let after_name = skip_quoted(b, i, b'\'');
                match reference_after_qualifier(formula, b, after_name, start) {
                    Some((span, local, parsed)) => {
                        i = span.end;
                        out.push(ReferenceSpan {
                            span,
                            local,
                            parsed,
                            qualified: true,
                        });
                    }
                    None => i = after_name,
                }
            }
            c if c.is_ascii_alphabetic() || c == b'$' || c == b'_' => {
                let start = i;
                let end = scan_ident(b, i);

                // `Sheet1!A1`: an unquoted name followed by `!` qualifies the
                // reference that comes after it.
                if end < b.len() && b[end] == b'!' {
                    match reference_after_qualifier(formula, b, end, start) {
                        Some((span, local, parsed)) => {
                            i = span.end;
                            out.push(ReferenceSpan {
                                span,
                                local,
                                parsed,
                                qualified: true,
                            });
                            continue;
                        }
                        None => {
                            i = end + 1;
                            continue;
                        }
                    }
                }

                // A bare token. It is a reference only if it looks like one and
                // is not a function call.
                let is_call = end < b.len() && b[end] == b'(';
                let preceded_by_ident = start > 0 && is_ident_byte(b[start - 1]);
                if !is_call && !preceded_by_ident {
                    if let Some(parsed) = parse_cell_token(&formula[start..end]) {
                        out.push(ReferenceSpan {
                            span: start..end,
                            local: start..end,
                            parsed,
                            qualified: false,
                        });
                    }
                }
                i = end;
            }
            // `[1]Sheet1!A1` — an external workbook qualifier.
            b'[' => {
                let close = b[i..].iter().position(|&c| c == b']').map(|p| i + p);
                match close {
                    Some(close) => {
                        let start = i;
                        let after = scan_ident(b, close + 1);
                        if after < b.len() && b[after] == b'!' {
                            match reference_after_qualifier(formula, b, after, start) {
                                Some((span, local, parsed)) => {
                                    i = span.end;
                                    out.push(ReferenceSpan {
                                        span,
                                        local,
                                        parsed,
                                        qualified: true,
                                    });
                                    continue;
                                }
                                None => i = after + 1,
                            }
                        } else {
                            i = close + 1;
                        }
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
}

/// Consume a quoted span starting at `i`, honouring `qq` as an escaped quote.
fn skip_quoted(b: &[u8], mut i: usize, q: u8) -> usize {
    debug_assert_eq!(b[i], q);
    i += 1;
    while i < b.len() {
        if b[i] == q {
            if i + 1 < b.len() && b[i + 1] == q {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    // Unterminated: treat the rest as part of the literal rather than looping.
    b.len()
}

/// Consume an identifier-ish run: letters, digits, `_`, `.` and `$`.
fn scan_ident(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (is_ident_byte(b[i]) || b[i] == b'$') {
        i += 1;
    }
    i
}

/// Parse the reference that follows a sheet qualifier ending at `bang`.
///
/// `bang` indexes the `!`, or the byte just before it for a quoted name.
/// `start` is where the whole qualified reference began. Returns the full span
/// and the parsed reference, or `None` if what follows is not a reference.
#[allow(clippy::type_complexity)]
fn reference_after_qualifier(
    formula: &str,
    b: &[u8],
    bang: usize,
    start: usize,
) -> Option<(Range<usize>, Range<usize>, ParsedRef)> {
    let bang = if bang < b.len() && b[bang] == b'!' {
        bang
    } else {
        return None;
    };
    let local_start = bang + 1;
    let local_end = scan_ident(b, local_start);
    if local_end == local_start {
        return None;
    }
    // Validate the local part on its own before parsing the whole thing, so a
    // qualifier followed by a defined name is not mistaken for a reference.
    parse_cell_token(&formula[local_start..local_end])?;
    let span = start..local_end;
    let parsed = parse_a1(&formula[span.clone()]).ok()?;
    Some((span, local_start..local_end, parsed))
}

/// Parse a bare `A1`-style token, rejecting anything that is not a reference.
fn parse_cell_token(token: &str) -> Option<ParsedRef> {
    let bytes = token.as_bytes();
    let mut i = 0;
    if i < bytes.len() && bytes[i] == b'$' {
        i += 1;
    }
    let letters_start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    // A column is one to three letters; more means a defined name.
    if i == letters_start || i - letters_start > 3 {
        return None;
    }
    if i < bytes.len() && bytes[i] == b'$' {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // The token must be entirely consumed, and must carry a row.
    if i != bytes.len() || i == digits_start {
        return None;
    }
    parse_a1(token).ok()
}

/// Rewrite a formula's references relative to the cell that owns it.
///
/// The result is a *shape*: two formulas share one if and only if they do the
/// same thing to correspondingly-placed cells. Everything that is not a
/// reference is copied through unchanged.
pub fn to_r1c1_shape(formula: &str, anchor: CellRef) -> String {
    let mut out = String::new();
    let mut scratch = Vec::new();
    write_r1c1_shape(formula, anchor, &mut out, &mut scratch);
    out
}

/// [`to_r1c1_shape`] into caller-owned buffers.
///
/// Both buffers are cleared and reused. On a workbook with millions of formulas
/// this is the difference between one allocation per cell and almost none, which
/// dominates the cost of grouping.
pub fn write_r1c1_shape(
    formula: &str,
    anchor: CellRef,
    out: &mut String,
    scratch: &mut Vec<ReferenceSpan>,
) {
    scan_references_into(formula, scratch);
    out.clear();
    if scratch.is_empty() {
        out.push_str(formula);
        return;
    }
    out.reserve(formula.len() + scratch.len() * 4);
    let mut last = 0;
    for r in scratch.iter() {
        out.push_str(&formula[last..r.span.start]);
        // The sheet or workbook qualifier is copied through verbatim rather
        // than rebuilt, which avoids re-quoting it and avoids the allocations
        // that rendering an owned R1C1Ref would need.
        out.push_str(&formula[r.span.start..r.local.start]);
        write_r1c1_coords(&r.parsed, anchor, out);
        last = r.span.end;
    }
    out.push_str(&formula[last..]);
}

/// Write just the `R…C…` part of a reference, relative to `anchor`.
fn write_r1c1_coords(parsed: &ParsedRef, anchor: CellRef, out: &mut String) {
    write_coord(out, parsed.top as i64, parsed.abs_top, anchor.row as i64, 'R');
    write_coord(
        out,
        parsed.left as i64,
        parsed.abs_left,
        anchor.col as i64,
        'C',
    );
    if !parsed.is_single_cell() {
        out.push(':');
        write_coord(
            out,
            parsed.bottom as i64,
            parsed.abs_bottom,
            anchor.row as i64,
            'R',
        );
        write_coord(
            out,
            parsed.right as i64,
            parsed.abs_right,
            anchor.col as i64,
            'C',
        );
    }
}

/// Write one R1C1 coordinate: absolute as `R5`, relative as `R` or `R[-2]`.
fn write_coord(out: &mut String, value: i64, absolute: bool, origin: i64, tag: char) {
    use std::fmt::Write;
    if absolute {
        let _ = write!(out, "{tag}{}", value + 1);
    } else {
        match value - origin {
            0 => out.push(tag),
            d => {
                let _ = write!(out, "{tag}[{d}]");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::SheetId;

    const S: SheetId = SheetId(0);

    fn texts(formula: &str) -> Vec<&str> {
        scan_references(formula)
            .iter()
            .map(|r| r.text(formula))
            .collect()
    }

    #[test]
    fn finds_plain_references() {
        assert_eq!(texts("A1+B2"), ["A1", "B2"]);
        assert_eq!(texts("SUM(A1:A9)"), ["A1", "A9"]);
        assert_eq!(texts("$A$1"), ["$A$1"]);
        assert_eq!(texts("42"), Vec::<&str>::new());
    }

    #[test]
    fn function_names_are_not_references() {
        // The classic false positive: a call whose name parses as a column.
        assert_eq!(texts("LOG10(A1)"), ["A1"]);
        assert_eq!(texts("SUM(A1)"), ["A1"]);
        assert_eq!(texts("IF(A1>0,B1,C1)"), ["A1", "B1", "C1"]);
    }

    #[test]
    fn scientific_notation_is_not_a_reference() {
        // `1E5` must not yield `E5`.
        assert_eq!(texts("1E5"), Vec::<&str>::new());
        assert_eq!(texts("A1*1E5"), ["A1"]);
        assert_eq!(texts("1.5E10+B2"), ["B2"]);
    }

    #[test]
    fn defined_names_are_not_references() {
        // Four letters cannot be a column, so this is a name.
        assert_eq!(texts("Data2023"), Vec::<&str>::new());
        assert_eq!(texts("Data2023+A1"), ["A1"]);
        assert_eq!(texts("_tax*B2"), ["B2"]);
        assert_eq!(texts("my.name1"), Vec::<&str>::new());
    }

    #[test]
    fn string_literals_are_never_scanned() {
        assert_eq!(texts("IF(A1,\"B2\",\"C3\")"), ["A1"]);
        assert_eq!(texts("CONCATENATE(\"A1\",B2)"), ["B2"]);
        // A doubled quote escapes and keeps the literal open.
        assert_eq!(texts("\"a\"\"A1\"\"b\"&C3"), ["C3"]);
    }

    #[test]
    fn sheet_qualified_references_are_whole() {
        assert_eq!(texts("Sheet1!A1"), ["Sheet1!A1"]);
        assert_eq!(texts("'Q3 Sales'!B7"), ["'Q3 Sales'!B7"]);
        assert_eq!(
            texts("Sheet1!A1+'Q3 Sales'!B7"),
            ["Sheet1!A1", "'Q3 Sales'!B7"]
        );
        assert!(scan_references("Sheet1!A1")[0].qualified);
        assert!(!scan_references("A1")[0].qualified);
    }

    #[test]
    fn sheet_name_containing_a_bang_stays_intact() {
        assert_eq!(texts("'Hey! Sales'!B2"), ["'Hey! Sales'!B2"]);
    }

    #[test]
    fn external_workbook_references() {
        assert_eq!(texts("[1]Sheet1!A1"), ["[1]Sheet1!A1"]);
        let r = &scan_references("[1]Sheet1!A1")[0];
        assert_eq!(r.parsed.workbook.as_deref(), Some("1"));
    }

    #[test]
    fn qualifier_followed_by_a_name_is_not_a_reference() {
        // `Sheet1!Total` names a range, not a cell.
        assert_eq!(texts("Sheet1!Total"), Vec::<&str>::new());
    }

    #[test]
    fn out_of_range_tokens_are_rejected() {
        // XFE is past the last column; A0 is not a row.
        assert_eq!(texts("XFE1"), Vec::<&str>::new());
        assert_eq!(texts("A0"), Vec::<&str>::new());
        assert_eq!(texts("XFD1048576"), ["XFD1048576"]);
    }

    #[test]
    fn error_values_are_not_references() {
        assert_eq!(texts("#REF!+A1"), ["A1"]);
        assert_eq!(texts("#N/A"), Vec::<&str>::new());
    }

    #[test]
    fn unterminated_literal_does_not_hang() {
        assert_eq!(texts("CONCAT(\"unclosed"), Vec::<&str>::new());
        assert_eq!(texts("'unclosed"), Vec::<&str>::new());
    }

    #[test]
    fn shape_is_identical_down_a_filled_column() {
        // The whole point: one shape for a whole filled-down column.
        let shapes: Vec<String> = (0..5)
            .map(|row| to_r1c1_shape(&format!("A{}*2", row + 1), CellRef::new(S, row, 1)))
            .collect();
        assert!(shapes.iter().all(|s| *s == shapes[0]), "{shapes:?}");
        assert_eq!(shapes[0], "RC[-1]*2");
    }

    #[test]
    fn shape_distinguishes_genuinely_different_formulas() {
        let a = to_r1c1_shape("A1*2", CellRef::new(S, 0, 1));
        let b = to_r1c1_shape("A1*3", CellRef::new(S, 0, 1));
        let c = to_r1c1_shape("B1*2", CellRef::new(S, 0, 1));
        assert_ne!(a, b, "different constants are different shapes");
        assert_ne!(a, c, "different offsets are different shapes");
    }

    #[test]
    fn shape_keeps_absolute_references_pinned() {
        // An absolute reference is the same cell from every row, so rows that
        // point at it do *not* share a shape with rows that walk down.
        let a = to_r1c1_shape("$A$1*2", CellRef::new(S, 0, 1));
        let b = to_r1c1_shape("$A$1*2", CellRef::new(S, 9, 1));
        assert_eq!(a, b);
        assert_eq!(a, "R1C1*2");
    }

    #[test]
    fn shape_preserves_everything_that_is_not_a_reference() {
        assert_eq!(
            to_r1c1_shape("IF(A1>0,\"yes A1\",SUM(B1:B9))", CellRef::new(S, 0, 3)),
            "IF(RC[-3]>0,\"yes A1\",SUM(RC[-2]:R[8]C[-2]))"
        );
    }

    #[test]
    fn shape_handles_sheet_qualified_references() {
        let s = to_r1c1_shape("Sheet2!A1*2", CellRef::new(S, 4, 1));
        assert_eq!(s, "Sheet2!R[-4]C[-1]*2");
    }

    #[test]
    fn shape_of_a_constant_formula_is_itself() {
        assert_eq!(to_r1c1_shape("1+2", CellRef::new(S, 0, 0)), "1+2");
    }

    #[test]
    fn shape_round_trips_through_scanning_unchanged_text() {
        // Non-reference bytes must survive byte-for-byte, including unicode.
        let f = "IF(A1,\"café — ok\",B1)";
        let out = to_r1c1_shape(f, CellRef::new(S, 0, 2));
        assert!(out.contains("café — ok"), "{out}");
    }
}
