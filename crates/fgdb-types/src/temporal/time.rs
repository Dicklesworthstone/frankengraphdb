//! Times of day (fgdb-txbg9): a local time, a time at a UTC offset and a
//! local date-time, with their constructors, accessors and ISO 8601 text
//! forms.
//!
//! Nothing here consults the host clock, zone or locale. A local time and a
//! local date-time order chronologically. A time at an offset orders by the
//! UTC time of day it denotes, then by its local time, so two such times are
//! equal exactly when their local times and offsets are. The profile's
//! default offset is UTC: a time read without one is at `Z`.

use super::{CanonicalDate, DateError, MAX_UTC_OFFSET_SECONDS};
use std::cmp::Ordering;
use std::fmt;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Nanoseconds in a day; a local time is a count below it.
pub const NANOS_PER_DAY: u64 = 86_400 * NANOS_PER_SECOND;

/// A time of day, `00:00 ..= 23:59:59.999999999`, without an offset.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalLocalTime {
    nanos_of_day: u64,
}

/// A time of day at a UTC offset within ±18:00.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CanonicalTime {
    local: CanonicalLocalTime,
    offset_seconds: i32,
}

/// A date and a time of day, without an offset.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalLocalDateTime {
    date: CanonicalDate,
    time: CanonicalLocalTime,
}

/// Why a time, a time at an offset or a local date-time could not be built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimeError {
    /// An hour, minute, second or fraction outside its range.
    NoSuchTime,
    /// A UTC offset beyond ±18:00, or with minutes or seconds above 59.
    OffsetOutOfRange,
    /// The text is not an ISO 8601 form this profile reads.
    Malformed,
    /// The date part names no supported day.
    Date(DateError),
}

impl fmt::Display for TimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchTime => f.write_str("the fields name no time of day"),
            Self::OffsetOutOfRange => f.write_str("UTC offset outside -18:00..=+18:00"),
            Self::Malformed => f.write_str("not an ISO 8601 time"),
            Self::Date(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for TimeError {}

impl From<DateError> for TimeError {
    fn from(error: DateError) -> Self {
        Self::Date(error)
    }
}

impl CanonicalLocalTime {
    /// Midnight, `00:00`.
    pub const MIDNIGHT: Self = Self { nanos_of_day: 0 };

    /// The time `nanos_of_day` nanoseconds after midnight.
    ///
    /// # Errors
    ///
    /// [`TimeError::NoSuchTime`] at or beyond [`NANOS_PER_DAY`].
    pub const fn from_nanos_of_day(nanos_of_day: u64) -> Result<Self, TimeError> {
        if nanos_of_day < NANOS_PER_DAY {
            Ok(Self { nanos_of_day })
        } else {
            Err(TimeError::NoSuchTime)
        }
    }

    /// `hour:minute:second.nanosecond`; there is no leap second.
    ///
    /// # Errors
    ///
    /// [`TimeError::NoSuchTime`] for hour 24 or more, minute or second 60 or
    /// more, or a nanosecond of one second or more.
    pub fn from_hms_nano(
        hour: u8,
        minute: u8,
        second: u8,
        nanosecond: u32,
    ) -> Result<Self, TimeError> {
        if hour > 23 || minute > 59 || second > 59 || u64::from(nanosecond) >= NANOS_PER_SECOND {
            return Err(TimeError::NoSuchTime);
        }
        let seconds = (u64::from(hour) * 60 + u64::from(minute)) * 60 + u64::from(second);
        Ok(Self {
            nanos_of_day: seconds * NANOS_PER_SECOND + u64::from(nanosecond),
        })
    }

    /// Nanoseconds since midnight.
    #[must_use]
    pub const fn nanos_of_day(self) -> u64 {
        self.nanos_of_day
    }

    /// The hour, 0-23.
    #[must_use]
    pub const fn hour(self) -> u8 {
        (self.nanos_of_day / (3_600 * NANOS_PER_SECOND)) as u8
    }

    /// The minute of the hour, 0-59.
    #[must_use]
    pub const fn minute(self) -> u8 {
        (self.nanos_of_day / (60 * NANOS_PER_SECOND) % 60) as u8
    }

    /// The second of the minute, 0-59.
    #[must_use]
    pub const fn second(self) -> u8 {
        (self.nanos_of_day / NANOS_PER_SECOND % 60) as u8
    }

    /// The fraction of the second in nanoseconds, 0-999999999.
    #[must_use]
    pub const fn nanosecond(self) -> u32 {
        (self.nanos_of_day % NANOS_PER_SECOND) as u32
    }

    /// Read an ISO 8601 local time, extended (`21:40:32.142`, `21:40:32`,
    /// `21:40`) or basic (`214032.142`, `214032`, `2140`), or a bare hour
    /// (`21`). A fraction has one to nine digits after a `.`, and only
    /// follows seconds. A missing minute or second is zero.
    ///
    /// # Errors
    ///
    /// [`TimeError::Malformed`] for any other text, [`TimeError::NoSuchTime`]
    /// for a field outside its range.
    pub fn parse_iso(text: &str) -> Result<Self, TimeError> {
        parse_clock(text.as_bytes())
    }
}

impl CanonicalTime {
    /// `local` at `offset_seconds` east of UTC.
    ///
    /// # Errors
    ///
    /// [`TimeError::OffsetOutOfRange`] beyond ±18:00.
    pub const fn new(local: CanonicalLocalTime, offset_seconds: i32) -> Result<Self, TimeError> {
        if offset_seconds.unsigned_abs() > MAX_UTC_OFFSET_SECONDS.unsigned_abs() {
            return Err(TimeError::OffsetOutOfRange);
        }
        Ok(Self {
            local,
            offset_seconds,
        })
    }

    /// The local time of day.
    #[must_use]
    pub const fn local(self) -> CanonicalLocalTime {
        self.local
    }

    /// Seconds east of UTC.
    #[must_use]
    pub const fn offset_seconds(self) -> i32 {
        self.offset_seconds
    }

    /// The UTC time of day in nanoseconds since midnight, unwrapped: below
    /// zero or at and beyond one day when the offset carries it there.
    #[must_use]
    pub const fn utc_nanos_of_day(self) -> i64 {
        self.local.nanos_of_day as i64 - self.offset_seconds as i64 * NANOS_PER_SECOND as i64
    }

    /// The time whose unwrapped UTC time of day is `utc_nanos_of_day` and
    /// whose local time is `local`; the inverse of [`Self::utc_nanos_of_day`].
    ///
    /// # Errors
    ///
    /// [`TimeError::OffsetOutOfRange`] when the two differ by anything but
    /// whole seconds within ±18:00.
    pub fn from_utc_nanos_of_day(
        utc_nanos_of_day: i64,
        local: CanonicalLocalTime,
    ) -> Result<Self, TimeError> {
        let shift = i128::from(local.nanos_of_day) - i128::from(utc_nanos_of_day);
        let per_second = i128::from(NANOS_PER_SECOND);
        if shift % per_second != 0 {
            return Err(TimeError::OffsetOutOfRange);
        }
        let offset = i32::try_from(shift / per_second).map_err(|_| TimeError::OffsetOutOfRange)?;
        Self::new(local, offset)
    }

    /// Read an ISO 8601 time as [`CanonicalLocalTime::parse_iso`] does,
    /// followed by an offset: `Z`, or a sign and `hh`, `hh:mm`, `hhmm`,
    /// `hh:mm:ss` or `hhmmss`. Without an offset the time is at UTC.
    ///
    /// # Errors
    ///
    /// As [`CanonicalLocalTime::parse_iso`], and
    /// [`TimeError::OffsetOutOfRange`] for an offset beyond ±18:00.
    pub fn parse_iso(text: &str) -> Result<Self, TimeError> {
        let bytes = text.as_bytes();
        let split = bytes
            .iter()
            .position(|byte| matches!(byte, b'+' | b'-' | b'Z'))
            .unwrap_or(bytes.len());
        let local = parse_clock(&bytes[..split])?;
        let offset = if split == bytes.len() {
            0
        } else {
            parse_offset(&bytes[split..])?
        };
        Self::new(local, offset)
    }
}

impl PartialOrd for CanonicalTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The UTC time of day first, then the local time. The pair determines the
/// offset, so this order agrees with equality.
impl Ord for CanonicalTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.utc_nanos_of_day()
            .cmp(&other.utc_nanos_of_day())
            .then(self.local.cmp(&other.local))
    }
}

impl CanonicalLocalDateTime {
    /// `time` on `date`.
    #[must_use]
    pub const fn new(date: CanonicalDate, time: CanonicalLocalTime) -> Self {
        Self { date, time }
    }

    /// The date.
    #[must_use]
    pub const fn date(self) -> CanonicalDate {
        self.date
    }

    /// The time of day.
    #[must_use]
    pub const fn time(self) -> CanonicalLocalTime {
        self.time
    }

    /// Read an ISO 8601 local date-time: any date form
    /// [`CanonicalDate::parse_iso`] reads, then optionally `T` and any time
    /// form [`CanonicalLocalTime::parse_iso`] reads (`2015-W30-2T214032.142`,
    /// `2015202T21`). A date alone is that day's midnight, as openCypher
    /// reads `localdatetime('-999999999-01-01')` (TCK Temporal10).
    ///
    /// # Errors
    ///
    /// As the two parsers, a date error wrapped in [`TimeError::Date`].
    pub fn parse_iso(text: &str) -> Result<Self, TimeError> {
        let (date, time) = match text.split_once('T') {
            Some((date, time)) => (date, parse_clock(time.as_bytes())?),
            None => (text, CanonicalLocalTime::MIDNIGHT),
        };
        Ok(Self {
            date: CanonicalDate::parse_iso(date)?,
            time,
        })
    }
}

/// Two ASCII digits at `at`.
fn two_digits(bytes: &[u8], at: usize) -> Option<u8> {
    match bytes.get(at..at + 2)? {
        [tens @ b'0'..=b'9', ones @ b'0'..=b'9'] => Some((tens - b'0') * 10 + (ones - b'0')),
        _ => None,
    }
}

/// `hh[[:]mm[[:]ss[.f{1,9}]]]`, the separator used throughout or nowhere.
fn parse_clock(bytes: &[u8]) -> Result<CanonicalLocalTime, TimeError> {
    use TimeError::Malformed;
    let hour = two_digits(bytes, 0).ok_or(Malformed)?;
    let extended = bytes.get(2) == Some(&b':');
    let mut fields = [0_u8; 2];
    let mut at = 2;
    for field in &mut fields {
        if at == bytes.len() {
            break;
        }
        if extended {
            if bytes[at] != b':' {
                return Err(Malformed);
            }
            at += 1;
        }
        *field = two_digits(bytes, at).ok_or(Malformed)?;
        at += 2;
    }
    let mut nanosecond = 0;
    if at < bytes.len() {
        // Only seconds take a fraction, so both fields must have been read.
        let fraction = match &bytes[at..] {
            [b'.', digits @ ..] if at >= if extended { 8 } else { 6 } => digits,
            _ => return Err(Malformed),
        };
        if fraction.is_empty() || fraction.len() > 9 || !fraction.iter().all(u8::is_ascii_digit) {
            return Err(Malformed);
        }
        let value = fraction
            .iter()
            .fold(0_u32, |value, digit| value * 10 + u32::from(digit - b'0'));
        nanosecond = value * 10_u32.pow(9 - fraction.len() as u32);
    }
    CanonicalLocalTime::from_hms_nano(hour, fields[0], fields[1], nanosecond)
}

/// Read a UTC offset, `Z` or a sign and `hh`, `hh:mm`, `hhmm`, `hh:mm:ss`
/// or `hhmmss`, as seconds east of UTC.
///
/// # Errors
///
/// [`TimeError::Malformed`] for any other text, a zone name included, and
/// [`TimeError::OffsetOutOfRange`] beyond ±18:00.
pub fn parse_utc_offset(text: &str) -> Result<i32, TimeError> {
    parse_offset(text.as_bytes())
}

/// An offset's canonical spelling: `Z` for UTC, else `±hh:mm`, with `:ss`
/// when it has seconds.
#[must_use]
pub fn format_utc_offset(offset_seconds: i32) -> String {
    struct Offset(i32);
    impl fmt::Display for Offset {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write_offset(f, self.0)
        }
    }
    Offset(offset_seconds).to_string()
}

fn write_offset(f: &mut fmt::Formatter<'_>, offset_seconds: i32) -> fmt::Result {
    if offset_seconds == 0 {
        return f.write_str("Z");
    }
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let magnitude = offset_seconds.unsigned_abs();
    let (hours, minutes, seconds) = (magnitude / 3_600, magnitude / 60 % 60, magnitude % 60);
    write!(f, "{sign}{hours:02}:{minutes:02}")?;
    if seconds != 0 {
        write!(f, ":{seconds:02}")?;
    }
    Ok(())
}

/// `Z`, or `±hh[[:]mm[[:]ss]]` as seconds east of UTC.
fn parse_offset(bytes: &[u8]) -> Result<i32, TimeError> {
    use TimeError::{Malformed, OffsetOutOfRange};
    let (sign, body) = match bytes {
        [b'Z'] => return Ok(0),
        [b'+', body @ ..] => (1, body),
        [b'-', body @ ..] => (-1, body),
        _ => return Err(Malformed),
    };
    let hours = two_digits(body, 0).ok_or(Malformed)?;
    let extended = body.get(2) == Some(&b':');
    let mut fields = [0_u8; 2];
    let mut at = 2;
    for field in &mut fields {
        if at == body.len() {
            break;
        }
        if extended {
            if body[at] != b':' {
                return Err(Malformed);
            }
            at += 1;
        }
        *field = two_digits(body, at).ok_or(Malformed)?;
        at += 2;
    }
    if at != body.len() {
        return Err(Malformed);
    }
    let [minutes, seconds] = fields;
    if minutes > 59 || seconds > 59 {
        return Err(OffsetOutOfRange);
    }
    let magnitude = (i32::from(hours) * 60 + i32::from(minutes)) * 60 + i32::from(seconds);
    if magnitude > MAX_UTC_OFFSET_SECONDS {
        return Err(OffsetOutOfRange);
    }
    Ok(sign * magnitude)
}

/// java.time's local time spelling: `hh:mm`, then `:ss` unless the second
/// and fraction are both zero, then the fraction in three, six or nine
/// digits, the fewest that hold it.
impl fmt::Display for CanonicalLocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour(), self.minute())?;
        let (second, nanosecond) = (self.second(), self.nanosecond());
        if second == 0 && nanosecond == 0 {
            return Ok(());
        }
        write!(f, ":{second:02}")?;
        if nanosecond == 0 {
            Ok(())
        } else if nanosecond % 1_000_000 == 0 {
            write!(f, ".{:03}", nanosecond / 1_000_000)
        } else if nanosecond % 1_000 == 0 {
            write!(f, ".{:06}", nanosecond / 1_000)
        } else {
            write!(f, ".{nanosecond:09}")
        }
    }
}

/// The local time, then `Z` for UTC or `±hh:mm`, with `:ss` when the offset
/// has seconds.
impl fmt::Display for CanonicalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.local.fmt(f)?;
        write_offset(f, self.offset_seconds)
    }
}

/// The date, `T`, and the local time.
impl fmt::Display for CanonicalLocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}T{}", self.date, self.time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(text: &str) -> Result<String, TimeError> {
        CanonicalLocalTime::parse_iso(text).map(|time| time.to_string())
    }

    fn offset_time(text: &str) -> Result<String, TimeError> {
        CanonicalTime::parse_iso(text).map(|time| time.to_string())
    }

    #[test]
    fn every_tck_spelling_of_a_time_reads_as_that_time() {
        // TCK Temporal2 [2], [3] and [4]: each text and its java.time spelling.
        for (text, expected) in [
            ("21:40:32.142", "21:40:32.142"),
            ("214032.142", "21:40:32.142"),
            ("21:40:32", "21:40:32"),
            ("214032", "21:40:32"),
            ("21:40", "21:40"),
            ("2140", "21:40"),
            ("21", "21:00"),
            ("00:00:00.000000001", "00:00:00.000000001"),
            ("23:59:59.9", "23:59:59.900"),
            ("12:31:14.645876", "12:31:14.645876"),
            ("12:31:00.000", "12:31"),
        ] {
            assert_eq!(local(text), Ok(expected.into()), "{text}");
        }
        for (text, expected) in [
            ("21:40:32.142+0100", "21:40:32.142+01:00"),
            ("214032.142Z", "21:40:32.142Z"),
            ("21:40:32+01:00", "21:40:32+01:00"),
            ("214032-0100", "21:40:32-01:00"),
            ("21:40-01:30", "21:40-01:30"),
            ("2140-00:00", "21:40Z"),
            ("2140-02", "21:40-02:00"),
            ("22+18:00", "22:00+18:00"),
            ("12:34:56+02:05:00", "12:34:56+02:05"),
            ("12:34:56+02:05:59", "12:34:56+02:05:59"),
            ("12:34:56-020507", "12:34:56-02:05:07"),
            ("12:00", "12:00Z"),
        ] {
            assert_eq!(offset_time(text), Ok(expected.into()), "{text}");
        }
        for (text, expected) in [
            ("2015-07-21T21:40:32.142", "2015-07-21T21:40:32.142"),
            ("2015-W30-2T214032.142", "2015-07-21T21:40:32.142"),
            ("2015-202T21:40:32", "2015-07-21T21:40:32"),
            ("2015T214032", "2015-01-01T21:40:32"),
            ("20150721T21:40", "2015-07-21T21:40"),
            ("2015-W30T2140", "2015-07-20T21:40"),
            ("2015202T21", "2015-07-21T21:00"),
            (
                "-0001-12-31T23:59:59.999999999",
                "-0001-12-31T23:59:59.999999999",
            ),
        ] {
            assert_eq!(
                CanonicalLocalDateTime::parse_iso(text).map(|value| value.to_string()),
                Ok(expected.into()),
                "{text}"
            );
        }
    }

    #[test]
    fn text_that_names_no_time_or_is_not_a_time_refuses_with_its_reason() {
        use TimeError::{Date, Malformed, NoSuchTime, OffsetOutOfRange};
        for (text, error) in [
            ("24:00", NoSuchTime),
            ("23:60", NoSuchTime),
            ("23:59:60", NoSuchTime),
            ("", Malformed),
            ("2", Malformed),
            ("21:4032", Malformed),
            ("2140:32", Malformed),
            ("21:40:", Malformed),
            ("21:40.5", Malformed),
            ("21.5", Malformed),
            ("21:40:32.", Malformed),
            ("21:40:32.1234567891", Malformed),
            ("21:40:32,142", Malformed),
            ("21:40Z", Malformed),
            (" 21:40", Malformed),
        ] {
            assert_eq!(CanonicalLocalTime::parse_iso(text), Err(error), "{text}");
        }
        for (text, error) in [
            ("21:40+18:01", OffsetOutOfRange),
            ("21:40-19", OffsetOutOfRange),
            ("21:40+01:60", OffsetOutOfRange),
            ("21:40+1", Malformed),
            ("21:40+01:0", Malformed),
            ("21:40+0100:00", Malformed),
            ("21:40ZZ", Malformed),
            ("21:40+", Malformed),
            ("24:00Z", NoSuchTime),
        ] {
            assert_eq!(CanonicalTime::parse_iso(text), Err(error), "{text}");
        }
        // A date alone is midnight; a space is not the date-time separator,
        // so the whole text is read, and refused, as a date.
        assert_eq!(
            CanonicalLocalDateTime::parse_iso("2015-07-21").map(|value| value.to_string()),
            Ok("2015-07-21T00:00".into())
        );
        for (text, error) in [
            ("2015-02-29T12:00", Date(DateError::NoSuchDay)),
            ("2015-07-21T", Malformed),
            ("2015-07-21T25:00", NoSuchTime),
            ("2015-07-21 12:00", Date(DateError::Malformed)),
            ("2015-02-29", Date(DateError::NoSuchDay)),
        ] {
            assert_eq!(
                CanonicalLocalDateTime::parse_iso(text),
                Err(error),
                "{text}"
            );
        }
        assert_eq!(
            CanonicalLocalTime::from_nanos_of_day(NANOS_PER_DAY),
            Err(NoSuchTime)
        );
        assert_eq!(
            CanonicalLocalTime::from_hms_nano(0, 0, 0, 1_000_000_000),
            Err(NoSuchTime)
        );
        assert_eq!(
            CanonicalTime::new(CanonicalLocalTime::MIDNIGHT, -18 * 3_600 - 1),
            Err(OffsetOutOfRange)
        );
    }

    #[test]
    fn fields_round_trip_through_text_and_order_follows_the_clock() {
        let mut previous = None;
        for nanos in (0..NANOS_PER_DAY).step_by(7_919_381_123).chain([
            NANOS_PER_DAY - 1,
            1,
            1_000,
            1_000_000,
            NANOS_PER_SECOND,
        ]) {
            let time = CanonicalLocalTime::from_nanos_of_day(nanos).unwrap();
            assert_eq!(CanonicalLocalTime::parse_iso(&time.to_string()), Ok(time));
            assert_eq!(
                CanonicalLocalTime::from_hms_nano(
                    time.hour(),
                    time.minute(),
                    time.second(),
                    time.nanosecond()
                ),
                Ok(time)
            );
            if let Some(previous) = previous
                && nanos > CanonicalLocalTime::nanos_of_day(previous)
            {
                assert!(previous < time);
            }
            previous = Some(time);
            for offset in [0, 3_600, -5_400, 18 * 3_600, -(2 * 3_600 + 5 * 60 + 7)] {
                let at = CanonicalTime::new(time, offset).unwrap();
                assert_eq!(CanonicalTime::parse_iso(&at.to_string()), Ok(at));
                assert_eq!(
                    CanonicalTime::from_utc_nanos_of_day(at.utc_nanos_of_day(), time),
                    Ok(at)
                );
            }
        }
        let date = CanonicalDate::from_civil(1984, 10, 11).unwrap();
        let time = CanonicalLocalTime::from_hms_nano(12, 31, 14, 645_876_123).unwrap();
        let value = CanonicalLocalDateTime::new(date, time);
        assert_eq!(value.to_string(), "1984-10-11T12:31:14.645876123");
        assert_eq!(
            CanonicalLocalDateTime::parse_iso(&value.to_string()),
            Ok(value)
        );
        let next_day = CanonicalLocalDateTime::new(
            CanonicalDate::from_epoch_day(date.epoch_day() + 1).unwrap(),
            CanonicalLocalTime::MIDNIGHT,
        );
        assert!(value < next_day);
    }

    #[test]
    fn times_at_offsets_order_by_utc_then_local_and_equal_only_when_both_match() {
        let at = |text: &str| CanonicalTime::parse_iso(text).unwrap();
        // 12:00+01:00 and 11:00Z are the same UTC time; the earlier local
        // time orders first, and they are not equal.
        assert!(at("11:00Z") < at("12:00+01:00"));
        assert_ne!(at("11:00Z"), at("12:00+01:00"));
        assert!(at("12:00+01:00") < at("11:30Z"));
        // Offsets carry the UTC time of day past either end of the day.
        assert!(at("01:00+05:00") < at("00:00Z"));
        assert!(at("23:00Z") < at("20:00-04:00"));
        assert_eq!(
            at("20:00-04:00").utc_nanos_of_day(),
            24 * 3_600 * NANOS_PER_SECOND as i64
        );
        assert_eq!(at("21:40-00:00"), at("21:40Z"));
        assert_eq!(parse_utc_offset("-02:05:07"), Ok(-7_507));
        assert_eq!(
            parse_utc_offset("Europe/Stockholm"),
            Err(TimeError::Malformed)
        );
        for offset in [0, 3_600, -5_400, -7_507, MAX_UTC_OFFSET_SECONDS] {
            assert_eq!(parse_utc_offset(&format_utc_offset(offset)), Ok(offset));
        }
        assert_eq!(format_utc_offset(0), "Z");
        // A difference that is not whole seconds names no offset.
        assert_eq!(
            CanonicalTime::from_utc_nanos_of_day(1, CanonicalLocalTime::MIDNIGHT),
            Err(TimeError::OffsetOutOfRange)
        );
    }
}
