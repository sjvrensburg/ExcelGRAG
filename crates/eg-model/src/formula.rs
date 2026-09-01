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
    !b.is_ascii() || b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
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
            c if !c.is_ascii() => {
                let start = i;
                let end = scan_ident(b, i);
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
                        }
                        None => i = end + 1,
                    }
                } else {
                    // A Unicode name is one token. In particular, do not
                    // rescan an ASCII A1-shaped suffix as a cell reference.
                    i = end;
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

                // `Jan:Dec!A1` — a 3-D reference. Checked before the bare-token
                // path below: `Jan:Dec` alone is exactly the shape the
                // whole-column shorthand `A:C` has, and the only thing that
                // tells a sheet range from a column range apart is whether a
                // `!` follows the second half.
                if end < b.len() && b[end] == b':' {
                    if let Some(bang) = scan_3d_sheet_range_bang(b, end) {
                        match reference_after_qualifier(formula, b, bang, start) {
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
                                i = bang + 1;
                                continue;
                            }
                        }
                    }
                }

                // A bare token. It is a reference only if it looks like one and
                // is not a function call.
                let is_call = end < b.len() && b[end] == b'(';
                let preceded_by_ident = start > 0 && is_ident_byte(b[start - 1]);
                if !is_call && !preceded_by_ident {
                    if let Some(parsed) = parse_cell_token(&formula[start..end]) {
                        // `A1:A4` is one range. Taken as two cells it would
                        // claim to depend on the endpoints and not the middle.
                        let (end, parsed) = match extend_to_range(formula, b, end) {
                            Some(extended) => match parse_a1(&formula[start..extended]) {
                                Ok(range) => (extended, range),
                                Err(_) => (end, parsed),
                            },
                            None => (end, parsed),
                        };
                        out.push(ReferenceSpan {
                            span: start..end,
                            local: start..end,
                            parsed,
                            qualified: false,
                        });
                        i = end;
                        continue;
                    }
                    // `A:A` — a bare column has no row for `parse_cell_token`
                    // to accept, so the whole-column shorthand is tried as its
                    // own shape rather than as a malformed cell.
                    if end < b.len() && b[end] == b':' {
                        if let Some(second_end) = scan_axis_only_after_colon(b, end) {
                            if let Ok(parsed) = parse_a1(&formula[start..second_end]) {
                                out.push(ReferenceSpan {
                                    span: start..second_end,
                                    local: start..second_end,
                                    parsed,
                                    qualified: false,
                                });
                                i = second_end;
                                continue;
                            }
                        }
                    }
                }
                i = end;
            }
            // `3:3` — the whole-row counterpart, a digit run where `A:A`'s is
            // a letter run. Not reached by the arm above, which only matches
            // an alphabetic/`$`/`_` first byte.
            c if c.is_ascii_digit() => {
                let start = i;
                let mut end = i;
                while end < b.len() && b[end].is_ascii_digit() {
                    end += 1;
                }
                let preceded_by_ident = start > 0 && is_ident_byte(b[start - 1]);
                if !preceded_by_ident && end < b.len() && b[end] == b':' {
                    if let Some(second_end) = scan_axis_only_after_colon(b, end) {
                        if let Ok(parsed) = parse_a1(&formula[start..second_end]) {
                            out.push(ReferenceSpan {
                                span: start..second_end,
                                local: start..second_end,
                                parsed,
                                qualified: false,
                            });
                            i = second_end;
                            continue;
                        }
                    }
                }
                // An ordinary number (or the digits half of `1E5`, left for
                // the alphabetic arm to judge next): nothing to record.
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

/// A defined-name reference found in formula text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameSpan {
    /// Byte range of the name itself, excluding any sheet qualifier.
    pub span: Range<usize>,
    /// The sheet that qualified it, as written, for a sheet-scoped name.
    pub sheet_name: Option<String>,
}

impl NameSpan {
    /// The name exactly as it was written.
    pub fn text<'a>(&self, formula: &'a str) -> &'a str {
        &formula[self.span.clone()]
    }
}

/// Find every token in a formula that could name something.
///
/// The complement of [`scan_references`]: this returns the identifiers that
/// scanning deliberately throws away, so a caller holding the workbook's list
/// of defined names can decide which of them are real. It cannot decide that
/// itself — `Tax_Rate` and `SUM` are the same shape of token, and only the
/// workbook knows which one is defined.
///
/// Excluded here, because they are never a defined name:
///
/// - anything that parses as a cell reference, which Excel forbids as a name;
/// - a token immediately followed by `(`, which is a function call;
/// - text inside string literals and quoted sheet names;
/// - the sheet part of a qualified reference, which names a sheet, not a value.
///
/// A structured-table reference such as `Table1[Amount]` yields `Table1`, since
/// a table name is resolved the same way by the caller.
pub fn scan_names(formula: &str) -> Vec<NameSpan> {
    let mut out = Vec::new();
    scan_names_into(formula, &mut out);
    out
}

/// [`scan_names`] into a caller-owned buffer, which is cleared first.
pub fn scan_names_into(formula: &str, out: &mut Vec<NameSpan>) {
    out.clear();
    let b = formula.as_bytes();
    let mut i = 0;

    while i < b.len() {
        match b[i] {
            b'"' => i = skip_quoted(b, i, b'"'),
            // A quoted sheet name. If a reference follows, both parts are
            // consumed; otherwise only the quoted span is, since a name cannot
            // be quoted.
            b'\'' => {
                let start = i;
                let after_name = skip_quoted(b, i, b'\'');
                i = match reference_after_qualifier(formula, b, after_name, start) {
                    Some((span, _, _)) => span.end,
                    None => {
                        // `'My Sheet'!Tax_Rate` — a sheet-scoped name.
                        match qualified_name(formula, b, after_name) {
                            Some(span) => {
                                let end = span.end;
                                let sheet = unquote_sheet_name(&formula[start..after_name]);
                                out.push(NameSpan {
                                    span,
                                    sheet_name: Some(sheet),
                                });
                                end
                            }
                            None => after_name,
                        }
                    }
                };
            }
            // An external workbook qualifier. Its contents name a workbook, and
            // whatever follows is qualified by it, so neither is a bare name.
            b'[' => {
                i = match b[i..].iter().position(|&c| c == b']') {
                    Some(p) => i + p + 1,
                    None => i + 1,
                };
            }
            c if !c.is_ascii() => {
                let start = i;
                let end = scan_ident(b, i);
                if end < b.len() && b[end] == b'!' {
                    let sheet = formula[start..end].to_string();
                    i = match reference_after_qualifier(formula, b, end, start) {
                        Some((span, _, _)) => span.end,
                        None => match qualified_name(formula, b, end) {
                            Some(span) => {
                                let e = span.end;
                                out.push(NameSpan {
                                    span,
                                    sheet_name: Some(sheet),
                                });
                                e
                            }
                            None => end + 1,
                        },
                    };
                } else {
                    let is_call = end < b.len() && b[end] == b'(';
                    if !is_call {
                        out.push(NameSpan {
                            span: start..end,
                            sheet_name: None,
                        });
                    }
                    i = end;
                }
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'\\' => {
                let start = i;
                // Excel allows a name to begin with `\`, but only there, so it
                // is not an identifier byte and `scan_ident` must start past
                // it. Scanning from `start` would return an empty run, leaving
                // `i` where it was: an infinite loop, pushing an empty name on
                // every pass until the allocator gives out.
                let end = scan_ident(b, if c == b'\\' { i + 1 } else { i });

                if end < b.len() && b[end] == b'!' {
                    let sheet = formula[start..end].to_string();
                    i = match reference_after_qualifier(formula, b, end, start) {
                        Some((span, _, _)) => span.end,
                        None => match qualified_name(formula, b, end) {
                            Some(span) => {
                                let e = span.end;
                                out.push(NameSpan {
                                    span,
                                    sheet_name: Some(sheet),
                                });
                                e
                            }
                            None => end + 1,
                        },
                    };
                    continue;
                }

                // `Jan:Dec!A1` — a 3-D reference, not two names either.
                // Checked before the whole-column-shorthand skip below, for
                // the same reason `scan_references_into` checks it first:
                // `Jan:Dec` alone is exactly the shape of the shorthand
                // `A:C`, and a `!` right after the second identifier is what
                // tells the two apart.
                if end < b.len() && b[end] == b':' {
                    if let Some(bang) = scan_3d_sheet_range_bang(b, end) {
                        i = match reference_after_qualifier(formula, b, bang, start) {
                            Some((span, _, _)) => span.end,
                            None => bang + 1,
                        };
                        continue;
                    }
                }

                let is_call = end < b.len() && b[end] == b'(';
                let preceded_by_ident = start > 0 && is_ident_byte(b[start - 1]);
                let is_reference = parse_cell_token(&formula[start..end]).is_some();
                if !is_call && !preceded_by_ident && !is_reference {
                    // `A:A` — a bare column with no row is not a defined name
                    // either, and neither is its partner past the colon; skip
                    // both rather than let the second one be re-scanned as its
                    // own token on the next pass through the loop.
                    if end < b.len() && b[end] == b':' {
                        if let Some(second_end) = scan_axis_only_after_colon(b, end) {
                            if parse_a1(&formula[start..second_end]).is_ok() {
                                i = second_end;
                                continue;
                            }
                        }
                    }
                    out.push(NameSpan {
                        span: start..end,
                        sheet_name: None,
                    });
                }
                i = end;
            }
            c if c.is_ascii_digit() => {
                // A digit-led token is never a name on its own — Excel forbids
                // it — so there is nothing to push here even for `3:3`'s
                // partner. Advance past the whole run rather than one byte at
                // a time, mirroring `scan_references_into`'s digit arm.
                let mut end = i;
                while end < b.len() && b[end].is_ascii_digit() {
                    end += 1;
                }
                i = end;
            }
            _ => i += 1,
        }
    }
}

/// Strip the surrounding quotes from a sheet name, undoubling any inside it.
fn unquote_sheet_name(written: &str) -> String {
    match written
        .strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
    {
        Some(inner) => inner.replace("''", "'"),
        None => written.to_string(),
    }
}

/// Parse the name following a sheet qualifier whose `!` sits at `bang`.
///
/// Returns the name's own span. `None` when what follows is a reference, a
/// function call, or nothing at all.
fn qualified_name(formula: &str, b: &[u8], bang: usize) -> Option<Range<usize>> {
    if bang >= b.len() || b[bang] != b'!' {
        return None;
    }
    let start = bang + 1;
    let end = scan_ident(b, start);
    if end == start || (end < b.len() && b[end] == b'(') {
        return None;
    }
    if parse_cell_token(&formula[start..end]).is_some() {
        return None;
    }
    Some(start..end)
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

/// Extend a reference that turns out to be the first half of a range.
///
/// `scan_ident` stops at the `:`, so `Rates!B2:B100` first parses as the single
/// cell `Rates!B2`. Left there, the second endpoint is then scanned as a bare
/// `B100` on whatever sheet the formula lives on — a reference to a real cell
/// that the workbook never mentioned. Ranges are most of what formulas refer
/// to, so this is not an edge case.
///
/// Returns the byte after the second endpoint, or `None` when what follows the
/// colon is not a plain cell (`A1:INDEX(...)` and the like are left alone).
fn extend_to_range(formula: &str, b: &[u8], end: usize) -> Option<usize> {
    if end >= b.len() || b[end] != b':' {
        return None;
    }
    let second_start = end + 1;
    let second_end = scan_ident(b, second_start);
    if second_end == second_start {
        return None;
    }
    parse_cell_token(&formula[second_start..second_end])?;
    Some(second_end)
}

/// The position of the `!` in `IDENT:IDENT!...` — a 3-D reference's sheet
/// range (`Jan:Dec!A1`) — if `colon` is immediately followed by an
/// identifier run and that run is immediately followed by `!`. `None`
/// otherwise.
///
/// `Jan:Dec` and the whole-column shorthand `A:C` are exactly the same
/// shape, and nothing about the two identifiers themselves tells them apart
/// — `letters_to_col` would happily read `JAN` and `DEC` as column codes.
/// What tells them apart is what comes next: a whole-column reference is
/// never itself sheet-qualified by a trailing `!`, so a `!` right after the
/// second identifier is unambiguous evidence this is a sheet range, checked
/// before the shorthand is ever attempted.
fn scan_3d_sheet_range_bang(b: &[u8], colon: usize) -> Option<usize> {
    debug_assert_eq!(b[colon], b':');
    let second_start = colon + 1;
    let second_end = scan_ident(b, second_start);
    if second_end == second_start {
        return None;
    }
    if second_end < b.len() && b[second_end] == b'!' {
        Some(second_end)
    } else {
        None
    }
}

/// The end of a `:`-following token that is a pure column or row half of a
/// whole-column/row shorthand (`A:A`, `3:3`, and their `$`-prefixed and
/// mixed-endpoint forms like `A:$C` or `3:5`) — optional `$`, then letters or
/// digits but never both.
///
/// `None` when what follows is not that shape: in particular an ordinary cell
/// corner (`A1`) must fall through to [`extend_to_range`] rather than being
/// swallowed here as a truncated column, which is what a naive letters-only
/// scan would do to `A:A1` — the digit-lookahead checks below exist for
/// exactly that.
fn scan_axis_only_after_colon(b: &[u8], colon: usize) -> Option<usize> {
    debug_assert_eq!(b[colon], b':');
    let mut i = colon + 1;
    if i < b.len() && b[i] == b'$' {
        i += 1;
    }
    let start = i;
    if i < b.len() && b[i].is_ascii_alphabetic() {
        while i < b.len() && b[i].is_ascii_alphabetic() {
            i += 1;
        }
        if i < b.len() && b[i].is_ascii_digit() {
            return None;
        }
    } else if i < b.len() && b[i].is_ascii_digit() {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_' || b[i] == b'.') {
            return None;
        }
    } else {
        return None;
    }
    if i == start {
        return None;
    }
    Some(i)
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
    if parse_cell_token(&formula[local_start..local_end]).is_some() {
        // `Sheet1!A1:A4` is one reference, not `Sheet1!A1` and a stray `A4`.
        let local_end = match extend_to_range(formula, b, local_end) {
            Some(extended) if parse_a1(&formula[start..extended]).is_ok() => extended,
            _ => local_end,
        };
        let span = start..local_end;
        let parsed = parse_a1(&formula[span.clone()]).ok()?;
        return Some((span, local_start..local_end, parsed));
    }
    // `Sheet1!A:A` / `Sheet1!3:3` — the whole-column/row shorthand, whose
    // first half has no row (or no column) for `parse_cell_token` to accept.
    if local_end < b.len() && b[local_end] == b':' {
        let second_end = scan_axis_only_after_colon(b, local_end)?;
        let span = start..second_end;
        let parsed = parse_a1(&formula[span.clone()]).ok()?;
        return Some((span, local_start..second_end, parsed));
    }
    None
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

/// Replace the string and numeric literals in formula text with
/// `<text>`/`<number>` placeholders, keeping everything else — references,
/// function names, operators, structure — intact.
///
/// A formula's own literals are the workbook's data as much as any cell's
/// value is: `=IF(A2="Smith, John",B2*0.15,0)` names a person and a rate.
/// Printing the formula unredacted while the cell's *value* shows `<number>`
/// under `--redact-values` would leak exactly what the flag exists to
/// withhold, so this is what a printing path calls on formula text under
/// that flag, the same way it already calls a value redactor on the value.
///
/// References are found with [`scan_references`] and copied through
/// verbatim, so a digit that is a row (`A100`) or a whole-row/column
/// shorthand (`3:3`) is never mistaken for a literal — this walks only the
/// text *between* reference spans. A digit run not preceded by an
/// identifier byte (so `1` in `my.name1` is left alone, but `0.15` in
/// `B2*0.15` is not) is a numeric literal, Excel's own shape: digits, an
/// optional `.digits`, an optional `[eE][+-]?digits` exponent — the same
/// lookahead [`scan_references_into`]'s digit arm uses to keep `1E5` from
/// being misread. A double-quoted span is a string literal; a single-quoted
/// one is sheet-name quoting, not a literal, and is copied through as-is.
pub fn redact_formula_literals(formula: &str) -> String {
    let refs = scan_references(formula);
    let b = formula.as_bytes();
    let mut out = String::with_capacity(formula.len());
    let mut i = 0;
    let mut ref_idx = 0;

    while i < b.len() {
        if ref_idx < refs.len() && refs[ref_idx].span.start == i {
            let span = refs[ref_idx].span.clone();
            out.push_str(&formula[span.clone()]);
            i = span.end;
            ref_idx += 1;
            continue;
        }
        let preceded_by_ident = i > 0 && is_ident_byte(b[i - 1]);
        let starts_number = !preceded_by_ident
            && (b[i].is_ascii_digit()
                || (b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)));
        match b[i] {
            b'"' => {
                let end = skip_quoted(b, i, b'"');
                out.push_str("<text>");
                i = end;
            }
            // A single-quoted span is sheet-name quoting, not a literal, and
            // is copied through whole. Walking into it a character at a time
            // instead put `<number>` over the digits of a sheet called
            // `'Q3 2024'` — every quoted sheet name the reference scanner did
            // *not* claim, which is every one qualifying a defined name
            // (`'Q3 2024'!Tax_Rate`) rather than a cell. That is not
            // redaction, it is a citation the reader can no longer follow.
            b'\'' => {
                let end = skip_quoted(b, i, b'\'');
                out.push_str(&formula[i..end]);
                i = end;
            }
            _ if starts_number => {
                let end = scan_number_literal(b, i);
                out.push_str("<number>");
                i = end;
            }
            _ => {
                let ch = formula[i..]
                    .chars()
                    .next()
                    .expect("i sits on a char boundary");
                out.push_str(&formula[i..i + ch.len_utf8()]);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Consume a numeric literal starting at a digit or at a `.` known to be
/// followed by one: digits, an optional `.digits`, an optional
/// `[eE][+-]?digits` exponent.
fn scan_number_literal(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    i
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
    write_coord(
        out,
        parsed.top as i64,
        parsed.abs_top,
        anchor.row as i64,
        'R',
    );
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
        assert_eq!(texts("SUM(A1:A9)"), ["A1:A9"]);
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
    fn unicode_defined_names_are_one_name_and_not_a_reference_suffix() {
        let formula = "éA1+税率A1";
        assert!(scan_references(formula).is_empty());
        let names: Vec<&str> = scan_names(formula)
            .iter()
            .map(|n| &formula[n.span.clone()])
            .collect();
        assert_eq!(names, ["éA1", "税率A1"]);
    }

    #[test]
    fn unicode_sheet_qualifiers_are_scanned_as_a_whole() {
        let formula = "État!A1+État!Taxe";
        assert_eq!(texts(formula), ["État!A1"]);
        let names = scan_names(formula);
        assert_eq!(&formula[names[0].span.clone()], "Taxe");
        assert_eq!(names[0].sheet_name.as_deref(), Some("État"));
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

    fn names(formula: &str) -> Vec<String> {
        scan_names(formula)
            .iter()
            .map(|n| n.text(formula).to_string())
            .collect()
    }

    #[test]
    fn names_are_what_reference_scanning_discards() {
        assert_eq!(names("Tax_Rate*A1"), ["Tax_Rate"]);
        // A function is not a name, and neither is anything that would parse
        // as a reference.
        assert_eq!(names("SUM(A1:A9)"), Vec::<String>::new());
        assert_eq!(names("SUM(Sales)*Rate"), ["Sales", "Rate"]);
    }

    #[test]
    fn names_inside_literals_are_not_names() {
        assert_eq!(names("IF(A1=\"Tax_Rate\",Tax_Rate,0)"), ["Tax_Rate"]);
    }

    #[test]
    fn a_sheet_scoped_name_keeps_its_sheet() {
        let f = "Sheet1!Tax_Rate+1";
        let found = scan_names(f);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].text(f), "Tax_Rate");
        assert_eq!(found[0].sheet_name.as_deref(), Some("Sheet1"));

        let f = "'My Sheet'!Tax_Rate";
        let found = scan_names(f);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].text(f), "Tax_Rate");
        assert_eq!(found[0].sheet_name.as_deref(), Some("My Sheet"));
    }

    #[test]
    fn a_qualified_reference_names_nothing() {
        // The sheet part names a sheet, not a value, and the cell part is a
        // reference. Neither is a defined name.
        assert_eq!(names("Sheet1!A1+'My Sheet'!B2"), Vec::<String>::new());
        assert_eq!(names("[1]Sheet1!A1"), Vec::<String>::new());
    }

    #[test]
    fn a_name_starting_with_a_backslash_terminates() {
        // Excel allows `\` as a name's first character. It is not an
        // identifier byte, so scanning once ran forever on it, pushing an
        // empty name each pass until the process ran out of memory. The
        // assertion on the text is what proves the fix consumed the byte
        // rather than merely skipping it.
        assert_eq!(names("\\Rate+1"), ["\\Rate"]);
        assert_eq!(names("SUM(\\A,\\B)"), ["\\A", "\\B"]);
        // A lone backslash still has to advance.
        assert_eq!(names("\\"), ["\\"]);
        assert_eq!(names("1+\\"), ["\\"]);
    }

    #[test]
    fn a_table_reference_yields_the_table_name() {
        assert_eq!(names("SUM(Table1[Amount])"), ["Table1"]);
    }

    #[test]
    fn scientific_notation_is_not_a_name() {
        // `1E5` is one number. The reference scanner has the same trap.
        assert_eq!(names("1E5+2"), Vec::<String>::new());
    }

    #[test]
    fn a_range_is_one_reference_not_two_cells() {
        // Taken as two cells, `SUM(Rates!B2:B100)` yields a reference to
        // `Rates!B2` and a bare `B100` — a cell on the formula's own sheet
        // that the workbook never mentioned, and a dependency that does not
        // exist. It also loses every row between the endpoints.
        let f = "SUM(Rates!B2:B100)";
        let refs = scan_references(f);
        assert_eq!(refs.len(), 1, "{refs:#?}");
        assert_eq!(refs[0].text(f), "Rates!B2:B100");
        assert_eq!(refs[0].parsed.sheet_name.as_deref(), Some("Rates"));
        assert_eq!((refs[0].parsed.top, refs[0].parsed.bottom), (1, 99));

        let f = "A1:A4";
        let refs = scan_references(f);
        assert_eq!(refs.len(), 1, "{refs:#?}");
        assert_eq!((refs[0].parsed.top, refs[0].parsed.bottom), (0, 3));
    }

    #[test]
    fn whole_column_and_row_references_are_found() {
        // Relative, absolute, sheet-qualified (bare and quoted): every form
        // that produced zero references before must now yield exactly one.
        assert_eq!(texts("SUM(A:A)"), ["A:A"]);
        assert_eq!(texts("SUM($A:$C)"), ["$A:$C"]);
        assert_eq!(texts("SUM(3:3)"), ["3:3"]);
        assert_eq!(texts("SUM($3:$5)"), ["$3:$5"]);
        assert_eq!(texts("SUM(Sheet1!B:B)"), ["Sheet1!B:B"]);
        assert_eq!(texts("SUM(Sheet1!3:3)"), ["Sheet1!3:3"]);
        assert_eq!(texts("SUM('Q3 Sales'!C:C)"), ["'Q3 Sales'!C:C"]);
        assert_eq!(texts("SUM('Q3 Sales'!3:3)"), ["'Q3 Sales'!3:3"]);

        let r = &scan_references("A:C")[0];
        assert_eq!((r.parsed.left, r.parsed.right), (0, 2));
        assert!(r.parsed.is_whole_column());

        let r = &scan_references("Sheet1!B:B")[0];
        assert!(r.qualified);
        assert_eq!(r.parsed.sheet_name.as_deref(), Some("Sheet1"));
        assert!(r.parsed.is_whole_column());
    }

    #[test]
    fn a_bare_column_or_row_is_not_a_name() {
        // Before the fix, `SUM(A:A)` scanned zero references and `scan_names`
        // picked up the endpoints as `["A", "A"]` — a wrong `REFERENCES_NAME`
        // edge waiting to happen if the workbook defined a name `A`.
        assert_eq!(names("SUM(A:A)"), Vec::<String>::new());
        assert_eq!(names("SUM(3:3)"), Vec::<String>::new());
        assert_eq!(names("SUM(Sheet1!B:B)"), Vec::<String>::new());
    }

    #[test]
    fn a_3d_reference_is_not_mistaken_for_the_whole_column_shorthand() {
        // `Jan:Dec` and the whole-column shorthand `A:C` are the identical
        // shape — `letters_to_col` reads `JAN`/`DEC` as column codes just as
        // happily as `A`/`C` — and the whole-column fix (C1) briefly broke
        // this: `SUM(Jan:Mar!B2)` scanned `Jan:Mar` as a bogus column range
        // and left a stray unqualified `B2` behind. A trailing `!` is what
        // tells the two apart, and must be checked first.
        assert_eq!(texts("SUM(Jan:Mar!B2)"), ["Jan:Mar!B2"]);
        let r = &scan_references("Jan:Mar!B2")[0];
        assert!(r.qualified);
        assert_eq!(r.parsed.sheet_name.as_deref(), Some("Jan"));
        assert_eq!(r.parsed.end_sheet_name.as_deref(), Some("Mar"));
        assert!(!r.parsed.is_whole_column());

        // Long sheet names (more than the three letters a column code could
        // ever be) must work too — column-shaped parsing was never going to
        // accept them, but the `!`-first check must not depend on that.
        assert_eq!(texts("SUM(January:December!B2)"), ["January:December!B2"]);

        // And a genuine whole-column reference, with no `!` after the second
        // half, must still be read as one.
        assert_eq!(texts("SUM(Jan:Mar)"), ["Jan:Mar"]);
        assert!(scan_references("Jan:Mar")[0].parsed.is_whole_column());

        // `scan_names` must not pick up either sheet name as a defined name.
        assert_eq!(names("SUM(Jan:Mar!B2)"), Vec::<String>::new());
    }

    #[test]
    fn a_colon_not_followed_by_a_cell_ends_the_reference() {
        // The intersection and function forms must not swallow what follows.
        let f = "A1:INDEX(B:B,2)";
        let refs = scan_references(f);
        assert_eq!(refs[0].text(f), "A1");
    }

    #[test]
    fn a_range_keeps_its_shape_when_normalised() {
        // The shape path was correct even while ranges scanned as two cells,
        // because both endpoints were rewritten independently. That is why
        // this defect survived grouping: it is invisible here.
        let anchor = CellRef::new(SheetId(0), 4, 3);
        assert_eq!(
            to_r1c1_shape("SUM(D1:D4)", anchor),
            to_r1c1_shape("SUM(D2:D5)", CellRef::new(SheetId(0), 5, 3))
        );
    }

    #[test]
    fn redaction_replaces_literals_and_keeps_structure() {
        assert_eq!(
            redact_formula_literals("IF(A2=\"Smith, John\",B2*0.15,0)"),
            "IF(A2=<text>,B2*<number>,<number>)"
        );
    }

    #[test]
    fn redaction_leaves_a_formula_with_only_references_unchanged() {
        assert_eq!(redact_formula_literals("A1+A2"), "A1+A2");
        assert_eq!(
            redact_formula_literals("SUM(Sheet1!A1:B9)"),
            "SUM(Sheet1!A1:B9)"
        );
        // References and `FALSE` (a keyword, not a literal this scanner
        // touches) survive; the column index `2` is a genuine numeric
        // literal and is redacted like any other.
        assert_eq!(
            redact_formula_literals("VLOOKUP(A1,Rates!A:B,2,FALSE)"),
            "VLOOKUP(A1,Rates!A:B,<number>,FALSE)"
        );
    }

    #[test]
    fn redaction_handles_scientific_notation_as_one_literal() {
        assert_eq!(redact_formula_literals("A1*1E5"), "A1*<number>");
        assert_eq!(redact_formula_literals("1.5E-3+B2"), "<number>+B2");
    }

    #[test]
    fn redaction_does_not_touch_digits_inside_a_name() {
        // `my.name1` is a legal defined name (see
        // `defined_names_are_not_references`); the trailing `1` must survive.
        assert_eq!(redact_formula_literals("my.name1*2"), "my.name1*<number>");
    }

    #[test]
    fn redaction_leaves_sheet_name_quoting_alone() {
        // The quotes around a sheet name are not a string literal.
        assert_eq!(
            redact_formula_literals("'Q3 Sales'!A1+\"total\""),
            "'Q3 Sales'!A1+<text>"
        );
    }

    #[test]
    fn redaction_handles_a_string_containing_formula_punctuation() {
        assert_eq!(
            redact_formula_literals("CONCATENATE(\"a, b (c)\",A1)"),
            "CONCATENATE(<text>,A1)"
        );
    }

    #[test]
    fn redaction_survives_a_doubled_quote_inside_a_string() {
        assert_eq!(
            redact_formula_literals("A1&\"say \"\"hi\"\"\""),
            "A1&<text>"
        );
    }

    #[test]
    fn redaction_leaves_a_quoted_sheet_name_alone_even_when_no_reference_follows() {
        // `'Q3 2024'!Tax_Rate` qualifies a *name*, so the reference scanner
        // claims none of it — and the redactor then walked the quoted span a
        // character at a time and turned the year into `<number>`, leaving a
        // sheet name no reader could follow back.
        assert_eq!(
            redact_formula_literals("'Q3 2024'!Tax_Rate*2"),
            "'Q3 2024'!Tax_Rate*<number>"
        );
        assert_eq!(
            redact_formula_literals("SUM('Sheet 1'!A1:A9)*3"),
            "SUM('Sheet 1'!A1:A9)*<number>"
        );
    }

    #[test]
    fn redaction_is_utf8_safe() {
        let f = "IF(A1,\"café — ok\",B1*2)";
        assert_eq!(redact_formula_literals(f), "IF(A1,<text>,B1*<number>)");
    }
}
