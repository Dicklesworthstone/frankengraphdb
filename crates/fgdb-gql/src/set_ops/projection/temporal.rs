//! Temporal values in the value language (fgdb-txbg9): the openCypher
//! constructors and field accessors. Pure arithmetic over the scalar domain:
//! no clock, no zone, no locale.

use super::GraphTemporalFunction;
use crate::GraphIntegerErrorKind;
use crate::algebra::GraphValue;
use fgdb_types::{CanonicalDate, CanonicalScalar};

/// The fields a date map may name, in the order `date_from_fields` reads them.
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

/// `function(argument)`: NULL for NULL, a value of the function's kind
/// passes through, text reads as ISO 8601, and a map names one calendar's
/// fields.
pub(crate) fn construct(
    function: GraphTemporalFunction,
    argument: &GraphValue,
) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
    match function {
        GraphTemporalFunction::Date => date(argument),
    }
}

fn date(argument: &GraphValue) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
    use GraphIntegerErrorKind::TemporalConstruction as Invalid;
    match argument {
        GraphValue::Scalar(CanonicalScalar::Null) => Ok(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::Date(day)) => Ok(CanonicalScalar::Date(*day)),
        GraphValue::Scalar(CanonicalScalar::Text(text)) => CanonicalDate::parse_iso(text.as_str())
            .map(CanonicalScalar::Date)
            .map_err(|_| Invalid),
        GraphValue::Map { keys, values } => {
            date_from_fields(keys, values).map(CanonicalScalar::Date)
        }
        _ => Err(Invalid),
    }
}

/// openCypher's date map: the fields of one calendar (year, month and day;
/// ISO week year, week and dayOfWeek; year and ordinalDay; or year, quarter
/// and dayOfQuarter), each an integer, optionally over a `date` base whose
/// own fields in that calendar fill the ones not given. Without a base the
/// year is required and a missing smaller field is its first value. An
/// unknown field, a mix of calendars or a day that does not exist refuses.
fn date_from_fields(
    keys: &[Box<str>],
    values: &[GraphValue],
) -> Result<CanonicalDate, GraphIntegerErrorKind> {
    use GraphIntegerErrorKind::TemporalConstruction as Invalid;
    let mut base = None;
    let mut fields = [None; DATE_FIELDS.len()];
    for (key, value) in keys.iter().zip(values) {
        if &**key == "date" {
            let GraphValue::Scalar(CanonicalScalar::Date(day)) = value else {
                return Err(Invalid);
            };
            base = Some(*day);
            continue;
        }
        let slot = DATE_FIELDS
            .iter()
            .position(|name| *name == &**key)
            .ok_or(Invalid)?;
        let GraphValue::Scalar(CanonicalScalar::Int(number)) = value else {
            return Err(Invalid);
        };
        fields[slot] = Some(*number);
    }
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

/// `day.key` for the openCypher date fields; `None` for any other key.
pub(crate) fn date_field(day: CanonicalDate, key: &str) -> Option<i64> {
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
