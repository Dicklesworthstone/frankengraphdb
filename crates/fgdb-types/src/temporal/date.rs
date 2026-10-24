//! Canonical calendar dates (fgdb-txbg9): a day number over the proleptic
//! Gregorian calendar, with the openCypher and GQL constructors, accessors
//! and ISO 8601 text forms.
//!
//! A date has no time of day and no zone. Equality, hashing and ordering are
//! those of the day number, so they are chronological. Nothing here consults
//! the host clock, timezone or locale.

use super::calendar;
use std::fmt;

/// A day in `-999999999-01-01 ..= +999999999-12-31`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalDate {
    epoch_day: i64,
}

/// Why a date could not be built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DateError {
    /// The day lies outside `-999999999-01-01 ..= +999999999-12-31`.
    OutOfRange,
    /// The fields name no calendar day: month 13, February 30, week 54, ...
    NoSuchDay,
    /// The text is not an ISO 8601 date form this profile reads.
    Malformed,
}

impl fmt::Display for DateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OutOfRange => "date outside -999999999-01-01..=+999999999-12-31",
            Self::NoSuchDay => "the fields name no calendar day",
            Self::Malformed => "not an ISO 8601 date",
        })
    }
}

impl std::error::Error for DateError {}

impl CanonicalDate {
    /// The date with day number `epoch_day` (days since 1970-01-01).
    ///
    /// # Errors
    ///
    /// [`DateError::OutOfRange`] outside the supported range.
    pub fn from_epoch_day(epoch_day: i64) -> Result<Self, DateError> {
        if calendar::in_range(epoch_day) {
            Ok(Self { epoch_day })
        } else {
            Err(DateError::OutOfRange)
        }
    }

    /// The calendar date `year-month-day`.
    ///
    /// # Errors
    ///
    /// [`DateError::OutOfRange`] for an unsupported year,
    /// [`DateError::NoSuchDay`] for a month or day that does not exist.
    pub fn from_civil(year: i64, month: u8, day: u8) -> Result<Self, DateError> {
        Self::within(year, calendar::from_civil(year, month, day))
    }

    /// The ISO week date `week_year-Wweek-weekday` (weekday 1 is Monday).
    ///
    /// # Errors
    ///
    /// [`DateError::NoSuchDay`] for a week the year does not have or a
    /// weekday outside 1-7, [`DateError::OutOfRange`] for a day outside the
    /// supported range.
    pub fn from_iso_week(week_year: i64, week: u8, weekday: u8) -> Result<Self, DateError> {
        let weeks = calendar::iso_week_count(week_year).ok_or(DateError::OutOfRange)?;
        if !(1..=weeks).contains(&week) || !(1..=7).contains(&weekday) {
            return Err(DateError::NoSuchDay);
        }
        calendar::from_iso_week(week_year, week, weekday)
            .map(|epoch_day| Self { epoch_day })
            .ok_or(DateError::OutOfRange)
    }

    /// The ordinal date `year-ordinal` (1 is January 1).
    ///
    /// # Errors
    ///
    /// As [`Self::from_civil`].
    pub fn from_ordinal(year: i64, ordinal: u16) -> Result<Self, DateError> {
        Self::within(year, calendar::from_ordinal(year, ordinal))
    }

    /// Day `day_of_quarter` of quarter `quarter` (1-4) of `year`.
    ///
    /// # Errors
    ///
    /// As [`Self::from_civil`].
    pub fn from_quarter(year: i64, quarter: u8, day_of_quarter: u8) -> Result<Self, DateError> {
        Self::within(year, calendar::from_quarter(year, quarter, day_of_quarter))
    }

    fn within(year: i64, epoch_day: Option<i64>) -> Result<Self, DateError> {
        match epoch_day {
            Some(epoch_day) => Ok(Self { epoch_day }),
            None if calendar::year_in_range(year) => Err(DateError::NoSuchDay),
            None => Err(DateError::OutOfRange),
        }
    }

    /// Days since 1970-01-01.
    #[must_use]
    pub const fn epoch_day(self) -> i64 {
        self.epoch_day
    }

    /// `(year, month, day)`.
    #[must_use]
    pub fn civil(self) -> (i64, u8, u8) {
        calendar::to_civil(self.epoch_day).expect("a canonical date is in range")
    }

    /// The year, astronomical (year 0 is 1 BCE).
    #[must_use]
    pub fn year(self) -> i64 {
        self.civil().0
    }

    /// The month, 1-12.
    #[must_use]
    pub fn month(self) -> u8 {
        self.civil().1
    }

    /// The day of the month, 1-31.
    #[must_use]
    pub fn day(self) -> u8 {
        self.civil().2
    }

    /// `(week_year, week, weekday)` of the ISO week date.
    #[must_use]
    pub fn iso_week(self) -> (i64, u8, u8) {
        calendar::to_iso_week(self.epoch_day).expect("a canonical date is in range")
    }

    /// The ISO weekday, 1 (Monday) through 7 (Sunday).
    #[must_use]
    pub const fn weekday(self) -> u8 {
        calendar::iso_weekday(self.epoch_day)
    }

    /// The day of the year, 1-366.
    #[must_use]
    pub fn ordinal_day(self) -> u16 {
        calendar::to_ordinal(self.epoch_day)
            .expect("a canonical date is in range")
            .1
    }

    /// `(quarter, day_of_quarter)`, quarter 1-4 and day 1-92.
    #[must_use]
    pub fn quarter(self) -> (u8, u8) {
        let (_, quarter, day) =
            calendar::to_quarter(self.epoch_day).expect("a canonical date is in range");
        (quarter, day)
    }

    /// Read an ISO 8601 date: calendar (`2015-07-21`, `20150721`), year and
    /// month (`2015-07`, `201507`), week (`2015-W30-2`, `2015W302`,
    /// `2015-W30`, `2015W30`), ordinal (`2015-202`, `2015202`) or a bare
    /// year (`2015`). A missing day, weekday or month is the first one. The
    /// year is four digits, or a sign and four to nine digits; a signed year
    /// takes only the extended (hyphenated) forms.
    ///
    /// # Errors
    ///
    /// [`DateError::Malformed`] for any other text, then as the matching
    /// constructor.
    pub fn parse_iso(text: &str) -> Result<Self, DateError> {
        use DateError::Malformed;
        let bytes = text.as_bytes();
        let (sign, body) = match bytes.first() {
            Some(b'+') => (Some(1), &bytes[1..]),
            Some(b'-') => (Some(-1), &bytes[1..]),
            _ => (None, bytes),
        };
        let year_digits = match sign {
            None => 4,
            Some(_) => body.iter().take_while(|byte| byte.is_ascii_digit()).count(),
        };
        if !(4..=9).contains(&year_digits) || body.len() < year_digits {
            return Err(Malformed);
        }
        let year = number(&body[..year_digits]).ok_or(Malformed)? * sign.unwrap_or(1);
        let rest = &body[year_digits..];
        let extended = rest.first() == Some(&b'-');
        if sign.is_some() && !rest.is_empty() && !extended {
            return Err(Malformed);
        }
        let rest = if extended { &rest[1..] } else { rest };
        if let [b'W', week @ ..] = rest {
            let (week, weekday) = match (extended, week) {
                (_, [w1, w2]) => ([*w1, *w2], 1),
                (true, [w1, w2, b'-', day]) | (false, [w1, w2, day]) => {
                    ([*w1, *w2], number(&[*day]).ok_or(Malformed)?)
                }
                _ => return Err(Malformed),
            };
            let week = number(&week).ok_or(Malformed)?;
            return Self::from_iso_week(year, week as u8, weekday as u8);
        }
        let field = |bytes: &[u8]| number(bytes).ok_or(Malformed);
        match (extended, rest.len()) {
            (false, 0) => Self::from_civil(year, 1, 1),
            (_, 2) => Self::from_civil(year, field(rest)? as u8, 1),
            (_, 3) => Self::from_ordinal(year, field(rest)? as u16),
            (false, 4) => {
                Self::from_civil(year, field(&rest[..2])? as u8, field(&rest[2..])? as u8)
            }
            (true, 5) if rest[2] == b'-' => {
                Self::from_civil(year, field(&rest[..2])? as u8, field(&rest[3..])? as u8)
            }
            _ => Err(Malformed),
        }
    }
}

/// An unsigned decimal of one to nine ASCII digits.
fn number(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() || bytes.len() > 9 {
        return None;
    }
    bytes.iter().try_fold(0_i64, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + i64::from(byte - b'0'))
    })
}

/// ISO 8601 extended calendar form, with the java.time year spelling: four
/// digits for years 0-9999, a `+` and the digits above 9999, and a `-` and at
/// least four digits below 0 (`-0001-07-21`, `+10000-01-01`).
impl fmt::Display for CanonicalDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.civil();
        if (0..=9999).contains(&year) {
            write!(f, "{year:04}")?;
        } else if year > 9999 {
            write!(f, "+{year}")?;
        } else {
            write!(f, "-{:04}", year.unsigned_abs())?;
        }
        write!(f, "-{month:02}-{day:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> Result<CanonicalDate, DateError> {
        CanonicalDate::parse_iso(text)
    }

    #[test]
    fn every_tck_spelling_of_a_date_reads_as_that_day() {
        // TCK Temporal2 [1]: each text and the day it denotes.
        for (text, expected) in [
            ("2015-07-21", "2015-07-21"),
            ("20150721", "2015-07-21"),
            ("2015-07", "2015-07-01"),
            ("201507", "2015-07-01"),
            ("2015-W30-2", "2015-07-21"),
            ("2015W302", "2015-07-21"),
            ("2015-W30", "2015-07-20"),
            ("2015W30", "2015-07-20"),
            ("2015-202", "2015-07-21"),
            ("2015202", "2015-07-21"),
            ("2015", "2015-01-01"),
            ("-999999999-01-01", "-999999999-01-01"),
            ("+999999999-12-31", "+999999999-12-31"),
            ("+2015-07-21", "2015-07-21"),
            ("-0001-12-31", "-0001-12-31"),
            ("0000-01-01", "0000-01-01"),
            // A signed year alone is a year, however many of its nine digits.
            ("+20150721", "+20150721-01-01"),
        ] {
            assert_eq!(
                date(text).map(|day| day.to_string()),
                Ok(expected.into()),
                "{text}"
            );
        }
        assert_eq!(date("2015-07-21").unwrap().epoch_day(), 16_637);
    }

    #[test]
    fn text_that_names_no_day_or_is_not_a_date_refuses_with_its_reason() {
        use DateError::{Malformed, NoSuchDay, OutOfRange};
        for (text, error) in [
            ("2015-02-29", NoSuchDay),
            ("2015-13", NoSuchDay),
            ("2015-00-10", NoSuchDay),
            ("2015-366", NoSuchDay),
            ("2015-W54", NoSuchDay),
            ("2014-W53-1", NoSuchDay),
            ("2015-W30-8", NoSuchDay),
            ("+1000000000-01-01", Malformed),
            ("-1000000000-01-01", Malformed),
            ("", Malformed),
            ("15-07-21", Malformed),
            ("2015-7-21", Malformed),
            ("2015-07-2", Malformed),
            ("2015-0721", Malformed),
            ("2015-", Malformed),
            ("2015W3", Malformed),
            ("2015-W30-", Malformed),
            ("+2015W30", Malformed),
            ("+2015-0721", Malformed),
            ("2015-07-21T00:00", Malformed),
            ("2015-Q3-21", Malformed),
            (" 2015-07-21", Malformed),
            ("２０１５", Malformed),
        ] {
            assert_eq!(date(text), Err(error), "{text}");
        }
        assert_eq!(
            CanonicalDate::from_epoch_day(calendar::MAX_EPOCH_DAY + 1),
            Err(OutOfRange)
        );
        assert_eq!(
            CanonicalDate::from_civil(1_000_000_000, 1, 1),
            Err(OutOfRange)
        );
        assert_eq!(
            CanonicalDate::from_iso_week(999_999_999, 52, 6),
            Err(OutOfRange)
        );
    }

    #[test]
    fn display_and_parse_round_trip_and_order_is_chronological() {
        let mut previous = None;
        for epoch_day in (calendar::MIN_EPOCH_DAY..=calendar::MAX_EPOCH_DAY)
            .step_by(7_919_381)
            .chain([
                calendar::MAX_EPOCH_DAY,
                -719_529,
                -719_528,
                -1,
                0,
                2_932_897,
            ])
        {
            let day = CanonicalDate::from_epoch_day(epoch_day).unwrap();
            assert_eq!(date(&day.to_string()), Ok(day), "{day}");
            let (year, month, of_month) = day.civil();
            assert_eq!(CanonicalDate::from_civil(year, month, of_month), Ok(day));
            let (week_year, week, weekday) = day.iso_week();
            assert_eq!(weekday, day.weekday());
            assert_eq!(
                CanonicalDate::from_iso_week(week_year, week, weekday),
                Ok(day)
            );
            assert_eq!(
                CanonicalDate::from_ordinal(year, day.ordinal_day()),
                Ok(day)
            );
            let (quarter, of_quarter) = day.quarter();
            assert_eq!(
                CanonicalDate::from_quarter(year, quarter, of_quarter),
                Ok(day)
            );
            if let Some(previous) = previous
                && epoch_day > CanonicalDate::epoch_day(previous)
            {
                assert!(previous < day);
            }
            previous = Some(day);
        }
        assert_eq!(
            CanonicalDate::from_civil(10_000, 1, 1).unwrap().to_string(),
            "+10000-01-01"
        );
        assert_eq!(
            CanonicalDate::from_civil(-1, 7, 21).unwrap().to_string(),
            "-0001-07-21"
        );
    }
}
