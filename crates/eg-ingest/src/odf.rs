//! OpenDocument formula text, translated into the A1 the workspace speaks.
//!
//! For ODS, calamine hands back the `table:formula` attribute verbatim, and
//! that attribute is OpenFormula, not A1:
//!
//! ```text
//! of:=VLOOKUP([.D2];[Rates.$A$4:.$B$7];2;FALSE())
//! ```
//!
//! References are bracketed and carry their sheet behind a `.`, arguments are
//! separated by `;`, and the whole thing wears its formula language as a
//! namespace prefix. Nothing downstream of this crate reads that. Left
//! untranslated it does not fail loudly — it fails as *absence*:
//! `scan_references` finds no references, so the graph gets no dependency
//! edges; `to_r1c1_shape` normalises nothing, so a filled-down column of two
//! thousand identical formulas becomes two thousand distinct shapes instead of
//! one; and `eg-eval` refuses every cell with a parse error. On the demo
//! workbook that was 0 edges against xlsx's 19, and 14,010 formula groups
//! against 19.
//!
//! Translating here rather than teaching each layer a second dialect keeps the
//! rule that makes the rest of the workspace simple: **one formula syntax
//! exists, and it is A1**. `eg-ingest` is where a format's peculiarities stop.
//!
//! What cannot be translated is left exactly as written, bracket and all, so
//! that it fails to parse downstream and the formula is refused. That is the
//! deliberate choice: a reference this does not understand must not be quietly
//! turned into a plausible local one, because a wrong edge is worse than a
//! missing formula. `load()` counts them into a warning per sheet.

use eg_model::quote_sheet_name;

/// Translate one ODF formula into A1 text, with the count of references that
/// could not be translated and were left as written.
///
/// `own_sheet` is the sheet the formula lives on, needed only for the rare
/// range whose *second* endpoint names a sheet and whose first does not.
pub(crate) fn to_a1(formula: &str, own_sheet: &str) -> (String, u32) {
    let src = strip_namespace(formula);
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut untranslated = 0;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            // A string literal can hold anything — `;`, `[`, `!` — and none of
            // it is syntax.
            b'"' => {
                let end = skip_double_quoted(bytes, i);
                out.push_str(&src[i..end]);
                i = end;
            }
            b'[' => {
                let Some(end) = closing_bracket(src, i) else {
                    // Unterminated: copy the remainder and stop looking.
                    untranslated += 1;
                    out.push_str(&src[i..]);
                    break;
                };
                match reference(&src[i + 1..end], Some(own_sheet)) {
                    Some(a1) => out.push_str(&a1),
                    None => {
                        untranslated += 1;
                        out.push_str(&src[i..=end]);
                    }
                }
                i = end + 1;
            }
            // Argument separator, and the column separator inside an array
            // literal; both are `,` in A1.
            b';' => {
                out.push(',');
                i += 1;
            }
            // Row separator inside an array literal.
            b'|' => {
                out.push(';');
                i += 1;
            }
            // Union, which A1 spells with the argument separator.
            b'~' => {
                out.push(',');
                i += 1;
            }
            // Intersection, which A1 spells with a space. Reached only when
            // the `#…!` arm below has not already claimed the `!` as the tail
            // of an error literal.
            b'!' => {
                out.push(' ');
                i += 1;
            }
            b'#' => {
                let end = error_literal(bytes, i);
                out.push_str(&src[i..end]);
                i = end;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let end = identifier(bytes, i);
                let call = bytes.get(end) == Some(&b'(');
                out.push_str(function_name(&src[i..end], call));
                i = end;
            }
            _ => {
                let end = next_char(bytes, i);
                out.push_str(&src[i..end]);
                i = end;
            }
        }
    }

    (out, untranslated)
}

/// Strip the formula language's namespace prefix, and any `=`.
///
/// ODF writes the language into the text: `of:=` for OpenFormula, `oooc:=` and
/// `msoxl:=` from older writers. The prefix is checked rather than assumed —
/// only a short alphanumeric run before the `:=` counts — so that a formula
/// which happens to contain `:=` is left alone.
fn strip_namespace(formula: &str) -> &str {
    let text = formula.trim();
    if let Some(at) = text.find(":=") {
        if (1..=8).contains(&at) && text[..at].bytes().all(|c| c.is_ascii_alphanumeric()) {
            return &text[at + 2..];
        }
    }
    text.strip_prefix('=').unwrap_or(text)
}

/// Translate the inside of one `[...]` reference, or `None` to leave it alone.
///
/// `own_sheet` is `None` when there is no containing formula to inherit from,
/// which is how an address outside formula text is translated; a reference that
/// would need it then comes back untranslated rather than guessed.
fn reference(body: &str, own_sheet: Option<&str>) -> Option<String> {
    let body = body.trim();

    // `[#REF!]`, and `[#REF!.A1]` where the sheet itself was deleted. The
    // workbook really does carry an error here, so saying so is not a guess.
    if body.starts_with('#') {
        return body.starts_with("#REF!").then(|| "#REF!".to_string());
    }
    // `['file:///books/rates.ods'#$Rates.A1]` — a reference into another
    // workbook. A1 writes those as `[1]Rates!A1`, keyed by an index into a
    // link table calamine does not expose (see `Capabilities::external_links`),
    // so there is nothing honest to translate it to.
    if body.contains('#') {
        return None;
    }

    let (first, second) = split_endpoints(body)?;
    let (start_sheet, start_cell) = endpoint(first)?;

    let Some(second) = second else {
        return Some(match start_sheet {
            Some(name) => format!("{}!{start_cell}", quote_sheet_name(&name)),
            None => start_cell.to_string(),
        });
    };
    let (end_sheet, end_cell) = endpoint(second)?;

    // `[Jan.B2:Mar.B2]` is Excel's `Jan:Mar!B2`, not `Jan:Mar!B2:B2` — a range
    // whose corners are the same cell is that cell. Written the long way, an
    // ods formula and the identical xlsx one would differ in text for no
    // reason, and the parity test compares that text.
    let local = if start_cell == end_cell && is_whole_cell(start_cell) {
        start_cell.to_string()
    } else {
        format!("{start_cell}:{end_cell}")
    };

    match (start_sheet, end_sheet) {
        (None, None) => Some(local),
        (start, end) => {
            // An endpoint that names no sheet takes the other's: ODF's second
            // endpoint inherits from the first (`[Rates.$A$4:.$B$7]` is all on
            // `Rates`), and a first that names none is on the formula's sheet.
            let start = match start {
                Some(name) => name,
                None => own_sheet?.to_string(),
            };
            let end = end.unwrap_or_else(|| start.clone());
            Some(if start == end {
                format!("{}!{local}", quote_sheet_name(&start))
            } else {
                format!("{}!{local}", quote_sheet_span(&start, &end))
            })
        }
    }
}

/// Translate an OpenDocument *address* into A1.
///
/// Outside formula text ODF writes an address unbracketed — a defined name's
/// target arrives as `$Rates.$B$11` rather than `Rates!$B$11` — and everything
/// that resolves a name parses it as A1. Left alone it is a name that resolves
/// to nothing, which `eg check` reports as a refusal rather than a miss, so it
/// does not even show up as wrong.
///
/// `None` when it is not an address this understands, in which case the caller
/// keeps what the file said.
pub(crate) fn address_to_a1(address: &str) -> Option<String> {
    reference(address.trim(), None)
}

/// Split a reference body at its range `:`, respecting quoted sheet names.
///
/// `None` when there is more than one, which is a cuboid A1 cannot write.
fn split_endpoints(body: &str) -> Option<(&str, Option<&str>)> {
    let bytes = body.as_bytes();
    let mut at = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = skip_single_quoted(bytes, i),
            b':' => {
                if at.is_some() {
                    return None;
                }
                at = Some(i);
                i += 1;
            }
            _ => i += 1,
        }
    }
    Some(match at {
        Some(at) => (&body[..at], Some(&body[at + 1..])),
        None => (body, None),
    })
}

/// One endpoint: its sheet name, if it names one, and its cell part.
///
/// The `.` is always written, so `.B2` is "this sheet, B2" and `Rates.B2` is
/// the qualified form.
fn endpoint(text: &str) -> Option<(Option<String>, &str)> {
    // A `$` before the sheet name marks the *sheet* reference absolute, which
    // A1 has no way to write and nothing downstream distinguishes.
    let text = text.trim().strip_prefix('$').unwrap_or(text.trim());

    let (sheet, rest) = if text.starts_with('\'') {
        let end = skip_single_quoted(text.as_bytes(), 0);
        if end < 2 || !text[..end].ends_with('\'') {
            return None;
        }
        (Some(text[1..end - 1].replace("''", "'")), &text[end..])
    } else {
        // An unquoted ODF sheet name cannot contain a `.`, so the first one
        // separates the sheet from the cell.
        let dot = text.find('.')?;
        let name = &text[..dot];
        ((!name.is_empty()).then(|| name.to_string()), &text[dot..])
    };

    let cell = rest.strip_prefix('.')?;
    cell_part(cell).is_some().then_some((sheet, cell))
}

/// The letter and digit counts of a cell part, or `None` if it is not one:
/// optional `$`, letters, optional `$`, digits, and nothing after.
///
/// Both halves being optional is what admits the endpoints of a whole-column
/// or whole-row shorthand (`[.A:.A]`, `[.3:.3]`).
fn cell_part(text: &str) -> Option<(usize, usize)> {
    let text = text.strip_prefix('$').unwrap_or(text);
    let letters = text.bytes().take_while(|c| c.is_ascii_alphabetic()).count();
    let rest = &text[letters..];
    let rest = rest.strip_prefix('$').unwrap_or(rest);
    let digits = rest.bytes().take_while(|c| c.is_ascii_digit()).count();
    (digits == rest.len() && letters + digits > 0).then_some((letters, digits))
}

/// Whether a cell part names one cell, rather than a bare column or row.
fn is_whole_cell(text: &str) -> bool {
    cell_part(text).is_some_and(|(letters, digits)| letters > 0 && digits > 0)
}

/// Write a 3-D sheet span the way Excel does, quoting the pair as a whole.
///
/// `'Q1 Jan:Q1 Mar'!B2`, not `'Q1 Jan':'Q1 Mar'!B2` — which is also the form
/// `eg_model::parse_a1` splits on.
fn quote_sheet_span(start: &str, end: &str) -> String {
    if quote_sheet_name(start) == start && quote_sheet_name(end) == end {
        format!("{start}:{end}")
    } else {
        format!(
            "'{}:{}'",
            start.replace('\'', "''"),
            end.replace('\'', "''")
        )
    }
}

/// Strip the namespace an ODF function name wears, when there is an A1 name
/// underneath.
///
/// `COM.MICROSOFT.` marks a function ODF borrowed from Excel and `LEGACY.` one
/// of Excel's superseded names, so both are the A1 spelling with a prefix.
/// `ORG.OPENOFFICE.` is deliberately left alone: those have no Excel
/// equivalent, and stripping the prefix would produce a name that either means
/// nothing or, worse, means something else.
fn function_name(run: &str, is_call: bool) -> &str {
    if !is_call {
        return run;
    }
    for prefix in ["COM.MICROSOFT.", "LEGACY."] {
        if run.len() > prefix.len() && run[..prefix.len()].eq_ignore_ascii_case(prefix) {
            return &run[prefix.len()..];
        }
    }
    run
}

/// The index just past a `[...]`, honouring quoted sheet names that hold a `]`.
fn closing_bracket(src: &str, open: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = skip_single_quoted(bytes, i),
            b']' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// The index just past a `'`-quoted run starting at `i`, where `''` escapes a
/// quote. Returns the end of the input if it is unterminated.
fn skip_single_quoted(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

/// The index just past a `"`-quoted string literal, where `""` escapes a quote.
fn skip_double_quoted(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if bytes.get(i + 1) == Some(&b'"') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

/// The index just past an error literal — `#N/A`, `#DIV/0!`, `#NAME?`.
///
/// This exists so the `!` ending most of them is not read as the intersection
/// operator and replaced with a space.
fn error_literal(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'/') {
        i += 1;
    }
    if matches!(bytes.get(i), Some(b'!') | Some(b'?')) {
        i += 1;
    }
    i
}

/// The index just past an identifier run: letters, digits, `_` and the `.`
/// that ODF function names are built from.
fn identifier(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
    {
        i += 1;
    }
    i
}

/// The index just past the UTF-8 character starting at `i`.
fn next_char(bytes: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < bytes.len() && bytes[j] & 0xC0 == 0x80 {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The translation, dropping the untranslated count.
    fn a1(formula: &str) -> String {
        to_a1(formula, "Debtors").0
    }

    #[test]
    fn the_shapes_the_demo_workbook_actually_contains() {
        // Every one of these is a real formula from `tests/fixtures/demo`, and
        // the expected text is what the *same* workbook's xlsx holds — so
        // these assertions are what makes the parity test's formula-text
        // comparison possible rather than a hope.
        assert_eq!(
            a1(r#"of:=IF([.F2]<=30;"Current";IF([.F2]<=60;"31 to 60 days";"Over 60 days"))"#),
            r#"IF(F2<=30,"Current",IF(F2<=60,"31 to 60 days","Over 60 days"))"#
        );
        assert_eq!(
            a1("of:=VLOOKUP([.D2];[Rates.$A$4:.$B$7];2;FALSE())"),
            "VLOOKUP(D2,Rates!$A$4:$B$7,2,FALSE())"
        );
        assert_eq!(
            a1("of:=PV([.H2]/12;[.F2]/30;0;-[.E2])"),
            "PV(H2/12,F2/30,0,-E2)"
        );
        assert_eq!(
            a1("of:=SUM([Debtors.$E$2:.$E$2001])"),
            "SUM(Debtors!$E$2:$E$2001)"
        );
        assert_eq!(a1("of:=ROUND([.B7]*Tax_Rate;2)"), "ROUND(B7*Tax_Rate,2)");
        assert_eq!(a1("of:=[.F2]/[.E2]"), "F2/E2");
    }

    #[test]
    fn a_range_across_sheets_becomes_a_3d_reference() {
        // The one shape where ODF and A1 disagree about more than punctuation:
        // ODF names a sheet per endpoint, A1 names a span once.
        assert_eq!(a1("of:=SUM([Jan.B2:Mar.B2])"), "SUM(Jan:Mar!B2)");
        assert_eq!(a1("of:=SUM([Jan.B2:Mar.C5])"), "SUM(Jan:Mar!B2:C5)");
    }

    #[test]
    fn a_range_whose_corners_are_one_cell_is_that_cell() {
        // `Jan:Mar!B2:B2` would mean the same thing and read differently, and
        // the parity test compares text.
        assert_eq!(a1("of:=[Jan.B2:Jan.B2]"), "Jan!B2");
        assert_eq!(a1("of:=[.B2:.B2]"), "B2");
        // Not a whole cell, so not collapsible: `A:A` is a column, `A` is not
        // a reference at all.
        assert_eq!(a1("of:=SUM([.A:.A])"), "SUM(A:A)");
        assert_eq!(a1("of:=SUM([.3:.3])"), "SUM(3:3)");
    }

    #[test]
    fn an_endpoint_that_names_no_sheet_takes_the_others() {
        // ODF's second endpoint inherits from the first.
        assert_eq!(a1("of:=SUM([Rates.$A$4:.$B$7])"), "SUM(Rates!$A$4:$B$7)");
        // And a first that names none is on the formula's own sheet, which is
        // the only reason this function needs to be told what that is.
        assert_eq!(a1("of:=SUM([.B2:Mar.B2])"), "SUM(Debtors:Mar!B2)");
        assert_eq!(a1("of:=SUM([.B2:Debtors.B9])"), "SUM(Debtors!B2:B9)");
    }

    #[test]
    fn sheet_names_are_quoted_the_way_excel_quotes_them() {
        assert_eq!(a1("of:=['Debt Rates'.$A$1]"), "'Debt Rates'!$A$1");
        // A 3-D span is quoted as a whole — which is also how `parse_a1`
        // splits it back apart.
        assert_eq!(
            a1("of:=SUM(['Q1 Jan'.B2:'Q1 Mar'.B2])"),
            "SUM('Q1 Jan:Q1 Mar'!B2)"
        );
        // An embedded quote doubles, on both sides of the translation.
        assert_eq!(a1("of:=['Bob''s Sheet'.A1]"), "'Bob''s Sheet'!A1");
        // A `$` before the name marks the sheet reference absolute, which A1
        // cannot write and nothing here distinguishes.
        assert_eq!(a1("of:=[$Rates.A1]"), "Rates!A1");
    }

    #[test]
    fn punctuation_that_means_something_different_in_a1() {
        // `;` separates arguments, `~` is union, `!` is intersection.
        assert_eq!(a1("of:=SUM([.A1];[.B1])"), "SUM(A1,B1)");
        assert_eq!(a1("of:=SUM([.A1:.A5]~[.C1:.C5])"), "SUM(A1:A5,C1:C5)");
        assert_eq!(a1("of:=[.A1:.A5]![.B1:.C9]"), "A1:A5 B1:C9");
        // In an array literal `;` separates columns and `|` separates rows;
        // A1 spells those `,` and `;`.
        assert_eq!(a1("of:=SUM({1;2|3;4})"), "SUM({1,2;3,4})");
    }

    #[test]
    fn an_error_literal_keeps_its_trailing_bang() {
        // Without this, `#DIV/0!` would come out as `#DIV/0 ` — the `!` read
        // as the intersection operator.
        assert_eq!(a1("of:=IFERROR([.A1];#DIV/0!)"), "IFERROR(A1,#DIV/0!)");
        assert_eq!(a1("of:=ISNA(#N/A)"), "ISNA(#N/A)");
        assert_eq!(a1("of:=[#REF!]+1"), "#REF!+1");
        // A reference whose sheet was deleted is still just `#REF!`.
        assert_eq!(a1("of:=[#REF!.A1]"), "#REF!");
    }

    #[test]
    fn nothing_inside_a_string_literal_is_syntax() {
        assert_eq!(
            a1(r#"of:=IF([.A1];"a;b[.C1]!~";"")"#),
            r#"IF(A1,"a;b[.C1]!~","")"#
        );
        // A doubled quote is an escape, not the end of the literal.
        assert_eq!(
            a1(r#"of:=[.A1]&"say ""hi"";now""#),
            r#"A1&"say ""hi"";now""#
        );
    }

    #[test]
    fn a_function_name_keeps_its_namespace_only_when_it_needs_it() {
        // These two are the A1 spelling with a prefix bolted on.
        assert_eq!(
            a1("of:=COM.MICROSOFT.CEILING.MATH([.A1];2)"),
            "CEILING.MATH(A1,2)"
        );
        assert_eq!(a1("of:=LEGACY.NORMSDIST([.A1])"), "NORMSDIST(A1)");
        // This one has no A1 equivalent, so it keeps its name and gets refused
        // by that name rather than mistaken for something that exists.
        assert_eq!(
            a1("of:=ORG.OPENOFFICE.ERRORTYPE([.A1])"),
            "ORG.OPENOFFICE.ERRORTYPE(A1)"
        );
        // Only a call is a function name: a defined name that happens to start
        // the same way is left alone.
        assert_eq!(a1("of:=LEGACY.RATE+1"), "LEGACY.RATE+1");
    }

    #[test]
    fn what_cannot_be_translated_is_left_exactly_as_written() {
        // An external workbook: A1 writes it as `[1]Rates!A1`, keyed by a link
        // table calamine does not expose. Guessing `Rates!A1` would silently
        // point at a local sheet that may not even exist.
        let (text, untranslated) = to_a1("of:=['file:///books/r.ods'#$Rates.A1]+1", "Debtors");
        assert_eq!(text, "['file:///books/r.ods'#$Rates.A1]+1");
        assert_eq!(untranslated, 1);
        // Left bracketed, it does not parse as a reference, so the formula is
        // refused rather than answered wrongly.
        assert!(eg_model::scan_references(&text).is_empty());

        // A cuboid, which A1 has no syntax for at all.
        assert_eq!(to_a1("of:=[Jan.A1:Feb.B2:Mar.C3]", "Debtors").1, 1);
        // And a truncated one.
        assert_eq!(to_a1("of:=SUM([.A1", "Debtors").1, 1);
    }

    #[test]
    fn a_defined_names_target_is_an_address_not_a_formula() {
        // Unbracketed, because ODF only brackets a reference inside formula
        // text. This is the form `table:cell-range-address` holds.
        assert_eq!(address_to_a1("$Rates.$B$11").unwrap(), "Rates!$B$11");
        assert_eq!(
            address_to_a1("$Rates.$A$4:$Rates.$B$7").unwrap(),
            "Rates!$A$4:$B$7"
        );
        assert_eq!(
            address_to_a1("$'Debt Rates'.$A$1").unwrap(),
            "'Debt Rates'!$A$1"
        );
        // No containing formula, so nothing to inherit a missing sheet from:
        // refused rather than attached to a guess.
        assert_eq!(address_to_a1("$Jan.B2:.B2"), Some("Jan!B2".to_string()));
        assert_eq!(address_to_a1(".B2:$Mar.B2"), None);
        assert_eq!(address_to_a1("not an address"), None);
    }

    #[test]
    fn a_formula_that_is_already_a1_is_left_alone() {
        // The namespace prefix is checked, not assumed, so this stays put even
        // though it contains a `:`.
        assert_eq!(to_a1("=SUM(A1:B2)", "Debtors").0, "SUM(A1:B2)");
        assert_eq!(to_a1("SUM(A1:B2)", "Debtors").0, "SUM(A1:B2)");
    }

    #[test]
    fn text_outside_ascii_survives_byte_by_byte_walking() {
        assert_eq!(a1(r#"of:=[.A1]&"café ☕""#), r#"A1&"café ☕""#);
        assert_eq!(a1("of:=[Café.A1]"), "Café!A1");
    }

    #[test]
    fn every_reference_the_translation_emits_parses_back() {
        // The point of the exercise: `scan_references` is what the graph and
        // the evaluator use, and before this it found nothing at all in an ods
        // formula.
        for formula in [
            "of:=VLOOKUP([.D2];[Rates.$A$4:.$B$7];2;FALSE())",
            "of:=SUM([Jan.B2:Mar.B2])",
            "of:=SUM(['Q1 Jan'.B2:'Q1 Mar'.B2])",
            "of:=INDEX([Rates.$B$4:.$B$7];MATCH(\"Business\";[Rates.$A$4:.$A$7];0))",
        ] {
            let (text, untranslated) = to_a1(formula, "Debtors");
            assert_eq!(untranslated, 0, "{formula}");
            let found = eg_model::scan_references(&text);
            assert!(!found.is_empty(), "no references in {text:?}");
            for span in found {
                let written = span.text(&text);
                let reparsed = eg_model::parse_a1(written)
                    .unwrap_or_else(|e| panic!("{text:?}: {written:?} {e:?}"));
                assert_eq!(reparsed, span.parsed, "{text:?}: {written:?}");
            }
        }
    }
}
