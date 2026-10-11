//! Temporal values in the value language (fgdb-txbg9): the openCypher
//! constructors and field accessors. Pure arithmetic over the scalar domain:
//! no clock, no zone database, no locale. The default offset is UTC.

use super::GraphTemporalFunction;
use crate::GraphIntegerErrorKind;
use crate::GraphIntegerErrorKind::TemporalConstruction as Invalid;
use crate::algebra::GraphValue;
use fgdb_types::{
    CanonicalDate, CanonicalLocalDateTime, CanonicalLocalTime, CanonicalScalar, CanonicalTime,
    CanonicalTimestamp, NANOS_PER_DAY, format_utc_offset, parse_utc_offset,
};

/// The calendar fields a map may name, in the order `date_from_fields`
/// reads them.
const DATE_FIELDS: [&str; 8] = [
    "year",
    "month",
    "day",
    "week",
    "dayOfWeek",
    "ordinalDay",
    "quarter",
    "dayOfQuarter",
];

/// The clock fields a map may name, in the order `time_from_fields` reads
/// them.
const TIME_FIELDS: [&str; 6] = [
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
];

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// A map argument split into its fields and bases.
#[derive(Default)]
struct MapArguments<'a> {
    date: [Option<i64>; 8],
    time: [Option<i64>; 6],
    /// `date: x`, whose calendar fields fill the ones not given.
    date_base: Option<&'a GraphValue>,
    /// `time: x`, whose clock fields (and offset, for `time`) fill in.
    time_base: Option<&'a GraphValue>,
    /// `datetime: x`, a base for both parts of a local date-time.
    datetime_base: Option<&'a GraphValue>,
    /// `timezone: '+01:00'`.
    timezone: Option<&'a str>,
    /// `epochSeconds: n` or `epochMillis: n`, for a datetime.
    epoch_seconds: Option<i64>,
    epoch_millis: Option<i64>,
}

impl<'a> MapArguments<'a> {
    /// The keys `function` takes: a date takes calendar fields and `date`;
    /// a local time and a time take clock fields and `time`, and a time also
    /// `timezone`; a local date-time takes both sets of fields and `date`,
    /// `time` and `datetime`; a datetime takes what a local date-time does,
    /// `timezone`, `epochSeconds` and `epochMillis`. Any other key, or a
    /// field that is not an integer, refuses.
    fn split(
        function: GraphTemporalFunction,
        keys: &'a [Box<str>],
        values: &'a [GraphValue],
    ) -> Result<Self, GraphIntegerErrorKind> {
        use GraphTemporalFunction as F;
        let (dates, times) = match function {
            F::Date => (true, false),
            F::LocalTime | F::Time => (false, true),
            F::LocalDateTime | F::DateTime => (true, true),
        };
        let zoned = matches!(function, F::Time | F::DateTime);
        let mut arguments = Self::default();
        for (key, value) in keys.iter().zip(values) {
            let key = &**key;
            match key {
                "date" if dates => arguments.date_base = Some(value),
                "time" if times => arguments.time_base = Some(value),
                "datetime" if dates && times => arguments.datetime_base = Some(value),
                "timezone" if zoned => match value {
                    GraphValue::Scalar(CanonicalScalar::Text(zone)) => {
                        arguments.timezone = Some(zone.as_str());
                    }
                    _ => return Err(Invalid),
                },
                "epochSeconds" if function == F::DateTime => {
                    arguments.epoch_seconds = Some(integer(value)?);
                }
                "epochMillis" if function == F::DateTime => {
                    arguments.epoch_millis = Some(integer(value)?);
                }
                _ => {
                    let number = integer(value)?;
                    let slot = |names: &[&str], allowed: bool| {
                        names
                            .iter()
                            .position(|name| *name == key)
                            .filter(|_| allowed)
                    };
                    if let Some(at) = slot(&DATE_FIELDS[..], dates) {
                        arguments.date[at] = Some(number);
                    } else if let Some(at) = slot(&TIME_FIELDS[..], times) {
                        arguments.time[at] = Some(number);
                    } else {
                        return Err(Invalid);
                    }
                }
            }
        }
        Ok(arguments)
    }
}

fn integer(value: &GraphValue) -> Result<i64, GraphIntegerErrorKind> {
    match value {
        GraphValue::Scalar(CanonicalScalar::Int(number)) => Ok(*number),
        _ => Err(Invalid),
    }
}

/// `function(argument)`: NULL for NULL, ISO 8601 for text, a map of fields
/// over optional bases, or a temporal value of another kind converted (its
/// date part, its time part, a local value at UTC).
pub(crate) fn construct(
    function: GraphTemporalFunction,
    argument: &GraphValue,
) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
    if matches!(argument, GraphValue::Scalar(CanonicalScalar::Null)) {
        return Ok(CanonicalScalar::Null);
    }
    match function {
        GraphTemporalFunction::Date => date(argument).map(CanonicalScalar::Date),
        GraphTemporalFunction::LocalTime => local_time(argument).map(CanonicalScalar::LocalTime),
        GraphTemporalFunction::Time => time(argument).map(CanonicalScalar::Time),
        GraphTemporalFunction::LocalDateTime => {
            local_datetime(argument).map(CanonicalScalar::LocalDateTime)
        }
        GraphTemporalFunction::DateTime => datetime(argument).map(CanonicalScalar::Timestamp),
    }
}

fn date(argument: &GraphValue) -> Result<CanonicalDate, GraphIntegerErrorKind> {
    match argument {
        GraphValue::Scalar(CanonicalScalar::Text(text)) => {
            CanonicalDate::parse_iso(text.as_str()).map_err(|_| Invalid)
        }
        GraphValue::Map { keys, values } => {
            let arguments = MapArguments::split(GraphTemporalFunction::Date, keys, values)?;
            let base = arguments.date_base.map(date_part).transpose()?;
            date_from_fields(arguments.date, base)
        }
        other => date_part(other),
    }
}

fn local_time(argument: &GraphValue) -> Result<CanonicalLocalTime, GraphIntegerErrorKind> {
    match argument {
        GraphValue::Scalar(CanonicalScalar::Text(text)) => {
            CanonicalLocalTime::parse_iso(text.as_str()).map_err(|_| Invalid)
        }
        GraphValue::Map { keys, values } => {
            let arguments = MapArguments::split(GraphTemporalFunction::LocalTime, keys, values)?;
            let base = arguments.time_base.map(time_part).transpose()?;
            time_from_fields(arguments.time, base.map(|(local, _)| local), false)
        }
        other => time_part(other).map(|(local, _)| local),
    }
}

/// A time map's `timezone` moves a base that has an offset to the same
/// instant at the new offset, and puts a local base at it.
fn time(argument: &GraphValue) -> Result<CanonicalTime, GraphIntegerErrorKind> {
    match argument {
        GraphValue::Scalar(CanonicalScalar::Text(text)) => {
            CanonicalTime::parse_iso(text.as_str()).map_err(|_| Invalid)
        }
        GraphValue::Map { keys, values } => {
            let arguments = MapArguments::split(GraphTemporalFunction::Time, keys, values)?;
            let base = arguments.time_base.map(time_part).transpose()?;
            let local = time_from_fields(arguments.time, base.map(|(local, _)| local), false)?;
            let zone = arguments
                .timezone
                .map(|zone| parse_utc_offset(zone).map_err(|_| Invalid))
                .transpose()?;
            let (local, offset) = match (base.and_then(|(_, offset)| offset), zone) {
                (Some(from), Some(to)) => (shift(local, to - from), to),
                (from, to) => (local, to.or(from).unwrap_or(0)),
            };
            CanonicalTime::new(local, offset).map_err(|_| Invalid)
        }
        other => {
            let (local, offset) = time_part(other)?;
            CanonicalTime::new(local, offset.unwrap_or(0)).map_err(|_| Invalid)
        }
    }
}

fn local_datetime(argument: &GraphValue) -> Result<CanonicalLocalDateTime, GraphIntegerErrorKind> {
    match argument {
        GraphValue::Scalar(CanonicalScalar::Text(text)) => {
            CanonicalLocalDateTime::parse_iso(text.as_str()).map_err(|_| Invalid)
        }
        GraphValue::Map { keys, values } => {
            let arguments =
                MapArguments::split(GraphTemporalFunction::LocalDateTime, keys, values)?;
            let both = arguments.datetime_base.map(datetime_part).transpose()?;
            let date_base = match arguments.date_base {
                Some(base) => Some(date_part(base)?),
                None => both.map(CanonicalLocalDateTime::date),
            };
            let time_base = match arguments.time_base {
                Some(base) => Some(time_part(base)?.0),
                None => both.map(CanonicalLocalDateTime::time),
            };
            Ok(CanonicalLocalDateTime::new(
                date_from_fields(arguments.date, date_base)?,
                time_from_fields(arguments.time, time_base, true)?,
            ))
        }
        other => datetime_part(other),
    }
}

/// A datetime is a timestamp at a UTC offset; without a `timezone` it is at
/// the offset of its time-bearing base, else at UTC. A named zone refuses.
/// A `timezone` moves a `datetime` base to the same instant at the new
/// offset, and a `time` base that has an offset to the same time of day at
/// it (the date stays); a date base never carries an offset.
fn datetime(argument: &GraphValue) -> Result<CanonicalTimestamp, GraphIntegerErrorKind> {
    match argument {
        GraphValue::Scalar(CanonicalScalar::Text(text)) => {
            // The offset starts at the first sign or Z after the T. A date
            // alone is its midnight at UTC (TCK: datetime('-999999999-01-01')).
            let text = text.as_str();
            let split = match text.find('T') {
                Some(clock) => text[clock..]
                    .find(&['+', '-', 'Z'][..])
                    .map_or(text.len(), |at| clock + at),
                None => text.len(),
            };
            let local = CanonicalLocalDateTime::parse_iso(&text[..split]).map_err(|_| Invalid)?;
            let offset = match &text[split..] {
                "" => 0,
                offset => parse_utc_offset(offset).map_err(|_| Invalid)?,
            };
            at_offset(local, offset)
        }
        GraphValue::Map { keys, values } => {
            let arguments = MapArguments::split(GraphTemporalFunction::DateTime, keys, values)?;
            if arguments.epoch_seconds.is_some() || arguments.epoch_millis.is_some() {
                return from_epoch(&arguments);
            }
            let both = arguments.datetime_base.map(zoned_part).transpose()?;
            let date_base = match arguments.date_base {
                Some(base) => Some(date_part(base)?),
                None => both.map(|(value, _)| value.date()),
            };
            let time_base = match arguments.time_base {
                Some(base) => Some(time_part(base)?),
                None => both.map(|(value, offset)| (value.time(), offset)),
            };
            let date = date_from_fields(arguments.date, date_base)?;
            let local = time_from_fields(arguments.time, time_base.map(|(time, _)| time), true)?;
            let zone = arguments
                .timezone
                .map(|zone| parse_utc_offset(zone).map_err(|_| Invalid))
                .transpose()?;
            match (time_base.and_then(|(_, offset)| offset), zone) {
                (Some(from), Some(to)) if arguments.time_base.is_none() => {
                    let instant = at_offset(CanonicalLocalDateTime::new(date, local), from)?;
                    CanonicalTimestamp::offset_only(instant.instant_utc_nanos(), to)
                        .map_err(|_| Invalid)
                }
                (Some(from), Some(to)) => at_offset(
                    CanonicalLocalDateTime::new(date, shift(local, to - from)),
                    to,
                ),
                (from, to) => at_offset(
                    CanonicalLocalDateTime::new(date, local),
                    to.or(from).unwrap_or(0),
                ),
            }
        }
        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => Ok(value.clone()),
        GraphValue::Scalar(CanonicalScalar::LocalDateTime(value)) => at_offset(*value, 0),
        _ => Err(Invalid),
    }
}

/// `datetime({epochSeconds: s[, nanosecond: n]})` or
/// `datetime({epochMillis: m})`, plus fraction fields, at an optional
/// `timezone`. Calendar and clock fields, and bases, refuse alongside.
fn from_epoch(arguments: &MapArguments<'_>) -> Result<CanonicalTimestamp, GraphIntegerErrorKind> {
    let [hour, minute, second, millisecond, microsecond, nanosecond] = arguments.time;
    if hour.or(minute).or(second).is_some()
        || arguments.date.iter().any(Option::is_some)
        || arguments.date_base.is_some()
        || arguments.time_base.is_some()
        || arguments.datetime_base.is_some()
    {
        return Err(Invalid);
    }
    let fraction = i128::from(fraction_nanos(millisecond, microsecond, nanosecond)?.unwrap_or(0));
    let instant = match (arguments.epoch_seconds, arguments.epoch_millis) {
        (Some(seconds), None) => i128::from(seconds) * NANOS_PER_SECOND + fraction,
        (None, Some(millis)) => i128::from(millis) * 1_000_000 + fraction,
        _ => return Err(Invalid),
    };
    let offset = arguments
        .timezone
        .map(|zone| parse_utc_offset(zone).map_err(|_| Invalid))
        .transpose()?
        .unwrap_or(0);
    CanonicalTimestamp::offset_only(instant, offset).map_err(|_| Invalid)
}

/// `local` at `offset_seconds` east of UTC, as a timestamp.
fn at_offset(
    local: CanonicalLocalDateTime,
    offset_seconds: i32,
) -> Result<CanonicalTimestamp, GraphIntegerErrorKind> {
    let wall = i128::from(local.date().epoch_day()) * i128::from(NANOS_PER_DAY)
        + i128::from(local.time().nanos_of_day());
    CanonicalTimestamp::offset_only(
        wall - i128::from(offset_seconds) * NANOS_PER_SECOND,
        offset_seconds,
    )
    .map_err(|_| Invalid)
}

/// A local date-time base and its offset if it has one: a local date-time,
/// or a timestamp's wall time at its offset.
fn zoned_part(
    value: &GraphValue,
) -> Result<(CanonicalLocalDateTime, Option<i32>), GraphIntegerErrorKind> {
    match value {
        GraphValue::Scalar(CanonicalScalar::LocalDateTime(value)) => Ok((*value, None)),
        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => {
            Ok((wall_clock(value)?, Some(value.utc_offset_seconds())))
        }
        _ => Err(Invalid),
    }
}

/// A timestamp's openCypher text: its wall-clock date-time, its offset, and
/// `[zone]` when it has a zone.
pub(crate) fn timestamp_text(value: &CanonicalTimestamp) -> Result<String, GraphIntegerErrorKind> {
    let mut text = wall_clock(value)?.to_string();
    text.push_str(&format_utc_offset(value.utc_offset_seconds()));
    if let Some(zone) = value.zone() {
        text.push('[');
        text.push_str(zone.identifier());
        text.push(']');
    }
    Ok(text)
}

/// The date of a date, a local date-time, or a timestamp's local wall time.
fn date_part(value: &GraphValue) -> Result<CanonicalDate, GraphIntegerErrorKind> {
    match value {
        GraphValue::Scalar(CanonicalScalar::Date(day)) => Ok(*day),
        GraphValue::Scalar(CanonicalScalar::LocalDateTime(value)) => Ok(value.date()),
        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => Ok(wall_clock(value)?.date()),
        _ => Err(Invalid),
    }
}

/// The local time of a time-bearing value, and its offset if it has one.
fn time_part(
    value: &GraphValue,
) -> Result<(CanonicalLocalTime, Option<i32>), GraphIntegerErrorKind> {
    match value {
        GraphValue::Scalar(CanonicalScalar::LocalTime(time)) => Ok((*time, None)),
        GraphValue::Scalar(CanonicalScalar::Time(time)) => {
            Ok((time.local(), Some(time.offset_seconds())))
        }
        GraphValue::Scalar(CanonicalScalar::LocalDateTime(value)) => Ok((value.time(), None)),
        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => {
            Ok((wall_clock(value)?.time(), Some(value.utc_offset_seconds())))
        }
        _ => Err(Invalid),
    }
}

/// A local date-time, or a timestamp's local wall time.
fn datetime_part(value: &GraphValue) -> Result<CanonicalLocalDateTime, GraphIntegerErrorKind> {
    match value {
        GraphValue::Scalar(CanonicalScalar::LocalDateTime(value)) => Ok(*value),
        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => wall_clock(value),
        _ => Err(Invalid),
    }
}

/// A timestamp's wall-clock date and time at its own offset; a timestamp
/// whose day lies outside the date range refuses.
fn wall_clock(value: &CanonicalTimestamp) -> Result<CanonicalLocalDateTime, GraphIntegerErrorKind> {
    let wall = value.local_wall_nanos();
    let per_day = i128::from(NANOS_PER_DAY);
    let day = i64::try_from(wall.div_euclid(per_day)).map_err(|_| Invalid)?;
    let nanos = u64::try_from(wall.rem_euclid(per_day)).map_err(|_| Invalid)?;
    Ok(CanonicalLocalDateTime::new(
        CanonicalDate::from_epoch_day(day).map_err(|_| Invalid)?,
        CanonicalLocalTime::from_nanos_of_day(nanos).map_err(|_| Invalid)?,
    ))
}

/// `local` moved by `seconds`, around the clock.
fn shift(local: CanonicalLocalTime, seconds: i32) -> CanonicalLocalTime {
    let nanos = (i128::from(local.nanos_of_day()) + i128::from(seconds) * NANOS_PER_SECOND)
        .rem_euclid(i128::from(NANOS_PER_DAY));
    u64::try_from(nanos)
        .ok()
        .and_then(|nanos| CanonicalLocalTime::from_nanos_of_day(nanos).ok())
        .expect("a time reduced modulo one day")
}

/// openCypher's date fields: those of one calendar (year, month and day;
/// ISO week year, week and dayOfWeek; year and ordinalDay; or year, quarter
/// and dayOfQuarter), optionally over a base date whose own fields in that
/// calendar fill the ones not given. Without a base the year is required and
/// a missing smaller field is its first value. A mix of calendars, or a day
/// that does not exist, refuses.
fn date_from_fields(
    fields: [Option<i64>; 8],
    base: Option<CanonicalDate>,
) -> Result<CanonicalDate, GraphIntegerErrorKind> {
    let [
        year,
        month,
        day,
        week,
        day_of_week,
        ordinal,
        quarter,
        day_of_quarter,
    ] = fields;
    let small = |value: Option<i64>| value.map(|value| u8::try_from(value).map_err(|_| Invalid));
    let calendars = [
        month.is_some() || day.is_some(),
        week.is_some() || day_of_week.is_some(),
        ordinal.is_some(),
        quarter.is_some() || day_of_quarter.is_some(),
    ];
    if calendars.iter().filter(|named| **named).count() > 1 {
        return Err(Invalid);
    }
    let built = if calendars[1] {
        let base = base.map(CanonicalDate::iso_week);
        let year = year.or(base.map(|week| week.0)).ok_or(Invalid)?;
        let week = small(week).transpose()?.or(base.map(|week| week.1));
        let weekday = small(day_of_week).transpose()?.or(base.map(|week| week.2));
        CanonicalDate::from_iso_week(year, week.unwrap_or(1), weekday.unwrap_or(1))
    } else if calendars[2] {
        let year = year.or(base.map(CanonicalDate::year)).ok_or(Invalid)?;
        let ordinal = ordinal.map_or(Err(Invalid), |ordinal| {
            u16::try_from(ordinal).map_err(|_| Invalid)
        })?;
        CanonicalDate::from_ordinal(year, ordinal)
    } else if calendars[3] {
        let year = year.or(base.map(CanonicalDate::year)).ok_or(Invalid)?;
        let base = base.map(CanonicalDate::quarter);
        let quarter = small(quarter)
            .transpose()?
            .or(base.map(|quarter| quarter.0));
        let day = small(day_of_quarter)
            .transpose()?
            .or(base.map(|quarter| quarter.1));
        CanonicalDate::from_quarter(year, quarter.unwrap_or(1), day.unwrap_or(1))
    } else {
        let base = base.map(CanonicalDate::civil);
        let year = year.or(base.map(|civil| civil.0)).ok_or(Invalid)?;
        let month = small(month).transpose()?.or(base.map(|civil| civil.1));
        let day = small(day).transpose()?.or(base.map(|civil| civil.2));
        CanonicalDate::from_civil(year, month.unwrap_or(1), day.unwrap_or(1))
    };
    built.map_err(|_| Invalid)
}

/// openCypher's clock fields over an optional base time. Without a base a
/// field needs every larger one (no minute without an hour), and the hour is
/// required unless `midnight` lets an empty clock be 00:00. Given fraction
/// fields replace the base's fraction and add up: millisecond 0-999, then
/// microsecond 0-999 after a millisecond (else 0-999999), then nanosecond
/// 0-999 after either (else 0-999999999).
fn time_from_fields(
    fields: [Option<i64>; 6],
    base: Option<CanonicalLocalTime>,
    midnight: bool,
) -> Result<CanonicalLocalTime, GraphIntegerErrorKind> {
    let [hour, minute, second, millisecond, microsecond, nanosecond] = fields;
    let fraction_given = millisecond.is_some() || microsecond.is_some() || nanosecond.is_some();
    if base.is_none() {
        let given = [
            hour.is_some(),
            minute.is_some(),
            second.is_some(),
            fraction_given,
        ];
        let first_missing = given.iter().position(|given| !given).unwrap_or(given.len());
        if given[first_missing..].iter().any(|given| *given) || (hour.is_none() && !midnight) {
            return Err(Invalid);
        }
    }
    let base = base.unwrap_or(CanonicalLocalTime::MIDNIGHT);
    let unit = |value: Option<i64>, limit: i64, from_base: u8| match value {
        None => Ok(from_base),
        Some(value) if (0..limit).contains(&value) => u8::try_from(value).map_err(|_| Invalid),
        Some(_) => Err(Invalid),
    };
    let fraction = fraction_nanos(millisecond, microsecond, nanosecond)?
        .unwrap_or_else(|| i64::from(base.nanosecond()));
    CanonicalLocalTime::from_hms_nano(
        unit(hour, 24, base.hour())?,
        unit(minute, 60, base.minute())?,
        unit(second, 60, base.second())?,
        u32::try_from(fraction).map_err(|_| Invalid)?,
    )
    .map_err(|_| Invalid)
}

/// The fraction of a second the given fraction fields add up to, `None`
/// when none is given: millisecond 0-999, then microsecond 0-999 after a
/// millisecond (else 0-999999), then nanosecond 0-999 after either (else
/// 0-999999999).
fn fraction_nanos(
    millisecond: Option<i64>,
    microsecond: Option<i64>,
    nanosecond: Option<i64>,
) -> Result<Option<i64>, GraphIntegerErrorKind> {
    if millisecond.is_none() && microsecond.is_none() && nanosecond.is_none() {
        return Ok(None);
    }
    let part = |value: Option<i64>, limit: i64| match value {
        None => Ok(0),
        Some(value) if (0..limit).contains(&value) => Ok(value),
        Some(_) => Err(Invalid),
    };
    let micro_limit = if millisecond.is_some() {
        1_000
    } else {
        1_000_000
    };
    let nano_limit = if millisecond.is_some() || microsecond.is_some() {
        1_000
    } else {
        1_000_000_000
    };
    Ok(Some(
        part(millisecond, 1_000)? * 1_000_000
            + part(microsecond, micro_limit)? * 1_000
            + part(nanosecond, nano_limit)?,
    ))
}

/// `value.key` for a temporal value: `None` when the value is not temporal,
/// `Some(Err(TemporalField))` for a key its kind does not have. Dates, local
/// date-times and datetimes have the calendar fields; times, local date-times
/// and datetimes the clock fields (a datetime's at its own offset); a time
/// and a datetime also `timezone` and `offset` (the offset's spelling, `Z`
/// for UTC; a zoned datetime's `timezone` is its zone), `offsetMinutes` and
/// `offsetSeconds`; a datetime also `epochSeconds` and `epochMillis`.
pub(crate) fn field(
    value: &CanonicalScalar,
    key: &str,
) -> Option<Result<CanonicalScalar, GraphIntegerErrorKind>> {
    let (date, time, offset, timestamp) = match value {
        CanonicalScalar::Date(day) => (Some(*day), None, None, None),
        CanonicalScalar::LocalTime(time) => (None, Some(*time), None, None),
        CanonicalScalar::Time(time) => {
            (None, Some(time.local()), Some(time.offset_seconds()), None)
        }
        CanonicalScalar::LocalDateTime(value) => {
            (Some(value.date()), Some(value.time()), None, None)
        }
        CanonicalScalar::Timestamp(value) => match wall_clock(value) {
            Ok(wall) => (
                Some(wall.date()),
                Some(wall.time()),
                Some(value.utc_offset_seconds()),
                Some(value),
            ),
            Err(kind) => return Some(Err(kind)),
        },
        _ => return None,
    };
    let found = date
        .and_then(|day| date_field(day, key))
        .or_else(|| time.and_then(|time| time_field(time, key)))
        .map(CanonicalScalar::Int)
        .or_else(|| timestamp.and_then(|value| timestamp_field(value, key)))
        .or_else(|| offset.and_then(|offset| offset_field(offset, key)));
    Some(found.ok_or(GraphIntegerErrorKind::TemporalField))
}

/// The scratch entries a field result takes: one for the value and one per
/// payload unit of its text (an offset spelling, or a zone of up to 255
/// bytes).
pub(crate) fn field_scratch_entries(value: &CanonicalScalar) -> usize {
    let text = match value {
        CanonicalScalar::Text(text) => text.len(),
        _ => 0,
    };
    1 + text.div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
}

/// A datetime's own fields: whole seconds and milliseconds since the epoch
/// (floored), and a zoned datetime's `timezone`, its zone.
fn timestamp_field(value: &CanonicalTimestamp, key: &str) -> Option<CanonicalScalar> {
    let instant = value.instant_utc_nanos();
    match key {
        "epochSeconds" => i64::try_from(instant.div_euclid(NANOS_PER_SECOND))
            .ok()
            .map(CanonicalScalar::Int),
        "epochMillis" => i64::try_from(instant.div_euclid(1_000_000))
            .ok()
            .map(CanonicalScalar::Int),
        "timezone" => value
            .zone()
            .and_then(|zone| CanonicalScalar::ucs_basic_text(zone.identifier()).ok()),
        _ => None,
    }
}

/// `day.key` for the openCypher date fields; `None` for any other key.
fn date_field(day: CanonicalDate, key: &str) -> Option<i64> {
    let (week_year, week, weekday) = day.iso_week();
    let (quarter, day_of_quarter) = day.quarter();
    Some(match key {
        "year" => day.year(),
        "quarter" => i64::from(quarter),
        "month" => i64::from(day.month()),
        "week" => i64::from(week),
        "weekYear" => week_year,
        "day" => i64::from(day.day()),
        "ordinalDay" => i64::from(day.ordinal_day()),
        "weekDay" | "dayOfWeek" => i64::from(weekday),
        "dayOfQuarter" => i64::from(day_of_quarter),
        _ => return None,
    })
}

/// `time.key` for the openCypher clock fields. `millisecond`, `microsecond`
/// and `nanosecond` each read the whole fraction of the second in that unit.
fn time_field(time: CanonicalLocalTime, key: &str) -> Option<i64> {
    let fraction = i64::from(time.nanosecond());
    Some(match key {
        "hour" => i64::from(time.hour()),
        "minute" => i64::from(time.minute()),
        "second" => i64::from(time.second()),
        "millisecond" => fraction / 1_000_000,
        "microsecond" => fraction / 1_000,
        "nanosecond" => fraction,
        _ => return None,
    })
}

fn offset_field(offset_seconds: i32, key: &str) -> Option<CanonicalScalar> {
    match key {
        "timezone" | "offset" => {
            CanonicalScalar::ucs_basic_text(&format_utc_offset(offset_seconds)).ok()
        }
        "offsetMinutes" => Some(CanonicalScalar::Int(i64::from(offset_seconds / 60))),
        "offsetSeconds" => Some(CanonicalScalar::Int(i64::from(offset_seconds))),
        _ => None,
    }
}
