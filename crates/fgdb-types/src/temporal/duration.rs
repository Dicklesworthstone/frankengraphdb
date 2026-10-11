//! Canonical durations (fgdb-txbg9): months, days and a time part in
//! nanoseconds, kept apart because none converts exactly into another (a
//! month is not a fixed number of days, nor a day of seconds across a zone
//! change). This is openCypher's duration.
//!
//! Equality and hashing are those of the three components, so P1M and P30D
//! differ. The total order is canonical (months, then days, then the time
//! part), not chronological; openCypher compares durations only for
//! equality. Text is ISO 8601 with a sign on each component that needs one.

use std::fmt;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// The seconds a month stands for when a fraction of one cascades down: a
/// twelfth of the Gregorian mean year, 365.2425 days.
pub const SECONDS_PER_AVERAGE_MONTH: i64 = 2_629_746;

/// Bound on the time part: as many whole seconds as an `i64` holds.
const MAX_TIME_NANOS: i128 = i64::MAX as i128 * NANOS_PER_SECOND + (NANOS_PER_SECOND - 1);

/// A duration of `months` months, `days` days and `nanos` nanoseconds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CanonicalDuration {
    months: i64,
    days: i64,
    nanos: i128,
}

/// Why a duration could not be built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DurationError {
    /// A component overflows its range.
    OutOfRange,
    /// The text is not an ISO 8601 duration this profile reads.
    Malformed,
}

impl fmt::Display for DurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OutOfRange => "duration component out of range",
            Self::Malformed => "not an ISO 8601 duration",
        })
    }
}

impl std::error::Error for DurationError {}

impl CanonicalDuration {
    /// The duration of no length, `PT0S`.
    pub const ZERO: Self = Self {
        months: 0,
        days: 0,
        nanos: 0,
    };

    /// `months` months, `days` days and `nanos` nanoseconds.
    ///
    /// # Errors
    ///
    /// [`DurationError::OutOfRange`] when the time part exceeds an `i64`
    /// of whole seconds.
    pub const fn new(months: i64, days: i64, nanos: i128) -> Result<Self, DurationError> {
        if nanos > MAX_TIME_NANOS || nanos < -MAX_TIME_NANOS {
            return Err(DurationError::OutOfRange);
        }
        Ok(Self {
            months,
            days,
            nanos,
        })
    }

    /// The months component.
    #[must_use]
    pub const fn months(self) -> i64 {
        self.months
    }

    /// The days component.
    #[must_use]
    pub const fn days(self) -> i64 {
        self.days
    }

    /// The time part in nanoseconds.
    #[must_use]
    pub const fn nanos(self) -> i128 {
        self.nanos
    }

    /// The sum of two durations, component by component.
    ///
    /// # Errors
    ///
    /// [`DurationError::OutOfRange`] on overflow.
    pub fn checked_add(self, other: Self) -> Result<Self, DurationError> {
        Self::new(
            self.months
                .checked_add(other.months)
                .ok_or(DurationError::OutOfRange)?,
            self.days
                .checked_add(other.days)
                .ok_or(DurationError::OutOfRange)?,
            self.nanos + other.nanos,
        )
    }

    /// The duration times the exact ratio `numerator / denominator`, as
    /// openCypher multiplies and divides a duration by a number: each
    /// component is scaled, and what a whole count leaves over cascades down,
    /// a fraction of a month into days and time by the average month and a
    /// fraction of a day into time, each step truncated toward zero. The
    /// ratio is exact, so the result never depends on binary64 rounding.
    ///
    /// # Errors
    ///
    /// [`DurationError::OutOfRange`] when `denominator` is not positive or a
    /// component overflows.
    pub fn checked_scale(self, numerator: i128, denominator: i128) -> Result<Self, DurationError> {
        const DAY_NANOS: i128 = 86_400 * NANOS_PER_SECOND;
        const MONTH_NANOS: i128 = SECONDS_PER_AVERAGE_MONTH as i128 * NANOS_PER_SECOND;
        let out = || DurationError::OutOfRange;
        if denominator <= 0 {
            return Err(out());
        }
        let months = i128::from(self.months)
            .checked_mul(numerator)
            .ok_or_else(out)?;
        let (whole_months, left_months) = (months / denominator, months % denominator);
        // Everything below the months is counted in nanoseconds over the
        // same denominator, so no step rounds before its own truncation.
        let day_nanos = i128::from(self.days)
            .checked_mul(numerator)
            .and_then(|days| days.checked_mul(DAY_NANOS))
            .and_then(|days| days.checked_add(left_months.checked_mul(MONTH_NANOS)?))
            .ok_or_else(out)?;
        let day_unit = denominator.checked_mul(DAY_NANOS).ok_or_else(out)?;
        let whole_days = day_nanos / day_unit;
        let left_nanos = day_nanos - whole_days * day_unit;
        let time = self
            .nanos
            .checked_mul(numerator)
            .and_then(|nanos| nanos.checked_add(left_nanos))
            .ok_or_else(out)?
            / denominator;
        Self::new(
            i64::try_from(whole_months).map_err(|_| out())?,
            i64::try_from(whole_days).map_err(|_| out())?,
            time,
        )
    }

    /// The duration with every component negated.
    ///
    /// # Errors
    ///
    /// [`DurationError::OutOfRange`] for `i64::MIN` months or days.
    pub fn checked_neg(self) -> Result<Self, DurationError> {
        Self::new(
            self.months.checked_neg().ok_or(DurationError::OutOfRange)?,
            self.days.checked_neg().ok_or(DurationError::OutOfRange)?,
            -self.nanos,
        )
    }

    /// Read an ISO 8601 duration: `P`, then signed, optionally fractional
    /// counts of years `Y`, months `M`, weeks `W` and days `D`, then `T` and
    /// hours `H`, minutes `M` and seconds `S` (`P12Y5M-14DT16H`, `PT0.75M`,
    /// `P2.5W`), or the date-time form `PYYYY-MM-DDThh:mm:ss[.f]`. A leading
    /// `-` negates the whole. A fraction cascades down: of a year into months,
    /// of a month into days and seconds by the average month, of a week into
    /// days, of a day into seconds, of an hour or minute into seconds, and of
    /// a second into nanoseconds, rounded to the nanosecond.
    ///
    /// # Errors
    ///
    /// [`DurationError::Malformed`] for any other text,
    /// [`DurationError::OutOfRange`] on overflow.
    pub fn parse_iso(text: &str) -> Result<Self, DurationError> {
        use DurationError::Malformed;
        let (negative, body) = match text.as_bytes() {
            [b'-', b'P', ..] => (true, &text[2..]),
            [b'P', ..] => (false, &text[1..]),
            _ => return Err(Malformed),
        };
        let bytes = body.as_bytes();
        let alternative =
            bytes.len() > 4 && bytes[..4].iter().all(u8::is_ascii_digit) && bytes[4] == b'-';
        let parsed = if alternative {
            parse_alternative(body)?
        } else {
            parse_designators(body)?
        };
        if negative {
            parsed.checked_neg()
        } else {
            Ok(parsed)
        }
    }
}

/// Accumulates fractional components exactly: each count is an integer and
/// a decimal fraction of up to nine digits, so every cascade below lands on
/// whole nanoseconds or is rounded once at the end.
#[derive(Default)]
struct Builder {
    months: i128,
    days: i128,
    /// The time part in units of 10^-9 nanoseconds, so a fraction of a
    /// second, minute, hour, day or month cascades without rounding until
    /// [`Builder::finish`].
    scaled_nanos: i128,
}

const SCALE: i128 = 1_000_000_000;

/// A unit a duration's count can be given in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DurationUnit {
    Years,
    Months,
    Weeks,
    Days,
    Hours,
    Minutes,
    Seconds,
    Milliseconds,
    Microseconds,
    Nanoseconds,
}

impl CanonicalDuration {
    /// The sum of counts `whole + fraction / 10^9` of their units, `whole`
    /// and `fraction` sharing a sign. Fractions cascade down as in
    /// [`Self::parse_iso`], rounded once to the nanosecond.
    ///
    /// # Errors
    ///
    /// [`DurationError::OutOfRange`] on overflow.
    pub fn from_parts(
        parts: impl IntoIterator<Item = (DurationUnit, i128, i128)>,
    ) -> Result<Self, DurationError> {
        let mut builder = Builder::default();
        for (unit, whole, fraction) in parts {
            builder.add(unit, whole, fraction)?;
        }
        builder.finish()
    }
}

impl Builder {
    /// Add `whole + fraction / 10^9` of `unit`; `whole` and `fraction` share
    /// a sign, and so does every part a fraction cascades into (division
    /// truncates toward zero).
    fn add(
        &mut self,
        unit: DurationUnit,
        whole: i128,
        fraction: i128,
    ) -> Result<(), DurationError> {
        use DurationError::OutOfRange;
        let scaled = |count: i128, per_unit: i128| {
            count
                .checked_mul(SCALE)
                .and_then(|count| count.checked_add(fraction))
                .and_then(|count| count.checked_mul(per_unit))
                .ok_or(OutOfRange)
        };
        let time = |per_unit: i128| scaled(whole, per_unit);
        match unit {
            DurationUnit::Years => {
                // Twelve months a year: 12 * fraction keeps its 10^-9 scale.
                let months = scaled(whole, 12)?;
                self.months = add(self.months, months / SCALE)?;
                self.add_month_fraction(months % SCALE)?;
            }
            DurationUnit::Months => {
                self.months = add(self.months, whole)?;
                self.add_month_fraction(fraction)?;
            }
            DurationUnit::Weeks => {
                let days = scaled(whole, 7)?;
                self.days = add(self.days, days / SCALE)?;
                self.scaled_nanos = add(
                    self.scaled_nanos,
                    (days % SCALE) * 86_400 * NANOS_PER_SECOND,
                )?;
            }
            DurationUnit::Days => {
                self.days = add(self.days, whole)?;
                self.scaled_nanos = add(self.scaled_nanos, fraction * 86_400 * NANOS_PER_SECOND)?;
            }
            DurationUnit::Hours => {
                self.scaled_nanos = add(self.scaled_nanos, time(3_600 * NANOS_PER_SECOND)?)?;
            }
            DurationUnit::Minutes => {
                self.scaled_nanos = add(self.scaled_nanos, time(60 * NANOS_PER_SECOND)?)?;
            }
            DurationUnit::Seconds => {
                self.scaled_nanos = add(self.scaled_nanos, time(NANOS_PER_SECOND)?)?;
            }
            DurationUnit::Milliseconds => {
                self.scaled_nanos = add(self.scaled_nanos, time(1_000_000)?)?;
            }
            DurationUnit::Microseconds => {
                self.scaled_nanos = add(self.scaled_nanos, time(1_000)?)?;
            }
            DurationUnit::Nanoseconds => {
                self.scaled_nanos = add(self.scaled_nanos, time(1)?)?;
            }
        }
        Ok(())
    }

    /// A fraction of a month, in 10^-9 months, as whole days and seconds of
    /// the average month.
    fn add_month_fraction(&mut self, fraction: i128) -> Result<(), DurationError> {
        let scaled = fraction * i128::from(SECONDS_PER_AVERAGE_MONTH) * NANOS_PER_SECOND;
        let day = 86_400 * NANOS_PER_SECOND * SCALE;
        self.days = add(self.days, scaled / day)?;
        self.scaled_nanos = add(self.scaled_nanos, scaled % day)?;
        Ok(())
    }

    fn finish(self) -> Result<CanonicalDuration, DurationError> {
        let nanos = rounded_div(self.scaled_nanos, SCALE);
        CanonicalDuration::new(
            i64::try_from(self.months).map_err(|_| DurationError::OutOfRange)?,
            i64::try_from(self.days).map_err(|_| DurationError::OutOfRange)?,
            nanos,
        )
    }
}

fn add(left: i128, right: i128) -> Result<i128, DurationError> {
    left.checked_add(right).ok_or(DurationError::OutOfRange)
}

/// `value / divisor` rounded half away from zero.
fn rounded_div(value: i128, divisor: i128) -> i128 {
    let quotient = value / divisor;
    let remainder = value % divisor;
    if remainder.abs() * 2 >= divisor {
        quotient + value.signum()
    } else {
        quotient
    }
}

/// A signed decimal count: `-`? digits (`.` digits)?, as a whole part and a
/// fraction in 10^-9. At most eighteen integer and nine fraction digits.
fn decimal(text: &str) -> Result<(i128, i128), DurationError> {
    use DurationError::Malformed;
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let separators = &['.', ','][..];
    let (whole, fraction) = digits.split_once(separators).unwrap_or((digits, ""));
    if whole.is_empty()
        || whole.len() > 18
        || fraction.len() > 9
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || (digits.contains(separators) && fraction.is_empty())
    {
        return Err(Malformed);
    }
    let whole: i128 = whole.parse().map_err(|_| Malformed)?;
    let fraction = fraction
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(9)
        .fold(0_i128, |value, digit| value * 10 + i128::from(digit - b'0'));
    Ok(if negative {
        (-whole, -fraction)
    } else {
        (whole, fraction)
    })
}

/// `nY nM nW nD [T nH nM nS]`, each optional but in that order, at least one.
fn parse_designators(body: &str) -> Result<CanonicalDuration, DurationError> {
    use DurationError::Malformed;
    let (date, time) = match body.split_once('T') {
        Some((date, time)) if !time.is_empty() => (date, Some(time)),
        Some(_) => return Err(Malformed),
        None => (body, None),
    };
    use DurationUnit as U;
    let mut builder = Builder::default();
    let mut any = false;
    let date_units = [U::Years, U::Months, U::Weeks, U::Days];
    let time_units = [U::Hours, U::Minutes, U::Seconds];
    for (part, designators, units) in [
        (Some(date), &b"YMWD"[..], &date_units[..]),
        (time, &b"HMS"[..], &time_units[..]),
    ] {
        let Some(mut part) = part else { continue };
        let mut next = 0;
        while !part.is_empty() {
            let end = part
                .find(|c: char| c.is_ascii_alphabetic())
                .ok_or(Malformed)?;
            let designator = part.as_bytes()[end];
            let at = designators[next..]
                .iter()
                .position(|candidate| *candidate == designator)
                .ok_or(Malformed)?
                + next;
            let (whole, fraction) = decimal(&part[..end])?;
            builder.add(units[at], whole, fraction)?;
            any = true;
            next = at + 1;
            part = &part[end + 1..];
        }
    }
    if !any {
        return Err(Malformed);
    }
    builder.finish()
}

/// `YYYY-MM-DDThh:mm:ss[.f]`: years, months, days, hours, minutes and
/// seconds as counts.
fn parse_alternative(body: &str) -> Result<CanonicalDuration, DurationError> {
    use DurationError::Malformed;
    let (date, time) = body.split_once('T').ok_or(Malformed)?;
    let date: Vec<&str> = date.split('-').collect();
    let time: Vec<&str> = time.split(':').collect();
    let ([year, month, day], [hour, minute, second]) = (date.as_slice(), time.as_slice()) else {
        return Err(Malformed);
    };
    let mut builder = Builder::default();
    for (count, unit, digits) in [
        (year, DurationUnit::Years, 4),
        (month, DurationUnit::Months, 2),
        (day, DurationUnit::Days, 2),
        (hour, DurationUnit::Hours, 2),
        (minute, DurationUnit::Minutes, 2),
    ] {
        if count.len() != digits || !count.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Malformed);
        }
        builder.add(unit, count.parse().map_err(|_| Malformed)?, 0)?;
    }
    if second.len() < 2 || !second.as_bytes()[..2].iter().all(u8::is_ascii_digit) {
        return Err(Malformed);
    }
    let (whole, fraction) = decimal(second)?;
    builder.add(DurationUnit::Seconds, whole, fraction)?;
    builder.finish()
}

/// openCypher's spelling: `P`, then years `Y`, months `M` and days `D`,
/// then `T` and hours `H`, minutes `M` and seconds `S` with the fraction's
/// trailing zeros dropped. Each part is shown only when it is not zero, with
/// its own sign: months split into years and months, and the time part into
/// hours, minutes and seconds, each truncated toward zero. No length at all
/// is `PT0S`.
impl fmt::Display for CanonicalDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("P")?;
        let (years, months) = (self.months / 12, self.months % 12);
        for (count, designator) in [(years, 'Y'), (months, 'M'), (self.days, 'D')] {
            if count != 0 {
                write!(f, "{count}{designator}")?;
            }
        }
        if self.nanos == 0 {
            if self.months == 0 && self.days == 0 {
                f.write_str("T0S")?;
            }
            return Ok(());
        }
        f.write_str("T")?;
        let hour = 3_600 * NANOS_PER_SECOND;
        let minute = 60 * NANOS_PER_SECOND;
        let (hours, rest) = (self.nanos / hour, self.nanos % hour);
        let (minutes, rest) = (rest / minute, rest % minute);
        for (count, designator) in [(hours, 'H'), (minutes, 'M')] {
            if count != 0 {
                write!(f, "{count}{designator}")?;
            }
        }
        if rest != 0 {
            let sign = if rest < 0 { "-" } else { "" };
            let (seconds, fraction) = (
                rest.unsigned_abs() / NANOS_PER_SECOND.unsigned_abs(),
                rest.unsigned_abs() % NANOS_PER_SECOND.unsigned_abs(),
            );
            write!(f, "{sign}{seconds}")?;
            if fraction != 0 {
                let digits = format!("{fraction:09}");
                write!(f, ".{}", digits.trim_end_matches('0'))?;
            }
            f.write_str("S")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(source: &str) -> Result<String, DurationError> {
        CanonicalDuration::parse_iso(source).map(|duration| duration.to_string())
    }

    #[test]
    fn every_tck_text_reads_and_prints_as_the_tck_spells_it() {
        // TCK Temporal2 [7].
        for (source, expected) in [
            ("P14DT16H12M", "P14DT16H12M"),
            ("P5M1.5D", "P5M1DT12H"),
            ("P0.75M", "P22DT19H51M49.5S"),
            ("PT0.75M", "PT45S"),
            ("P2.5W", "P17DT12H"),
            ("P12Y5M14DT16H12M70S", "P12Y5M14DT16H13M10S"),
            ("P2012-02-02T14:37:21.545", "P2012Y2M2DT14H37M21.545S"),
        ] {
            assert_eq!(text(source), Ok(expected.into()), "{source}");
        }
        // TCK Temporal6 [6]: each spelling reads back as itself.
        for spelling in [
            "P12Y5M14DT16H13M10.000000001S",
            "P12Y5M-14DT16H",
            "PT11M",
            "PT1.999S",
            "PT-1.999S",
            "PT-2.001S",
            "P1DT0.001S",
            "P1DT-0.001S",
            "PT59.999S",
            "PT-59.999S",
            "PT-1M-0.001S",
            "PT0S",
        ] {
            assert_eq!(text(spelling), Ok(spelling.into()), "{spelling}");
        }
        assert_eq!(text("-P1Y2DT3S"), Ok("P-1Y-2DT-3S".into()));
        assert_eq!(text("PT1,5S"), Ok("PT1.5S".into()));
    }

    #[test]
    fn components_stay_apart_and_fractions_cascade_down() {
        let parsed = CanonicalDuration::parse_iso("P1M30DT24H").unwrap();
        assert_eq!(
            (parsed.months(), parsed.days(), parsed.nanos()),
            (1, 30, 24 * 3_600 * NANOS_PER_SECOND)
        );
        assert_ne!(
            CanonicalDuration::parse_iso("P1M").unwrap(),
            CanonicalDuration::parse_iso("P30D").unwrap()
        );
        assert_ne!(
            CanonicalDuration::parse_iso("P1D").unwrap(),
            CanonicalDuration::parse_iso("PT24H").unwrap()
        );
        // A tenth of a year is 1.2 months: one month and 0.2 of the average
        // month, 525,949.2 seconds, which is 6 days and 7,549.2 seconds.
        let tenth = CanonicalDuration::parse_iso("P0.1Y").unwrap();
        assert_eq!(
            (tenth.months(), tenth.days(), tenth.nanos()),
            (1, 6, 7_549_200_000_000)
        );
        // A negative fraction cascades with its sign: every part is negative.
        let back = CanonicalDuration::parse_iso("P-0.1Y").unwrap();
        assert_eq!(
            (back.months(), back.days(), back.nanos()),
            (-1, -6, -7_549_200_000_000)
        );
        // A fraction past nine digits is beyond a nanosecond and refuses.
        assert_eq!(
            CanonicalDuration::parse_iso("PT0.0000000005S"),
            Err(DurationError::Malformed)
        );
    }

    #[test]
    fn malformed_text_and_overflow_refuse() {
        use DurationError::{Malformed, OutOfRange};
        for (source, error) in [
            ("", Malformed),
            ("P", Malformed),
            ("PT", Malformed),
            ("P1", Malformed),
            ("P1S", Malformed),
            ("PT1D", Malformed),
            ("P1D1M", Malformed),
            ("P1Y1Y", Malformed),
            ("1D", Malformed),
            ("P1.D", Malformed),
            ("P.5D", Malformed),
            ("P1DT", Malformed),
            ("P2012-02-02", Malformed),
            ("P2012-2-02T14:37:21", Malformed),
            ("P--1D", Malformed),
            ("P9999999999999999999D", Malformed),
            ("P999999999999999999Y", OutOfRange),
        ] {
            assert_eq!(CanonicalDuration::parse_iso(source), Err(error), "{source}");
        }
        assert_eq!(
            CanonicalDuration::new(0, 0, MAX_TIME_NANOS + 1),
            Err(OutOfRange)
        );
    }

    /// TCK Temporal8 [7]: a duration times 1, 2 and 1/2. Half of 149 months
    /// leaves half an average month, 15 days and 18,873 seconds, which joins
    /// the halved days and time; the half nanosecond truncates. A negative
    /// ratio truncates toward zero as well, so it is the exact negation.
    #[test]
    fn scaling_follows_the_tck_and_truncates_each_cascade_toward_zero() {
        let base = CanonicalDuration::parse_iso("P12Y5M14DT16H13M10.000000001S").unwrap();
        for (numerator, denominator, expected) in [
            (1, 1, "P12Y5M14DT16H13M10.000000001S"),
            (2, 1, "P24Y10M28DT32H26M20.000000002S"),
            (1, 2, "P6Y2M22DT13H21M8S"),
            (-1, 2, "P-6Y-2M-22DT-13H-21M-8S"),
            (0, 7, "PT0S"),
        ] {
            assert_eq!(
                base.checked_scale(numerator, denominator)
                    .map(|duration| duration.to_string()),
                Ok(expected.to_owned()),
                "{numerator}/{denominator}"
            );
        }
        assert_eq!(base.checked_scale(1, 0), Err(DurationError::OutOfRange));
        assert_eq!(base.checked_scale(1, -2), Err(DurationError::OutOfRange));
        assert_eq!(
            base.checked_scale(i128::MAX, 1),
            Err(DurationError::OutOfRange)
        );
    }
}
