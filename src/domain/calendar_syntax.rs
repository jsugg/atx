//! Strict calendar syntax parsing.

use jiff::civil::{Date, DateTime, Time};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CalendarSyntax {
    Time(Time),
    Date(Date),
    DateTime(DateTime),
}

pub(crate) fn parse_calendar(input: &str) -> Result<CalendarSyntax, CalendarSyntaxError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(CalendarSyntaxError::Empty);
    }

    if input.contains(':') && !input.contains('-') {
        return parse_time(input).map(CalendarSyntax::Time);
    }
    if input.contains('-') && !input.contains(':') {
        return parse_date(input).map(CalendarSyntax::Date);
    }
    parse_datetime(input).map(CalendarSyntax::DateTime)
}

fn parse_date(input: &str) -> Result<Date, CalendarSyntaxError> {
    let bytes = input.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(CalendarSyntaxError::InvalidDate);
    }
    let year = parse_i16(&input[0..4])?;
    let month = parse_i8(&input[5..7])?;
    let day = parse_i8(&input[8..10])?;
    Date::new(year, month, day).map_err(|_| CalendarSyntaxError::InvalidDate)
}

fn parse_time(input: &str) -> Result<Time, CalendarSyntaxError> {
    let bytes = input.as_bytes();
    match bytes {
        [_, _, b':', _, _] | [_, _, b':', _, _, b':', _, _] => {}
        _ => return Err(CalendarSyntaxError::InvalidTime),
    }

    let hour = parse_i8(&input[0..2])?;
    let minute = parse_i8(&input[3..5])?;
    let second = if bytes.len() == 8 {
        parse_i8(&input[6..8])?
    } else {
        0
    };
    Time::new(hour, minute, second, 0).map_err(|_| CalendarSyntaxError::InvalidTime)
}

fn parse_datetime(input: &str) -> Result<DateTime, CalendarSyntaxError> {
    let bytes = input.as_bytes();
    if !matches!(bytes.len(), 16 | 19) || !matches!(bytes.get(10), Some(b'T' | b' ')) {
        return Err(CalendarSyntaxError::InvalidDateTime);
    }
    let date = parse_date(&input[..10])?;
    let time = parse_time(&input[11..])?;
    Ok(date.to_datetime(time))
}

fn parse_i8(input: &str) -> Result<i8, CalendarSyntaxError> {
    if !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CalendarSyntaxError::InvalidNumber);
    }
    input
        .parse()
        .map_err(|_| CalendarSyntaxError::InvalidNumber)
}

fn parse_i16(input: &str) -> Result<i16, CalendarSyntaxError> {
    if !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CalendarSyntaxError::InvalidNumber);
    }
    input
        .parse()
        .map_err(|_| CalendarSyntaxError::InvalidNumber)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub(crate) enum CalendarSyntaxError {
    #[error("calendar input cannot be empty")]
    Empty,
    #[error("invalid decimal component")]
    InvalidNumber,
    #[error("expected YYYY-MM-DD")]
    InvalidDate,
    #[error("expected HH:MM or HH:MM:SS")]
    InvalidTime,
    #[error("expected YYYY-MM-DD HH:MM[:SS] or YYYY-MM-DDTHH:MM[:SS]")]
    InvalidDateTime,
}

#[cfg(test)]
mod tests {
    use super::{CalendarSyntax, parse_calendar, parse_time};

    #[test]
    fn parses_supported_shapes() {
        for input in [
            "15:30",
            "15:30:45",
            "2026-08-01",
            "2026-08-01 09:30",
            "2026-08-01T09:30:45",
        ] {
            assert!(parse_calendar(input).is_ok(), "{input}");
        }
        assert!(matches!(
            parse_calendar("15:30"),
            Ok(CalendarSyntax::Time(_))
        ));
    }

    #[test]
    fn rejects_offsets_and_out_of_range_components() {
        for input in [
            "24:00",
            "12:60",
            "12345",
            "12345678",
            "12:34567",
            "12345:67",
            "2026/08-01",
            "2026-08/01",
            "2026-02-30",
            "2026-08-01X09:30",
            "2026-08-01T09:30Z",
            "2026-08-01T09:30+01:00",
            "tomorrow",
        ] {
            assert!(parse_calendar(input).is_err(), "{input}");
        }
    }

    #[test]
    fn time_parser_checks_every_separator() {
        for input in ["12x34", "12x34:56", "12:34x56"] {
            assert!(parse_time(input).is_err(), "{input}");
        }
    }
}
