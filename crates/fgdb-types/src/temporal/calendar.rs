//! Proleptic Gregorian calendar arithmetic over day numbers (days since
//! 1970-01-01), with ISO 8601 week dates, ordinal dates and quarters
//! (fgdb-txbg9).
//!
//! Years are astronomical: year 0 exists and is 1 BCE. Every computation is
//! pure `i64` arithmetic with Euclidean division, so results never depend on
//! the host clock, timezone or locale. The supported range is the openCypher
//! (java.time) one, years -999,999,999 through 999,999,999. Checked
//! constructors refuse anything outside it, so no intermediate step can
//! overflow.

/// Smallest supported year.
pub const MIN_YEAR: i64 = -999_999_999;
/// Largest supported year.
pub const MAX_YEAR: i64 = 999_999_999;
/// Day number of `-999999999-01-01`, the first supported day.
pub const MIN_EPOCH_DAY: i64 = -365_243_219_162;
/// Day number of `+999999999-12-31`, the last supported day.
pub const MAX_EPOCH_DAY: i64 = 365_241_780_471;

/// Days in one 400-year Gregorian cycle.
const DAYS_PER_ERA: i64 = 146_097;
/// Day number of 0000-03-01, the start of era 0 in the March-based count.
const ERA_BASE_OFFSET: i64 = 719_468;

/// Whether `year` has a February 29.
#[must_use]
pub const fn is_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

/// 365 or 366.
#[must_use]
pub const fn days_in_year(year: i64) -> u16 {
    if is_leap_year(year) { 366 } else { 365 }
}

/// Days in `month` (1-12) of `year`; 0 for a month outside 1-12.
#[must_use]
pub const fn days_in_month(year: i64, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// The day number of a valid in-range civil date. Callers validate first;
/// [`from_civil`] is the checked form.
const fn days_from_civil(year: i64, month: u8, day: u8) -> i64 {
    // Count years from March, so February (and its leap day) ends the year.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = month as i64;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * DAYS_PER_ERA + day_of_era - ERA_BASE_OFFSET
}

/// Whether `year` is a supported year.
#[must_use]
pub fn year_in_range(year: i64) -> bool {
    (MIN_YEAR..=MAX_YEAR).contains(&year)
}

/// The checked day number of `year-month-day`: `None` for a month outside
/// 1-12, a day outside the month, or a year outside the supported range.
#[must_use]
pub fn from_civil(year: i64, month: u8, day: u8) -> Option<i64> {
    (year_in_range(year) && (1..=days_in_month(year, month)).contains(&day))
        .then(|| days_from_civil(year, month, day))
}

/// Whether `day` is a supported day number.
#[must_use]
pub fn in_range(day: i64) -> bool {
    (MIN_EPOCH_DAY..=MAX_EPOCH_DAY).contains(&day)
}

/// The civil `(year, month, day)` of a supported day number; `None` outside
/// the range.
#[must_use]
pub fn to_civil(day: i64) -> Option<(i64, u8, u8)> {
    if !in_range(day) {
        return None;
    }
    let shifted = day + ERA_BASE_OFFSET;
    let era = shifted.div_euclid(DAYS_PER_ERA);
    let day_of_era = shifted.rem_euclid(DAYS_PER_ERA);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day_of_month = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Some((year, month as u8, day_of_month as u8))
}

/// ISO weekday of a day number: 1 is Monday, 7 is Sunday. 1970-01-01 was a
/// Thursday.
#[must_use]
pub const fn iso_weekday(day: i64) -> u8 {
    ((day.rem_euclid(7) + 3) % 7 + 1) as u8
}

/// Day number of the Monday that starts ISO week 1 of `week_year`: the week
/// holding January 4. `week_year` may lie one year outside the civil range.
const fn week_one_monday(week_year: i64) -> i64 {
    let january_fourth = days_from_civil(week_year, 1, 4);
    january_fourth - (iso_weekday(january_fourth) as i64 - 1)
}

/// 52 or 53: the ISO weeks in `week_year`, which must lie within one year of
/// the supported range.
const fn weeks_in_week_year(week_year: i64) -> u8 {
    ((week_one_monday(week_year + 1) - week_one_monday(week_year)) / 7) as u8
}

/// 52 or 53: the ISO weeks in `week_year`; `None` more than one year outside
/// the supported range, where no supported day can fall in that week year.
#[must_use]
pub fn iso_week_count(week_year: i64) -> Option<u8> {
    (MIN_YEAR - 1..=MAX_YEAR + 1)
        .contains(&week_year)
        .then(|| weeks_in_week_year(week_year))
}

/// The ISO week date `(week_year, week, weekday)` of a supported day number.
/// The week year can differ from the civil year near January 1, and lies
/// one year outside the civil range at its two ends.
#[must_use]
pub fn to_iso_week(day: i64) -> Option<(i64, u8, u8)> {
    let (year, _, _) = to_civil(day)?;
    let weekday = iso_weekday(day);
    // The Thursday of this week decides its week year.
    let thursday = day - weekday as i64 + 4;
    let week_year = if thursday < days_from_civil(year, 1, 1) {
        year - 1
    } else if thursday > days_from_civil(year, 12, 31) {
        year + 1
    } else {
        year
    };
    let week = (day - week_one_monday(week_year)) / 7 + 1;
    Some((week_year, week as u8, weekday))
}

/// The checked day number of ISO week date `week_year-Wweek-weekday`: `None`
/// for a week outside the year's 52 or 53, a weekday outside 1-7, or a day
/// outside the supported range.
#[must_use]
pub fn from_iso_week(week_year: i64, week: u8, weekday: u8) -> Option<i64> {
    if !(MIN_YEAR - 1..=MAX_YEAR + 1).contains(&week_year)
        || !(1..=weeks_in_week_year(week_year)).contains(&week)
        || !(1..=7).contains(&weekday)
    {
        return None;
    }
    let day = week_one_monday(week_year) + 7 * (i64::from(week) - 1) + (i64::from(weekday) - 1);
    in_range(day).then_some(day)
}

/// The 1-based day of the civil year of a supported day number.
#[must_use]
pub fn to_ordinal(day: i64) -> Option<(i64, u16)> {
    let (year, _, _) = to_civil(day)?;
    Some((year, (day - days_from_civil(year, 1, 1) + 1) as u16))
}

/// The checked day number of ordinal date `year-ordinal`.
#[must_use]
pub fn from_ordinal(year: i64, ordinal: u16) -> Option<i64> {
    (year_in_range(year) && (1..=days_in_year(year)).contains(&ordinal))
        .then(|| days_from_civil(year, 1, 1) + i64::from(ordinal) - 1)
}

/// The quarter (1-4) of `month` (1-12).
#[must_use]
pub const fn quarter_of_month(month: u8) -> u8 {
    month.div_ceil(3)
}

/// Days in `quarter` (1-4) of `year`: 90 or 91, 91, 92, 92; 0 outside 1-4.
#[must_use]
pub const fn days_in_quarter(year: i64, quarter: u8) -> u8 {
    match quarter {
        1 => 62 + days_in_month(year, 2),
        2 => 91,
        3 | 4 => 92,
        _ => 0,
    }
}

/// The `(year, quarter, day_of_quarter)` of a supported day number.
#[must_use]
pub fn to_quarter(day: i64) -> Option<(i64, u8, u8)> {
    let (year, month, _) = to_civil(day)?;
    let quarter = quarter_of_month(month);
    let first = days_from_civil(year, 3 * quarter - 2, 1);
    Some((year, quarter, (day - first + 1) as u8))
}

/// The checked day number of `year-Qquarter-day_of_quarter`.
#[must_use]
pub fn from_quarter(year: i64, quarter: u8, day_of_quarter: u8) -> Option<i64> {
    (year_in_range(year) && (1..=days_in_quarter(year, quarter)).contains(&day_of_quarter))
        .then(|| days_from_civil(year, 3 * quarter - 2, 1) + i64::from(day_of_quarter) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The calendar successor of a civil date, from month lengths alone: an
    /// oracle independent of the era arithmetic under test.
    fn successor((year, month, day): (i64, u8, u8)) -> (i64, u8, u8) {
        if day < days_in_month(year, month) {
            (year, month, day + 1)
        } else if month < 12 {
            (year, month + 1, 1)
        } else {
            (year + 1, 1, 1)
        }
    }

    /// Walk `days` consecutive day numbers from `start`: each converts to the
    /// successor of the previous one, converts back to itself, and its week,
    /// ordinal and quarter forms round-trip.
    fn walk(start: i64, days: i64) {
        let mut previous = to_civil(start).expect("walk starts in range");
        for day in start..start + days {
            let civil = to_civil(day).unwrap();
            if day > start {
                assert_eq!(civil, successor(previous), "day {day}");
            }
            previous = civil;
            assert_eq!(
                from_civil(civil.0, civil.1, civil.2),
                Some(day),
                "day {day}"
            );
            let (week_year, week, weekday) = to_iso_week(day).unwrap();
            assert_eq!(weekday, iso_weekday(day));
            assert!(
                (1..=weeks_in_week_year(week_year)).contains(&week),
                "day {day}"
            );
            assert_eq!(
                from_iso_week(week_year, week, weekday),
                Some(day),
                "day {day}"
            );
            let (year, ordinal) = to_ordinal(day).unwrap();
            assert_eq!(year, civil.0);
            assert_eq!(from_ordinal(year, ordinal), Some(day), "day {day}");
            let (year, quarter, day_of_quarter) = to_quarter(day).unwrap();
            assert_eq!((year, quarter), (civil.0, quarter_of_month(civil.1)));
            assert_eq!(from_quarter(year, quarter, day_of_quarter), Some(day));
        }
    }

    #[test]
    fn day_numbers_walk_the_calendar_and_round_trip_through_every_form() {
        const SPAN: i64 = 800 * 366;
        walk(-SPAN, 2 * SPAN);
        walk(MIN_EPOCH_DAY, SPAN);
        walk(MAX_EPOCH_DAY - SPAN + 1, SPAN);
    }

    #[test]
    fn anchors_agree_with_an_independent_calendar() {
        // Day numbers and ISO week dates cross-checked against Python's
        // datetime (date.toordinal() - 719163, date.isocalendar()).
        for ((year, month, day), number, week_date) in [
            ((1970, 1, 1), 0, (1970, 1, 4)),
            ((1969, 12, 31), -1, (1970, 1, 3)),
            ((2000, 3, 1), 11_017, (2000, 9, 3)),
            ((2000, 1, 1), 10_957, (1999, 52, 6)),
            ((2015, 7, 21), 16_637, (2015, 30, 2)),
            ((1, 1, 1), -719_162, (1, 1, 1)),
            ((9999, 12, 31), 2_932_896, (9999, 52, 5)),
            ((2014, 12, 29), 16_433, (2015, 1, 1)),
            ((2016, 1, 1), 16_801, (2015, 53, 5)),
            ((2010, 1, 3), 14_612, (2009, 53, 7)),
        ] {
            assert_eq!(
                from_civil(year, month, day),
                Some(number),
                "{year}-{month}-{day}"
            );
            assert_eq!(to_iso_week(number), Some(week_date), "{year}-{month}-{day}");
        }
        assert_eq!(from_civil(MIN_YEAR, 1, 1), Some(MIN_EPOCH_DAY));
        assert_eq!(from_civil(MAX_YEAR, 12, 31), Some(MAX_EPOCH_DAY));
        // One date, five spellings (the TCK's 2015-07-21).
        assert_eq!(from_iso_week(2015, 30, 2), Some(16_637));
        assert_eq!(from_ordinal(2015, 202), Some(16_637));
        assert_eq!(from_quarter(2015, 3, 21), Some(16_637));
    }

    #[test]
    fn a_400_year_cycle_has_97_leap_years_and_71_long_week_years() {
        let leap = (2000..2400).filter(|&year| is_leap_year(year)).count();
        let long = (2000..2400)
            .filter(|&year| weeks_in_week_year(year) == 53)
            .count();
        assert_eq!((leap, long), (97, 71));
        assert!(!is_leap_year(1900) && is_leap_year(2000) && is_leap_year(0));
        assert!(is_leap_year(-4) && !is_leap_year(-100) && is_leap_year(-400));
        assert_eq!(
            [1, 2, 3, 4].map(|quarter| days_in_quarter(2016, quarter)),
            [91, 91, 92, 92]
        );
        assert_eq!(days_in_quarter(2015, 1), 90);
    }

    #[test]
    fn checked_constructors_refuse_dates_that_do_not_exist() {
        assert_eq!(from_civil(2015, 2, 29), None);
        assert!(from_civil(2016, 2, 29).is_some());
        for (month, day) in [(0, 1), (13, 1), (1, 0), (4, 31)] {
            assert_eq!(from_civil(2015, month, day), None, "{month}-{day}");
        }
        assert_eq!(from_civil(MIN_YEAR - 1, 12, 31), None);
        assert_eq!(from_civil(MAX_YEAR + 1, 1, 1), None);
        assert!(from_iso_week(2015, 53, 7).is_some());
        assert_eq!(from_iso_week(2015, 54, 1), None);
        assert_eq!(from_iso_week(2014, 53, 1), None);
        assert_eq!(from_iso_week(2015, 1, 8), None);
        assert_eq!(from_iso_week(2015, 0, 1), None);
        assert_eq!(from_ordinal(2015, 366), None);
        assert!(from_ordinal(2016, 366).is_some());
        assert_eq!(from_ordinal(2015, 0), None);
        assert_eq!(from_quarter(2015, 1, 91), None);
        assert!(from_quarter(2016, 1, 91).is_some());
        assert_eq!(from_quarter(2015, 5, 1), None);
        // Week dates that spill past either end of the supported range.
        assert_eq!(from_iso_week(MAX_YEAR + 1, 1, 1), None);
        assert_eq!(to_civil(MIN_EPOCH_DAY - 1), None);
        assert_eq!(to_civil(MAX_EPOCH_DAY + 1), None);
        assert_eq!(to_iso_week(MAX_EPOCH_DAY + 1), None);
    }
}
