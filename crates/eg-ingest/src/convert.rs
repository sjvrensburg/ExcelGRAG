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
