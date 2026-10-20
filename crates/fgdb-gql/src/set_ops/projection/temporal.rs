//! Temporal values in the value language (fgdb-txbg9): the openCypher
//! constructors and field accessors. Pure arithmetic over the scalar domain:
//! no clock, no locale, and no zone database except a pinned tzdb artifact
//! the statement carries ([`GqlTimeZones`]). The default offset is UTC.

use super::GraphTemporalFunction;
use crate::GraphIntegerErrorKind;
use crate::GraphIntegerErrorKind::TemporalConstruction as Invalid;
use crate::algebra::GraphValue;
use fgdb_types::{
    CanonicalDate, CanonicalDuration, CanonicalLocalDateTime, CanonicalLocalTime, CanonicalScalar,
    CanonicalTime, CanonicalTimestamp, DurationUnit, NANOS_PER_DAY, ObjectId, TzdbResolver,
    format_utc_offset, parse_utc_offset,
};
use std::sync::Arc;

/// A pinned tzdb capability for named time zones (`timezone:
/// 'Europe/Stockholm'`). A named zone resolves only through this exact
/// artifact, never host state or a clock, and a plan that carries it records
/// its identity: the same artifact gives the same result (doctrine #4).
#[derive(Clone)]
pub struct GqlTimeZones {
    resolver: Arc<dyn TzdbResolver + Send + Sync>,
    tzdb: ObjectId,
}

impl GqlTimeZones {
    /// `None` unless `resolver` holds the exact artifact `tzdb`.
    pub fn new(resolver: Arc<dyn TzdbResolver + Send + Sync>, tzdb: ObjectId) -> Option<Self> {
        resolver
            .contains_tzdb(&tzdb)
            .then_some(Self { resolver, tzdb })
    }

    /// The content address of the artifact, as plans record it.
    #[must_use]
    pub fn tzdb(&self) -> ObjectId {
        self.tzdb
    }

    fn offset_at(&self, zone: &str, instant: i128) -> Option<i32> {
        self.resolver
            .canonical_utc_offset_seconds(&self.tzdb, zone, instant)
    }

    /// The instant and offset of local wall time `wall` (nanoseconds, as if
    /// at UTC) in `zone`, by java.time's rules, which openCypher follows: in
    /// an overlap the earlier offset, and in a gap the wall time moves later
    /// by the gap's length. Transitions are taken to be over a day apart.
    fn local(&self, zone: &str, wall: i128) -> Option<(i128, i32)> {
        const DAY: i128 = 86_400 * NANOS_PER_SECOND;
        let before = self.offset_at(zone, wall - DAY)?;
        let after = self.offset_at(zone, wall + DAY)?;
        let at = |offset: i32| wall - i128::from(offset) * NANOS_PER_SECOND;
        for offset in [before, after] {
            if self.offset_at(zone, at(offset))? == offset {
                return Some((at(offset), offset));
            }
        }
        let instant = at(before);
        Some((instant, self.offset_at(zone, instant)?))
    }

    fn timestamp(
        &self,
        zone: &str,
        instant: i128,
        offset: i32,
    ) -> Result<CanonicalTimestamp, GraphIntegerErrorKind> {
        CanonicalTimestamp::zoned(instant, offset, zone, self.tzdb, &*self.resolver)
            .map_err(|_| Invalid)
    }
}

impl PartialEq for GqlTimeZones {
    fn eq(&self, other: &Self) -> bool {
        self.tzdb == other.tzdb
    }
}
impl Eq for GqlTimeZones {}

impl core::fmt::Debug for GqlTimeZones {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GqlTimeZones")
            .field("tzdb", &self.tzdb)
            .finish_non_exhaustive()
    }
}

/// Where a value is placed in time: a fixed UTC offset, or a named zone of
/// the statement's pinned tzdb.
#[derive(Clone, Copy)]
enum Zone<'a> {
    Offset(i32),
    Named(&'a str),
}

/// A `timezone` value: an offset (`+01:00`, `Z`), or a zone the pinned tzdb
/// names. Without a tzdb, or for a name it lacks, a name refuses.
fn zone_spec<'a>(
    text: &'a str,
    zones: Option<&GqlTimeZones>,
) -> Result<Zone<'a>, GraphIntegerErrorKind> {
    if let Ok(offset) = parse_utc_offset(text) {
        return Ok(Zone::Offset(offset));
    }
    match zones {
        Some(zones) if zones.offset_at(text, 0).is_some() => Ok(Zone::Named(text)),
        _ => Err(Invalid),
    }
}

/// `local` placed in `zone`: at a fixed offset, or resolved in a named zone.
fn in_zone(
    local: CanonicalLocalDateTime,
    zone: Zone<'_>,
    zones: Option<&GqlTimeZones>,
) -> Result<CanonicalTimestamp, GraphIntegerErrorKind> {
    match zone {
        Zone::Offset(offset) => at_offset(local, offset),
        Zone::Named(name) => {
            let zones = zones.ok_or(Invalid)?;
            let (instant, offset) = zones.local(name, wall_nanos(local)).ok_or(Invalid)?;
            zones.timestamp(name, instant, offset)
        }
    }
}

/// The UTC instant `instant` seen in `zone`.
fn at_instant(
    instant: i128,
    zone: Zone<'_>,
    zones: Option<&GqlTimeZones>,
) -> Result<CanonicalTimestamp, GraphIntegerErrorKind> {
    match zone {
        Zone::Offset(offset) => {
            CanonicalTimestamp::offset_only(instant, offset).map_err(|_| Invalid)
        }
        Zone::Named(name) => {
            let zones = zones.ok_or(Invalid)?;
            let offset = zones.offset_at(name, instant).ok_or(Invalid)?;
            zones.timestamp(name, instant, offset)
        }
    }
}

/// The zone a timestamp is in: its named zone when the statement carries a
/// matching tzdb, else its offset for offset-only timestamps. A zoned timestamp
/// whose named zone is required by a temporal rule refuses without its tzdb.
fn timestamp_zone<'v>(
    value: &'v CanonicalTimestamp,
    zones: Option<&GqlTimeZones>,
) -> Result<Zone<'v>, GraphIntegerErrorKind> {
    match value.zone() {
        Some(zone) => {
            if zones.is_some_and(|zones| zones.tzdb == zone.tzdb_oid()) {
                Ok(Zone::Named(zone.identifier()))
            } else {
                Err(Invalid)
            }
        }
        None => Ok(Zone::Offset(value.utc_offset_seconds())),
    }
}

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
            // Only the constructors take a map of fields.
            _ => return Err(Invalid),
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
    zones: Option<&GqlTimeZones>,
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
        GraphTemporalFunction::DateTime => {
            datetime(argument, zones).map(CanonicalScalar::Timestamp)
        }
        GraphTemporalFunction::TruncateDate
        | GraphTemporalFunction::TruncateLocalTime
        | GraphTemporalFunction::TruncateTime
        | GraphTemporalFunction::TruncateLocalDateTime
        | GraphTemporalFunction::TruncateDateTime => {
            truncate(function, arguments(argument)?, zones)
        }
        GraphTemporalFunction::FromEpoch | GraphTemporalFunction::FromEpochMillis => {
            from_epoch_call(function, arguments(argument)?)
        }
        GraphTemporalFunction::Duration => duration(argument).map(CanonicalScalar::Duration),
        GraphTemporalFunction::Between
        | GraphTemporalFunction::InMonths
        | GraphTemporalFunction::InDays
        | GraphTemporalFunction::InSeconds => between(function, arguments(argument)?, zones),
        // A clock read with a timezone argument needs a clock capability
        // the query context does not grant (NULL returned above).
        GraphTemporalFunction::Clock => Err(GraphIntegerErrorKind::IncompatibleOperands),
    }
}

/// A temporal value as a point: its date if it has one, its local time of
/// day (midnight for a date), and its offset if it has one.
struct Moment {
    date: Option<CanonicalDate>,
    time: CanonicalLocalTime,
    offset: Option<i32>,
}

fn moment(value: &GraphValue) -> Result<Moment, GraphIntegerErrorKind> {
    let (date, time, offset) = match value {
        GraphValue::Scalar(CanonicalScalar::Date(day)) => {
            (Some(*day), CanonicalLocalTime::MIDNIGHT, None)
        }
        GraphValue::Scalar(CanonicalScalar::LocalTime(time)) => (None, *time, None),
        GraphValue::Scalar(CanonicalScalar::Time(time)) => {
            (None, time.local(), Some(time.offset_seconds()))
        }
        GraphValue::Scalar(CanonicalScalar::LocalDateTime(value)) => {
            (Some(value.date()), value.time(), None)
        }
        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => {
            let wall = wall_clock(value)?;
            (
                Some(wall.date()),
                wall.time(),
                Some(value.utc_offset_seconds()),
            )
        }
        _ => return Err(Invalid),
    };
    Ok(Moment { date, time, offset })
}

/// `duration.between(a, b)` and its `inMonths`, `inDays` and `inSeconds`
/// projections; NULL for a NULL side. When both sides have an offset, `b`
/// is read at `a`'s offset, the same instant. When both have a date,
/// `between` counts the whole months from `a` that do not pass `b` (a
/// month added to the 31st stops at the month's last day), then the whole
/// days, then the time left, all of one sign; `inMonths` is those months,
/// `inDays` the whole days of the whole span and `inSeconds` all of it as
/// time. When either lacks a date, only the times of day differ (a date's
/// is midnight): `between` and `inSeconds` are that difference, and
/// `inMonths` and `inDays` are zero. A side without an offset is read in
/// the other side's named zone at its own wall time, on that side's date
/// when it has none, so a span across a daylight saving change counts the
/// hour the change adds or removes (TCK Temporal10 [8]).
fn between(
    function: GraphTemporalFunction,
    items: &[GraphValue],
    zones: Option<&GqlTimeZones>,
) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
    let [left, right] = items else {
        return Err(Invalid);
    };
    if [left, right]
        .iter()
        .any(|value| matches!(value, GraphValue::Scalar(CanonicalScalar::Null)))
    {
        return Ok(CanonicalScalar::Null);
    }
    let (mut from, mut to) = (moment(left)?, moment(right)?);
    if to.offset.is_none()
        && let Some(zone) = named_zone(left, zones)?
    {
        let date = to.date.or(from.date).ok_or(Invalid)?;
        to.offset = Some(offset_in_zone(zone, date, to.time, zones)?);
    }
    if from.offset.is_none()
        && let Some(zone) = named_zone(right, zones)?
    {
        let date = from.date.or(to.date).ok_or(Invalid)?;
        from.offset = Some(offset_in_zone(zone, date, from.time, zones)?);
    }
    let second = NANOS_PER_SECOND;
    let shift = match (from.offset, to.offset) {
        (Some(from), Some(to)) => i128::from(from) - i128::from(to),
        _ => 0,
    };
    let (months, days, nanos) = match (from.date, to.date) {
        (Some(from_date), Some(to_date)) => {
            let start = point(from_date, from.time);
            let end = point(to_date, to.time) + shift * second;
            let span = |months: i64| -> Result<(i64, i64, i128), GraphIntegerErrorKind> {
                let day = i128::from(NANOS_PER_DAY);
                let after_months = add_months(start, months)?;
                let mut days = i64::try_from((end - after_months) / day).map_err(|_| Invalid)?;
                // Days that overshoot `end` once the time of day counts.
                let landed = after_months + i128::from(days) * day;
                if end >= start && landed > end {
                    days -= 1;
                } else if end < start && landed < end {
                    days += 1;
                }
                let rest = end - (after_months + i128::from(days) * day);
                Ok((months, days, rest))
            };
            match function {
                GraphTemporalFunction::InDays => {
                    let days = i64::try_from((end - start) / i128::from(NANOS_PER_DAY))
                        .map_err(|_| Invalid)?;
                    (0, days, 0)
                }
                GraphTemporalFunction::InSeconds => (0, 0, end - start),
                _ => {
                    let months = whole_months(start, end)?;
                    let (months, days, rest) = span(months)?;
                    if function == GraphTemporalFunction::InMonths {
                        (months, 0, 0)
                    } else {
                        (months, days, rest)
                    }
                }
            }
        }
        _ => match function {
            GraphTemporalFunction::Between | GraphTemporalFunction::InSeconds => {
                let start = i128::from(from.time.nanos_of_day());
                let end = i128::from(to.time.nanos_of_day()) + shift * second;
                (0, 0, end - start)
            }
            _ => (0, 0, 0),
        },
    };
    CanonicalDuration::new(months, days, nanos)
        .map(CanonicalScalar::Duration)
        .map_err(|_| Invalid)
}

/// The named zone a timestamp carries, when the statement's tzdb has it.
fn named_zone<'v>(
    value: &'v GraphValue,
    zones: Option<&GqlTimeZones>,
) -> Result<Option<&'v str>, GraphIntegerErrorKind> {
    match value {
        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => {
            match timestamp_zone(value, zones)? {
                Zone::Named(name) => Ok(Some(name)),
                Zone::Offset(_) => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// The offset `zone` has at the wall time `date` `time`.
fn offset_in_zone(
    zone: &str,
    date: CanonicalDate,
    time: CanonicalLocalTime,
    zones: Option<&GqlTimeZones>,
) -> Result<i32, GraphIntegerErrorKind> {
    let (_, offset) = zones
        .ok_or(Invalid)?
        .local(zone, point(date, time))
        .ok_or(Invalid)?;
    Ok(offset)
}

/// A local date and time as nanoseconds since 1970-01-01T00:00.
fn point(date: CanonicalDate, time: CanonicalLocalTime) -> i128 {
    i128::from(date.epoch_day()) * i128::from(NANOS_PER_DAY) + i128::from(time.nanos_of_day())
}

/// `point` plus `months` calendar months, its day clamped to the month.
fn add_months(point: i128, months: i64) -> Result<i128, GraphIntegerErrorKind> {
    let day = i128::from(NANOS_PER_DAY);
    let epoch_day = i64::try_from(point.div_euclid(day)).map_err(|_| Invalid)?;
    let time = point.rem_euclid(day);
    let (year, month, of_month) = CanonicalDate::from_epoch_day(epoch_day)
        .map_err(|_| Invalid)?
        .civil();
    let total = i128::from(year) * 12 + i128::from(month - 1) + i128::from(months);
    let year = i64::try_from(total.div_euclid(12)).map_err(|_| Invalid)?;
    let month = u8::try_from(total.rem_euclid(12) + 1).map_err(|_| Invalid)?;
    if !fgdb_types::temporal::calendar::year_in_range(year) {
        return Err(Invalid);
    }
    let of_month = of_month.min(fgdb_types::temporal::calendar::days_in_month(year, month));
    let date = CanonicalDate::from_civil(year, month, of_month).map_err(|_| Invalid)?;
    Ok(i128::from(date.epoch_day()) * day + time)
}

/// Whether `value` is a temporal value, a duration among them.
fn is_temporal(value: &CanonicalScalar) -> bool {
    matches!(
        value,
        CanonicalScalar::Date(_)
            | CanonicalScalar::LocalTime(_)
            | CanonicalScalar::Time(_)
            | CanonicalScalar::LocalDateTime(_)
            | CanonicalScalar::Timestamp(_)
            | CanonicalScalar::Duration(_)
    )
}

/// openCypher arithmetic on temporal values (TCK Temporal8): a temporal
/// value plus or minus a duration, a duration plus a temporal value,
/// durations added and subtracted component by component, and a duration
/// times or divided by an Int or Float. `None` when neither operand is
/// temporal, so ordinary arithmetic runs; any other combination with a
/// temporal operand is IncompatibleOperands, and a NULL operand is NULL.
pub(crate) fn arithmetic(
    op: crate::GraphIntegerBinary,
    left: &CanonicalScalar,
    right: &CanonicalScalar,
) -> Result<Option<CanonicalScalar>, GraphIntegerErrorKind> {
    use crate::GraphIntegerBinary::{Add, Divide, Multiply, Subtract};
    use CanonicalScalar::{Duration, Null};
    if !is_temporal(left) && !is_temporal(right) {
        return Ok(None);
    }
    if matches!(left, Null) || matches!(right, Null) {
        return Ok(Some(Null));
    }
    let overflow = |_| GraphIntegerErrorKind::Overflow;
    Ok(Some(match (op, left, right) {
        (Add, Duration(a), Duration(b)) => Duration(a.checked_add(*b).map_err(overflow)?),
        (Subtract, Duration(a), Duration(b)) => Duration(
            a.checked_add(b.checked_neg().map_err(overflow)?)
                .map_err(overflow)?,
        ),
        (Add, value, Duration(duration)) | (Add, Duration(duration), value) => {
            plus(value, *duration)?
        }
        (Subtract, value, Duration(duration)) => {
            plus(value, duration.checked_neg().map_err(overflow)?)?
        }
        (Multiply, Duration(duration), factor) | (Multiply, factor, Duration(duration)) => {
            let (numerator, denominator) = ratio(factor)?;
            Duration(
                duration
                    .checked_scale(numerator, denominator)
                    .map_err(overflow)?,
            )
        }
        (Divide, Duration(duration), divisor) => {
            // Dividing by p / q multiplies by q / p, the sign moved up.
            let (numerator, denominator) = ratio(divisor)?;
            if numerator == 0 {
                return Err(GraphIntegerErrorKind::DivisionByZero);
            }
            let (numerator, denominator) = if numerator < 0 {
                (-denominator, -numerator)
            } else {
                (denominator, numerator)
            };
            Duration(
                duration
                    .checked_scale(numerator, denominator)
                    .map_err(overflow)?,
            )
        }
        _ => return Err(GraphIntegerErrorKind::IncompatibleOperands),
    }))
}

/// A number as an exact ratio with a positive denominator. A finite
/// binary64 is a binary fraction, doubled until whole (each doubling is
/// exact); one that needs more than 62 fraction bits, or is not finite,
/// overflows rather than round.
fn ratio(value: &CanonicalScalar) -> Result<(i128, i128), GraphIntegerErrorKind> {
    match value {
        CanonicalScalar::Int(value) => Ok((i128::from(*value), 1)),
        CanonicalScalar::Float(value) => {
            let mut scaled = value.get();
            if !scaled.is_finite() {
                return Err(GraphIntegerErrorKind::Overflow);
            }
            let mut denominator: i128 = 1;
            while scaled.fract() != 0.0 {
                if denominator >= 1 << 62 {
                    return Err(GraphIntegerErrorKind::Overflow);
                }
                scaled *= 2.0;
                denominator *= 2;
            }
            if scaled.abs() >= 2_f64.powi(100) {
                return Err(GraphIntegerErrorKind::Overflow);
            }
            Ok((scaled as i128, denominator))
        }
        _ => Err(GraphIntegerErrorKind::IncompatibleOperands),
    }
}

/// `value` moved by `duration`: calendar months first (the day clamped to
/// the month), then days, then the time part. A date keeps only the whole
/// days of the time part and a time of day only the time part, around the
/// clock; a datetime keeps its offset. A timestamp in a named zone would
/// need the zone database, so it refuses as incompatible.
fn plus(
    value: &CanonicalScalar,
    duration: CanonicalDuration,
) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
    let day = i128::from(NANOS_PER_DAY);
    let calendar = |start: i128, keep_time: bool| -> Result<i128, GraphIntegerErrorKind> {
        let mut days = i128::from(duration.days());
        if !keep_time {
            // Truncated toward zero: a date minus 16 hours is the same date.
            days += duration.nanos() / day;
        }
        let moved = add_months(start, duration.months())?
            .checked_add(days.checked_mul(day).ok_or(Invalid)?)
            .ok_or(Invalid)?;
        if keep_time {
            moved.checked_add(duration.nanos()).ok_or(Invalid)
        } else {
            Ok(moved)
        }
    };
    let local = |point: i128| -> Result<CanonicalLocalDateTime, GraphIntegerErrorKind> {
        let epoch_day = i64::try_from(point.div_euclid(day)).map_err(|_| Invalid)?;
        let nanos = u64::try_from(point.rem_euclid(day)).map_err(|_| Invalid)?;
        Ok(CanonicalLocalDateTime::new(
            CanonicalDate::from_epoch_day(epoch_day).map_err(|_| Invalid)?,
            CanonicalLocalTime::from_nanos_of_day(nanos).map_err(|_| Invalid)?,
        ))
    };
    let clock = |time: CanonicalLocalTime| -> Result<CanonicalLocalTime, GraphIntegerErrorKind> {
        let nanos = (i128::from(time.nanos_of_day()) + duration.nanos()).rem_euclid(day);
        CanonicalLocalTime::from_nanos_of_day(u64::try_from(nanos).map_err(|_| Invalid)?)
            .map_err(|_| Invalid)
    };
    Ok(match value {
        CanonicalScalar::Date(date) => CanonicalScalar::Date(
            local(calendar(point(*date, CanonicalLocalTime::MIDNIGHT), false)?)?.date(),
        ),
        CanonicalScalar::LocalTime(time) => CanonicalScalar::LocalTime(clock(*time)?),
        CanonicalScalar::Time(time) => CanonicalScalar::Time(
            CanonicalTime::new(clock(time.local())?, time.offset_seconds()).map_err(|_| Invalid)?,
        ),
        CanonicalScalar::LocalDateTime(value) => CanonicalScalar::LocalDateTime(local(calendar(
            point(value.date(), value.time()),
            true,
        )?)?),
        CanonicalScalar::Timestamp(value) if value.zone().is_none() => {
            let wall = wall_clock(value)?;
            let moved = local(calendar(point(wall.date(), wall.time()), true)?)?;
            CanonicalScalar::Timestamp(at_offset(moved, value.utc_offset_seconds())?)
        }
        _ => return Err(GraphIntegerErrorKind::IncompatibleOperands),
    })
}

/// The whole calendar months from `start` that do not pass `end`, toward it.
fn whole_months(start: i128, end: i128) -> Result<i64, GraphIntegerErrorKind> {
    let day = i128::from(NANOS_PER_DAY);
    let civil = |point: i128| {
        CanonicalDate::from_epoch_day(i64::try_from(point.div_euclid(day)).map_err(|_| Invalid)?)
            .map(CanonicalDate::civil)
            .map_err(|_| Invalid)
    };
    let ((start_year, start_month, _), (end_year, end_month, _)) = (civil(start)?, civil(end)?);
    let mut months = i64::try_from(
        (i128::from(end_year) - i128::from(start_year)) * 12 + i128::from(end_month)
            - i128::from(start_month),
    )
    .map_err(|_| Invalid)?;
    let landed = add_months(start, months)?;
    if end >= start && landed > end {
        months -= 1;
    } else if end < start && landed < end {
        months += 1;
    }
    Ok(months)
}

/// `duration(x)`: ISO 8601 text, a duration, or a map of counts of years,
/// quarters, months, weeks, days, hours, minutes, seconds, milliseconds,
/// microseconds and nanoseconds, each an integer or a float whose fraction
/// cascades into the smaller units (`{months: 0.75}` is P22DT19H51M49.5S).
fn duration(argument: &GraphValue) -> Result<CanonicalDuration, GraphIntegerErrorKind> {
    use DurationUnit as U;
    match argument {
        GraphValue::Scalar(CanonicalScalar::Text(text)) => {
            CanonicalDuration::parse_iso(text.as_str()).map_err(|_| Invalid)
        }
        GraphValue::Scalar(CanonicalScalar::Duration(value)) => Ok(*value),
        GraphValue::Map { keys, values } => {
            let mut parts = Vec::with_capacity(keys.len());
            for (key, value) in keys.iter().zip(values.iter()) {
                let (unit, factor) = match &**key {
                    "years" => (U::Years, 1),
                    "quarters" => (U::Months, 3),
                    "months" => (U::Months, 1),
                    "weeks" => (U::Weeks, 1),
                    "days" => (U::Days, 1),
                    "hours" => (U::Hours, 1),
                    "minutes" => (U::Minutes, 1),
                    "seconds" => (U::Seconds, 1),
                    "milliseconds" => (U::Milliseconds, 1),
                    "microseconds" => (U::Microseconds, 1),
                    "nanoseconds" => (U::Nanoseconds, 1),
                    _ => return Err(Invalid),
                };
                let (whole, fraction) = count(value)?;
                // Three months a quarter: whole months of a quarter's
                // fraction stay calendar months (truncating keeps the sign).
                let scaled = fraction * factor;
                parts.push((
                    unit,
                    whole * factor + scaled / 1_000_000_000,
                    scaled % 1_000_000_000,
                ));
            }
            CanonicalDuration::from_parts(parts).map_err(|_| Invalid)
        }
        _ => Err(Invalid),
    }
}

/// A count as a whole number and a fraction in 10^-9 of the same sign: an
/// integer exactly, a finite float to the nearest 10^-9.
fn count(value: &GraphValue) -> Result<(i128, i128), GraphIntegerErrorKind> {
    match value {
        GraphValue::Scalar(CanonicalScalar::Int(number)) => Ok((i128::from(*number), 0)),
        GraphValue::Scalar(CanonicalScalar::Float(number)) => {
            let number = number.get();
            if !number.is_finite() || number.abs() >= 9.0e18 {
                return Err(Invalid);
            }
            let whole = number.trunc();
            let fraction = ((number - whole) * 1e9).round();
            // The casts are exact: both are integral and within i64.
            Ok((whole as i128, fraction as i128))
        }
        _ => Err(Invalid),
    }
}

/// A duration's openCypher fields: the whole count in each unit, and the
/// count of a unit within the next larger one (`monthsOfYear`,
/// `secondsOfMinute`, ...). As openCypher splits it, the time part is whole
/// seconds floored and a fraction of 0-999999999 nanoseconds (PT-1.9S is
/// -2 seconds and 100000000 nanosecondsOfSecond); every other count is
/// truncated toward zero.
fn duration_field(value: CanonicalDuration, key: &str) -> Option<i64> {
    let (months, days, nanos) = (value.months(), value.days(), value.nanos());
    let seconds = i64::try_from(nanos.div_euclid(NANOS_PER_SECOND)).ok()?;
    let fraction = i64::try_from(nanos.rem_euclid(NANOS_PER_SECOND)).ok()?;
    let finer = |per_second: i64| {
        seconds
            .checked_mul(per_second)?
            .checked_add(fraction / (1_000_000_000 / per_second))
    };
    match key {
        "years" => Some(months / 12),
        "quarters" => Some(months / 3),
        "months" => Some(months),
        "weeks" => Some(days / 7),
        "days" => Some(days),
        "hours" => Some(seconds / 3_600),
        "minutes" => Some(seconds / 60),
        "seconds" => Some(seconds),
        "milliseconds" => finer(1_000),
        "microseconds" => finer(1_000_000),
        "nanoseconds" => finer(1_000_000_000),
        "quartersOfYear" => Some(months % 12 / 3),
        "monthsOfQuarter" => Some(months % 3),
        "monthsOfYear" => Some(months % 12),
        "daysOfWeek" => Some(days % 7),
        "minutesOfHour" => Some(seconds / 60 % 60),
        "secondsOfMinute" => Some(seconds % 60),
        "millisecondsOfSecond" => Some(fraction / 1_000_000),
        "microsecondsOfSecond" => Some(fraction / 1_000),
        "nanosecondsOfSecond" => Some(fraction),
        _ => None,
    }
}

/// A dotted call's evaluated argument list.
fn arguments(argument: &GraphValue) -> Result<&[GraphValue], GraphIntegerErrorKind> {
    match argument {
        GraphValue::List(items) => Ok(&items[..]),
        _ => Err(Invalid),
    }
}

/// A truncation unit, from the millennium down to the microsecond.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Unit {
    Millennium,
    Century,
    Decade,
    Year,
    WeekYear,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
}

impl Unit {
    fn named(name: &str) -> Option<Self> {
        [
            ("millennium", Self::Millennium),
            ("century", Self::Century),
            ("decade", Self::Decade),
            ("year", Self::Year),
            ("weekYear", Self::WeekYear),
            ("quarter", Self::Quarter),
            ("month", Self::Month),
            ("week", Self::Week),
            ("day", Self::Day),
            ("hour", Self::Hour),
            ("minute", Self::Minute),
            ("second", Self::Second),
            ("millisecond", Self::Millisecond),
            ("microsecond", Self::Microsecond),
        ]
        .into_iter()
        .find_map(|(spelling, unit)| name.eq_ignore_ascii_case(spelling).then_some(unit))
    }
}

/// `kind.truncate(unit, value[, map])`: the value cut down to the start of
/// its `unit`, then the map's fields set over it as over a base. A date
/// truncates to a day or coarser, a time to a day or finer; a date-time to
/// any unit, a time-of-day unit keeping its date. NULL for a NULL unit or
/// value. A map's `timezone` sets the offset without moving the clock, unlike
/// a constructor's. As openCypher does, a sub-millisecond field after a
/// millisecond or microsecond truncation keeps the truncated digits:
/// millisecond truncation then `{nanosecond: 2}` is `.645000002`.
fn truncate(
    function: GraphTemporalFunction,
    items: &[GraphValue],
    zones: Option<&GqlTimeZones>,
) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
    use GraphTemporalFunction as F;
    let (unit, value, map) = match items {
        [unit, value] => (unit, value, None),
        [unit, value, map] => (unit, value, Some(map)),
        _ => return Err(Invalid),
    };
    if [unit, value]
        .iter()
        .any(|value| matches!(value, GraphValue::Scalar(CanonicalScalar::Null)))
    {
        return Ok(CanonicalScalar::Null);
    }
    let GraphValue::Scalar(CanonicalScalar::Text(unit)) = unit else {
        return Err(Invalid);
    };
    let unit = Unit::named(unit.as_str()).ok_or(Invalid)?;
    let mut arguments = match map {
        None | Some(GraphValue::Scalar(CanonicalScalar::Null)) => MapArguments::default(),
        Some(GraphValue::Map { keys, values }) => {
            let kind = match function {
                F::TruncateDate => F::Date,
                F::TruncateLocalTime => F::LocalTime,
                F::TruncateTime => F::Time,
                F::TruncateLocalDateTime => F::LocalDateTime,
                _ => F::DateTime,
            };
            MapArguments::split(kind, keys, values)?
        }
        Some(_) => return Err(Invalid),
    };
    if arguments.date_base.is_some()
        || arguments.time_base.is_some()
        || arguments.datetime_base.is_some()
        || arguments.epoch_seconds.is_some()
        || arguments.epoch_millis.is_some()
    {
        return Err(Invalid);
    }
    let zone = arguments
        .timezone
        .map(|zone| zone_spec(zone, zones))
        .transpose()?;
    Ok(match function {
        F::TruncateDate => {
            let date = truncate_date(date_part(value)?, unit)?;
            CanonicalScalar::Date(date_from_fields(arguments.date, Some(date))?)
        }
        F::TruncateLocalTime => {
            let time = truncate_time(time_part(value)?.0, unit)?;
            keep_subseconds(&mut arguments.time, time, unit);
            CanonicalScalar::LocalTime(time_from_fields(arguments.time, Some(time), false)?)
        }
        F::TruncateTime => {
            let (time, offset) = time_part(value)?;
            let time = truncate_time(time, unit)?;
            keep_subseconds(&mut arguments.time, time, unit);
            let time = time_from_fields(arguments.time, Some(time), false)?;
            // A time has an offset, never a named zone.
            let zone = match zone {
                Some(Zone::Offset(offset)) => Some(offset),
                Some(Zone::Named(_)) => return Err(Invalid),
                None => None,
            };
            let offset = zone.or(offset).unwrap_or(0);
            CanonicalScalar::Time(CanonicalTime::new(time, offset).map_err(|_| Invalid)?)
        }
        _ => {
            let (local, base) = match value {
                GraphValue::Scalar(CanonicalScalar::Date(day)) => (
                    CanonicalLocalDateTime::new(*day, CanonicalLocalTime::MIDNIGHT),
                    None,
                ),
                other => zoned_part(other, zones)?,
            };
            let (date, time) = if unit <= Unit::Day {
                (
                    truncate_date(local.date(), unit)?,
                    CanonicalLocalTime::MIDNIGHT,
                )
            } else {
                (local.date(), truncate_time(local.time(), unit)?)
            };
            keep_subseconds(&mut arguments.time, time, unit);
            let local = CanonicalLocalDateTime::new(
                date_from_fields(arguments.date, Some(date))?,
                time_from_fields(arguments.time, Some(time), true)?,
            );
            if function == F::TruncateLocalDateTime {
                CanonicalScalar::LocalDateTime(local)
            } else {
                let zone = zone.or(base).unwrap_or(Zone::Offset(0));
                CanonicalScalar::Timestamp(in_zone(local, zone, zones)?)
            }
        }
    })
}

/// `date` at the start of its `unit`, which must be a day or coarser.
fn truncate_date(date: CanonicalDate, unit: Unit) -> Result<CanonicalDate, GraphIntegerErrorKind> {
    let (year, month, _) = date.civil();
    let floor = |step: i64| year.div_euclid(step) * step;
    let truncated = match unit {
        Unit::Millennium => CanonicalDate::from_civil(floor(1_000), 1, 1),
        Unit::Century => CanonicalDate::from_civil(floor(100), 1, 1),
        Unit::Decade => CanonicalDate::from_civil(floor(10), 1, 1),
        Unit::Year => CanonicalDate::from_civil(year, 1, 1),
        Unit::WeekYear => CanonicalDate::from_iso_week(date.iso_week().0, 1, 1),
        Unit::Quarter => CanonicalDate::from_civil(year, (month - 1) / 3 * 3 + 1, 1),
        Unit::Month => CanonicalDate::from_civil(year, month, 1),
        Unit::Week => {
            CanonicalDate::from_epoch_day(date.epoch_day() - i64::from(date.weekday() - 1))
        }
        Unit::Day => Ok(date),
        _ => return Err(Invalid),
    };
    truncated.map_err(|_| Invalid)
}

/// `time` at the start of its `unit`, which must be a day or finer.
fn truncate_time(
    time: CanonicalLocalTime,
    unit: Unit,
) -> Result<CanonicalLocalTime, GraphIntegerErrorKind> {
    const NANOS: u64 = 1_000_000_000;
    let step = match unit {
        Unit::Day => NANOS_PER_DAY,
        Unit::Hour => 3_600 * NANOS,
        Unit::Minute => 60 * NANOS,
        Unit::Second => NANOS,
        Unit::Millisecond => 1_000_000,
        Unit::Microsecond => 1_000,
        _ => return Err(Invalid),
    };
    let nanos = time.nanos_of_day();
    CanonicalLocalTime::from_nanos_of_day(nanos - nanos % step).map_err(|_| Invalid)
}

/// After a millisecond (microsecond) truncation, a finer fraction field
/// without its millisecond (microsecond) keeps the truncated digits.
fn keep_subseconds(fields: &mut [Option<i64>; 6], truncated: CanonicalLocalTime, unit: Unit) {
    let [.., millisecond, microsecond, nanosecond] = fields;
    let fraction = i64::from(truncated.nanosecond());
    if unit == Unit::Millisecond
        && millisecond.is_none()
        && (microsecond.is_some() || nanosecond.is_some())
    {
        *millisecond = Some(fraction / 1_000_000);
    }
    if unit == Unit::Microsecond
        && millisecond.is_none()
        && microsecond.is_none()
        && nanosecond.is_some()
    {
        *microsecond = Some(fraction / 1_000);
    }
}

/// `datetime.fromepoch(seconds, nanoseconds)` and
/// `datetime.fromepochmillis(milliseconds)`, at UTC; NULL for a NULL
/// argument.
fn from_epoch_call(
    function: GraphTemporalFunction,
    items: &[GraphValue],
) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
    if items
        .iter()
        .any(|item| matches!(item, GraphValue::Scalar(CanonicalScalar::Null)))
    {
        return Ok(CanonicalScalar::Null);
    }
    let instant = match (function, items) {
        (GraphTemporalFunction::FromEpoch, [seconds, nanoseconds]) => {
            let nanoseconds = integer(nanoseconds)?;
            if !(0..1_000_000_000).contains(&nanoseconds) {
                return Err(Invalid);
            }
            i128::from(integer(seconds)?) * NANOS_PER_SECOND + i128::from(nanoseconds)
        }
        (GraphTemporalFunction::FromEpochMillis, [millis]) => {
            i128::from(integer(millis)?) * 1_000_000
        }
        _ => return Err(Invalid),
    };
    CanonicalTimestamp::offset_only(instant, 0)
        .map(CanonicalScalar::Timestamp)
        .map_err(|_| Invalid)
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

/// A datetime is a timestamp in a zone: a fixed UTC offset, or a named zone
/// of the statement's pinned tzdb (`timezone: 'Europe/Stockholm'`, or a text
/// value's `[Europe/Stockholm]` suffix). Without a `timezone` it is in the
/// zone of its time-bearing base, else at UTC. A `timezone` moves a
/// `datetime` base to the same instant in the new zone, and a `time` base
/// that has an offset to the same time of day there (the date stays); a date
/// base never carries a zone.
fn datetime(
    argument: &GraphValue,
    zones: Option<&GqlTimeZones>,
) -> Result<CanonicalTimestamp, GraphIntegerErrorKind> {
    match argument {
        GraphValue::Scalar(CanonicalScalar::Text(text)) => {
            let text = text.as_str();
            let (text, named) = match text
                .strip_suffix(']')
                .and_then(|rest| rest.rsplit_once('['))
            {
                Some((rest, name)) => (rest, Some(zone_spec(name, zones)?)),
                None => (text, None),
            };
            // The offset starts at the first sign or Z after the T. A date
            // alone is its midnight at UTC (TCK: datetime('-999999999-01-01')).
            let split = match text.find('T') {
                Some(clock) => text[clock..]
                    .find(&['+', '-', 'Z'][..])
                    .map_or(text.len(), |at| clock + at),
                None => text.len(),
            };
            let local = CanonicalLocalDateTime::parse_iso(&text[..split]).map_err(|_| Invalid)?;
            let offset = match &text[split..] {
                "" => None,
                offset => Some(parse_utc_offset(offset).map_err(|_| Invalid)?),
            };
            match (offset, named) {
                // A written offset must be the zone's own at that instant.
                (Some(offset), Some(zone)) => {
                    let instant = at_offset(local, offset)?.instant_utc_nanos();
                    let value = at_instant(instant, zone, zones)?;
                    if value.utc_offset_seconds() != offset {
                        return Err(Invalid);
                    }
                    Ok(value)
                }
                (None, Some(zone)) => in_zone(local, zone, zones),
                (offset, None) => at_offset(local, offset.unwrap_or(0)),
            }
        }
        GraphValue::Map { keys, values } => {
            let arguments = MapArguments::split(GraphTemporalFunction::DateTime, keys, values)?;
            if arguments.epoch_seconds.is_some() || arguments.epoch_millis.is_some() {
                return from_epoch(&arguments, zones);
            }
            let both = arguments
                .datetime_base
                .map(|base| zoned_part(base, zones))
                .transpose()?;
            let date_base = match arguments.date_base {
                Some(base) => Some(date_part(base)?),
                None => both.map(|(value, _)| value.date()),
            };
            let time_base = match arguments.time_base {
                Some(base) => {
                    let (time, offset) = time_part(base)?;
                    // A zoned datetime lends its zone, which resolves at the
                    // new wall time (TCK Temporal3 [9], [10]); any other
                    // time base keeps the offset it was written with.
                    let zone = match base {
                        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => {
                            Some(timestamp_zone(value, zones)?)
                        }
                        _ => offset.map(Zone::Offset),
                    };
                    Some((time, zone))
                }
                None => both.map(|(value, zone)| (value.time(), zone)),
            };
            let date = date_from_fields(arguments.date, date_base)?;
            let local = time_from_fields(arguments.time, time_base.map(|(time, _)| time), true)?;
            let zone = arguments
                .timezone
                .map(|zone| zone_spec(zone, zones))
                .transpose()?;
            let from = time_base.and_then(|(_, zone)| zone);
            let wall = CanonicalLocalDateTime::new(date, local);
            match (from, zone) {
                // A datetime base keeps its instant in the new zone.
                (Some(from), Some(to)) if arguments.time_base.is_none() => {
                    let instant = in_zone(wall, from, zones)?.instant_utc_nanos();
                    at_instant(instant, to, zones)
                }
                // A time base keeps its time of day, read at the new zone's
                // offset for that instant. A zoned base's offset is its
                // zone's at the new wall time.
                (Some(from), Some(to)) => {
                    let from = match from {
                        Zone::Offset(offset) => offset,
                        Zone::Named(_) => in_zone(wall, from, zones)?.utc_offset_seconds(),
                    };
                    let instant = at_offset(wall, from)?.instant_utc_nanos();
                    let to_offset = at_instant(instant, to, zones)?.utc_offset_seconds();
                    let shifted = CanonicalLocalDateTime::new(date, shift(local, to_offset - from));
                    match to {
                        Zone::Offset(offset) => at_offset(shifted, offset),
                        Zone::Named(name) => zones.ok_or(Invalid)?.timestamp(
                            name,
                            wall_nanos(shifted) - i128::from(to_offset) * NANOS_PER_SECOND,
                            to_offset,
                        ),
                    }
                }
                (from, to) => in_zone(wall, to.or(from).unwrap_or(Zone::Offset(0)), zones),
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
fn from_epoch(
    arguments: &MapArguments<'_>,
    zones: Option<&GqlTimeZones>,
) -> Result<CanonicalTimestamp, GraphIntegerErrorKind> {
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
    let zone = arguments
        .timezone
        .map(|zone| zone_spec(zone, zones))
        .transpose()?
        .unwrap_or(Zone::Offset(0));
    at_instant(instant, zone, zones)
}

/// `local` at `offset_seconds` east of UTC, as a timestamp.
fn at_offset(
    local: CanonicalLocalDateTime,
    offset_seconds: i32,
) -> Result<CanonicalTimestamp, GraphIntegerErrorKind> {
    CanonicalTimestamp::offset_only(
        wall_nanos(local) - i128::from(offset_seconds) * NANOS_PER_SECOND,
        offset_seconds,
    )
    .map_err(|_| Invalid)
}

/// `local` as nanoseconds from the epoch, as if at UTC.
fn wall_nanos(local: CanonicalLocalDateTime) -> i128 {
    i128::from(local.date().epoch_day()) * i128::from(NANOS_PER_DAY)
        + i128::from(local.time().nanos_of_day())
}

/// A local date-time base and the zone it is in, if any: a local date-time,
/// or a timestamp's wall time in its zone ([`timestamp_zone`]).
fn zoned_part<'v>(
    value: &'v GraphValue,
    zones: Option<&GqlTimeZones>,
) -> Result<(CanonicalLocalDateTime, Option<Zone<'v>>), GraphIntegerErrorKind> {
    match value {
        GraphValue::Scalar(CanonicalScalar::LocalDateTime(value)) => Ok((*value, None)),
        GraphValue::Scalar(CanonicalScalar::Timestamp(value)) => {
            Ok((wall_clock(value)?, Some(timestamp_zone(value, zones)?)))
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
/// Whether `key` names a field of some temporal value. On a scalar these are
/// the only keys that can answer with a value: any other key is NULL on NULL
/// and an error on every other scalar. The accessors themselves answer, so
/// this set cannot drift from [`field`].
pub(crate) fn is_field_name(key: &str) -> bool {
    duration_field(CanonicalDuration::ZERO, key).is_some()
        || CanonicalDate::from_epoch_day(0).is_ok_and(|day| date_field(day, key).is_some())
        || time_field(CanonicalLocalTime::MIDNIGHT, key).is_some()
        || offset_field(0, key).is_some()
        || CanonicalTimestamp::offset_only(0, 0)
            .is_ok_and(|value| timestamp_field(&value, key).is_some())
}

pub(crate) fn field(
    value: &CanonicalScalar,
    key: &str,
) -> Option<Result<CanonicalScalar, GraphIntegerErrorKind>> {
    if let CanonicalScalar::Duration(value) = value {
        return Some(
            duration_field(*value, key)
                .map(CanonicalScalar::Int)
                .ok_or(GraphIntegerErrorKind::TemporalField),
        );
    }
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
