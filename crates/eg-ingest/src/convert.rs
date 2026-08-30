//! Translation from calamine's types into the ExcelGRAG model.

use calamine::{CellErrorType, Data, Dimensions, SheetVisible};
use eg_model::{CellFormat, CellValue, ErrorKind, RangeRef, SheetId};

/// Convert a calamine cell value, deriving what formatting we can infer from it.
///
/// calamine gives no access to styles, so the only presentation signal
/// recoverable here is date-ness, which it encodes in the value type itself.
pub fn convert_value(data: &Data) -> (CellValue, CellFormat) {
    let mut format = CellFormat::default();
    let value = match data {
        Data::Int(i) => CellValue::Number(*i as f64),
        Data::Float(f) => CellValue::Number(*f),
        Data::String(s) => CellValue::Text(s.clone()),
        Data::Bool(b) => CellValue::Bool(*b),
        Data::DateTime(dt) => {
            format.is_date = true;
            // Keep the serial number: it is what formulas actually operate on,
            // and the date-ness is preserved in the format flag instead.
            CellValue::Number(dt.as_f64())
        }
        Data::DateTimeIso(s) => {
            format.is_date = true;
            CellValue::Text(s.clone())
        }
        Data::DurationIso(s) => CellValue::Text(s.clone()),
        Data::Error(e) => CellValue::Error(convert_error(e)),
        Data::Empty => CellValue::Empty,
    };
    (value, format)
}

/// Map calamine's error enum onto ours.
///
/// calamine has no variants for the newer `#SPILL!` and `#CALC!` errors, so
/// those arrive as `#VALUE!` from the file and cannot be recovered here.
pub fn convert_error(e: &CellErrorType) -> ErrorKind {
    match e {
        CellErrorType::Div0 => ErrorKind::Div0,
        CellErrorType::NA => ErrorKind::NA,
        CellErrorType::Name => ErrorKind::Name,
        CellErrorType::Null => ErrorKind::Null,
        CellErrorType::Num => ErrorKind::Num,
        CellErrorType::Ref => ErrorKind::Ref,
        CellErrorType::Value => ErrorKind::Value,
        CellErrorType::GettingData => ErrorKind::GettingData,
    }
}

pub fn visibility(v: SheetVisible) -> eg_model::Visibility {
    match v {
        SheetVisible::Visible => eg_model::Visibility::Visible,
        SheetVisible::Hidden => eg_model::Visibility::Hidden,
        SheetVisible::VeryHidden => eg_model::Visibility::VeryHidden,
    }
}

/// Convert a calamine rectangle, returning `None` if it exceeds Excel's limits.
pub fn dimensions_to_range(d: &Dimensions, sheet: SheetId) -> Option<RangeRef> {
    let left = u16::try_from(d.start.1).ok()?;
    let right = u16::try_from(d.end.1).ok()?;
    if d.start.0 > eg_model::MAX_ROW
        || d.end.0 > eg_model::MAX_ROW
        || u32::from(right) > eg_model::MAX_COL
    {
        return None;
    }
    Some(RangeRef::new(sheet, d.start.0, left, d.end.0, right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints_and_floats_both_become_numbers() {
        assert_eq!(convert_value(&Data::Int(5)).0, CellValue::Number(5.0));
        assert_eq!(convert_value(&Data::Float(1.5)).0, CellValue::Number(1.5));
    }

    #[test]
    fn dates_keep_their_serial_and_gain_a_flag() {
        let dt = calamine::ExcelDateTime::new(45000.0, calamine::ExcelDateTimeType::DateTime, false);
        let (value, format) = convert_value(&Data::DateTime(dt));
        assert_eq!(value, CellValue::Number(45000.0));
        assert!(format.is_date, "date-ness must survive as a format flag");
    }

    #[test]
    fn errors_map_across_faithfully() {
        assert_eq!(convert_error(&CellErrorType::Div0), ErrorKind::Div0);
        assert_eq!(convert_error(&CellErrorType::Ref), ErrorKind::Ref);
        assert_eq!(
            convert_value(&Data::Error(CellErrorType::NA)).0,
            CellValue::Error(ErrorKind::NA)
        );
    }

    #[test]
    fn empty_maps_to_empty() {
        assert_eq!(convert_value(&Data::Empty).0, CellValue::Empty);
    }

    #[test]
    fn dimensions_convert_and_reject_overflow() {
        let d = Dimensions {
            start: (1, 1),
            end: (1, 3),
        };
        assert_eq!(
            dimensions_to_range(&d, SheetId(0)).unwrap().to_a1(),
            "B2:D2"
        );

        let bad = Dimensions {
            start: (0, 0),
            end: (0, 100_000),
        };
        assert!(dimensions_to_range(&bad, SheetId(0)).is_none());
    }
}

/// Repair `>` / `>=` in formulas decoded from the binary formats.
///
/// calamine's BIFF token tables map the two greater-than operators the wrong way
/// round. The spec assigns `PtgGe = 0x0C` and `PtgGt = 0x0D`, but calamine
/// renders `0x0C` as `>` and `0x0D` as `>=`, so every `>` it emits from an
/// `.xlsb` or `.xls` file actually means `>=`, and vice versa. The two are
/// exactly transposed, so swapping them back is a complete fix rather than a
/// guess. Confirmed against a fixture whose XLSX twin stores the authoritative
/// text `A1>A2` while the XLSB read yields `A1>=A2`.
///
/// The swap is token-aware: `<>` and `<=` must survive untouched, and text
/// inside string literals or quoted sheet names is never rewritten.
///
/// XLSX is unaffected — it stores formulas as text — so this is applied only to
/// the binary formats.
pub fn fix_binary_comparison_operators(formula: &str) -> String {
    if !formula.contains('>') {
        return formula.to_string();
    }

    let bytes = formula.as_bytes();
    // Built as bytes rather than chars: every byte is either copied verbatim
    // from valid UTF-8 input or is ASCII we inserted, so the result is always
    // valid UTF-8, and multi-byte sequences are never reinterpreted.
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 4);
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            // Copy quoted spans verbatim. A doubled quote is an escaped quote
            // and keeps the span open.
            q @ (b'"' | b'\'') => {
                out.push(q);
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == q {
                        if i + 1 < bytes.len() && bytes[i + 1] == q {
                            out.extend_from_slice(&[q, q]);
                            i += 2;
                            continue;
                        }
                        out.push(q);
                        i += 1;
                        break;
                    }
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            // `<` starts `<`, `<=` or `<>`; none need swapping, but they must be
            // consumed here so a following `>` is not misread as its own token.
            b'<' => {
                out.push(b'<');
                i += 1;
                if i < bytes.len() && (bytes[i] == b'=' || bytes[i] == b'>') {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    // calamine's `>=` really means `>`.
                    out.push(b'>');
                    i += 2;
                } else {
                    // calamine's `>` really means `>=`.
                    out.extend_from_slice(b">=");
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }

    String::from_utf8(out).expect("only verbatim UTF-8 bytes and ASCII are emitted")
}

#[cfg(test)]
mod operator_tests {
    use super::fix_binary_comparison_operators as fix;

    #[test]
    fn greater_than_operators_are_transposed() {
        assert_eq!(fix("A1>A2"), "A1>=A2");
        assert_eq!(fix("A1>=A2"), "A1>A2");
    }

    #[test]
    fn swapping_twice_is_the_identity() {
        for f in ["A1>A2", "A1>=A2", "IF(A1>=B1,C1>D1,0)"] {
            assert_eq!(fix(&fix(f)), f, "{f}");
        }
    }

    #[test]
    fn less_than_and_not_equal_are_untouched() {
        for f in ["A1<A2", "A1<=A2", "A1<>A2", "A1=A2"] {
            assert_eq!(fix(f), f, "{f}");
        }
    }

    #[test]
    fn mixed_comparisons_in_one_formula() {
        assert_eq!(
            fix("IF(AND(A1>=1,B1<>2,C1>3),\"y\",\"n\")"),
            "IF(AND(A1>1,B1<>2,C1>=3),\"y\",\"n\")"
        );
    }

    #[test]
    fn string_literals_are_never_rewritten() {
        assert_eq!(fix("IF(A1>1,\">=\",\">\")"), "IF(A1>=1,\">=\",\">\")");
        assert_eq!(fix("CONCATENATE(\"a>b\")"), "CONCATENATE(\"a>b\")");
        // A doubled quote escapes, and must not end the literal early.
        assert_eq!(fix("IF(A1>1,\"a\"\">\"\"b\")"), "IF(A1>=1,\"a\"\">\"\"b\")");
    }

    #[test]
    fn quoted_sheet_names_are_never_rewritten() {
        assert_eq!(fix("'a>b'!A1+1"), "'a>b'!A1+1");
        assert_eq!(fix("'a>b'!A1>2"), "'a>b'!A1>=2");
    }

    #[test]
    fn formulas_without_comparisons_pass_through_unchanged() {
        assert_eq!(fix("SUM(A1:A9)*2"), "SUM(A1:A9)*2");
        assert_eq!(fix(""), "");
    }

    #[test]
    fn non_ascii_text_survives() {
        assert_eq!(fix("IF(A1>1,\"café\",\"—\")"), "IF(A1>=1,\"café\",\"—\")");
    }

    #[test]
    fn unterminated_literal_does_not_panic_or_lose_text() {
        assert_eq!(fix("CONCAT(\"unclosed"), "CONCAT(\"unclosed");
    }
}
