//! Time in UTC, to the microsecond, as PostgreSQL keeps it (agent guide, rule 3).
//!
//! No time library: a [`Timestamp`] is a count of microseconds since the Unix epoch, a [`UtcDay`] a count of days,
//! and a [`UtcMonth`] a year and a month. The calendar is the proleptic Gregorian one, converted with the integer
//! algorithms of Howard Hinnant's `chrono`-compatible date library (*days from civil*, *civil from days*).

use std::fmt;

use crate::error::Error;

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// The last day a [`Timestamp`] may fall on: 9999-12-31, so every timestamp formats with a four-digit year.
const LAST_DAY: i64 = 2_932_896;

/// An instant in UTC, in microseconds since 1970-01-01T00:00:00Z. From the epoch to the end of 9999-12-31.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The epoch, 1970-01-01T00:00:00Z.
    pub const EPOCH: Self = Self(0);

    /// The instant `micros` microseconds after the epoch.
    ///
    /// # Errors
    ///
    /// [`Error::TimestampOutOfRange`] before the epoch or after 9999-12-31.
    pub const fn from_unix_micros(micros: i64) -> Result<Self, Error> {
        if micros < 0 || micros >= (LAST_DAY + 1) * MICROS_PER_DAY {
            Err(Error::TimestampOutOfRange)
        } else {
            Ok(Self(micros))
        }
    }

    /// Microseconds since the epoch.
    #[must_use]
    pub const fn unix_micros(self) -> i64 {
        self.0
    }

    /// `self` moved by `micros` microseconds, or `None` outside the range.
    #[must_use]
    pub const fn checked_add_micros(self, micros: i64) -> Option<Self> {
        match self.0.checked_add(micros) {
            Some(sum) => match Self::from_unix_micros(sum) {
                Ok(at) => Some(at),
                Err(_) => None,
            },
            None => None,
        }
    }

    /// The UTC day this instant falls on.
    #[must_use]
    pub const fn utc_day(self) -> UtcDay {
        UtcDay(self.0.div_euclid(MICROS_PER_DAY))
    }

    /// The UTC month this instant falls in.
    #[must_use]
    pub const fn utc_month(self) -> UtcMonth {
        self.utc_day().month()
    }

    /// The instant as text, `YYYY-MM-DDTHH:MM:SS.ffffffZ`: the form the payload hash uses (ADR 0001, point 3).
    #[must_use]
    pub fn to_rfc3339_micros(self) -> String {
        let (year, month, day) = civil_from_days(self.utc_day().0);
        let of_day = self.0.rem_euclid(MICROS_PER_DAY);
        let seconds = of_day / 1_000_000;
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60,
            of_day % 1_000_000
        )
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339_micros())
    }
}

/// A day in UTC, counted from 1970-01-01 (day 0). The partition key of a usage event (ADR 0001, point 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcDay(i64);

impl UtcDay {
    /// The day `days` days after 1970-01-01.
    ///
    /// # Errors
    ///
    /// [`Error::TimestampOutOfRange`] before 1970-01-01 or after 9999-12-31.
    pub const fn from_unix_days(days: i64) -> Result<Self, Error> {
        if days < 0 || days > LAST_DAY {
            Err(Error::TimestampOutOfRange)
        } else {
            Ok(Self(days))
        }
    }

    /// The day of a calendar date, or `None` for a date that does not exist or is out of range.
    #[must_use]
    pub fn from_ymd(year: i32, month: u8, day: u8) -> Option<Self> {
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return None;
        }
        Self::from_unix_days(days_from_civil(year, month, day)).ok()
    }

    /// Days since 1970-01-01.
    #[must_use]
    pub const fn unix_days(self) -> i64 {
        self.0
    }

    /// Midnight UTC at the start of this day.
    #[must_use]
    pub const fn start(self) -> Timestamp {
        Timestamp(self.0 * MICROS_PER_DAY)
    }

    /// The day `days` later (or earlier, when negative), or `None` outside the range.
    #[must_use]
    pub const fn checked_add_days(self, days: i64) -> Option<Self> {
        match self.0.checked_add(days) {
            Some(next) => match Self::from_unix_days(next) {
                Ok(day) => Some(day),
                Err(_) => None,
            },
            None => None,
        }
    }

    /// The year, the month (1 to 12) and the day of the month (1 to 31).
    #[must_use]
    pub const fn ymd(self) -> (i32, u8, u8) {
        civil_from_days(self.0)
    }

    /// The month this day falls in.
    #[must_use]
    pub const fn month(self) -> UtcMonth {
        let (year, month, _) = civil_from_days(self.0);
        UtcMonth { year, month }
    }
}

impl fmt::Display for UtcDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.ymd();
        write!(f, "{year:04}-{month:02}-{day:02}")
    }
}

/// A calendar month in UTC: the period of a member cap and of an allowance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcMonth {
    year: i32,
    month: u8,
}

impl UtcMonth {
    /// The month `month` (1 to 12) of `year`, or `None` outside 1970-01 to 9999-12.
    #[must_use]
    pub fn new(year: i32, month: u8) -> Option<Self> {
        UtcDay::from_ymd(year, month, 1).map(UtcDay::month)
    }

    /// The year.
    #[must_use]
    pub const fn year(self) -> i32 {
        self.year
    }

    /// The month, 1 to 12.
    #[must_use]
    pub const fn month(self) -> u8 {
        self.month
    }

    /// The first day of the month.
    #[must_use]
    pub const fn first_day(self) -> UtcDay {
        UtcDay(days_from_civil(self.year, self.month, 1))
    }

    /// Midnight UTC on the first day of the month.
    #[must_use]
    pub const fn start(self) -> Timestamp {
        self.first_day().start()
    }

    /// The month after this one. After 9999-12 there is none.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        if self.month == 12 {
            if self.year >= 9999 {
                None
            } else {
                Some(Self {
                    year: self.year + 1,
                    month: 1,
                })
            }
        } else {
            Some(Self {
                year: self.year,
                month: self.month + 1,
            })
        }
    }

    /// When this month ends: midnight UTC on the first day of the next month. For 9999-12, the end of the range.
    #[must_use]
    pub const fn end(self) -> Timestamp {
        match self.next() {
            Some(next) => next.start(),
            None => Timestamp((LAST_DAY + 1) * MICROS_PER_DAY - 1),
        }
    }
}

impl fmt::Display for UtcMonth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
    }
}

const fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant, *days_from_civil*).
const fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = if month <= 2 { year - 1 } else { year } as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of a count of days since 1970-01-01 (Hinnant, *civil_from_days*).
#[allow(clippy::cast_possible_truncation)]
const fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    // In range by construction: the year fits an i32 for every day a `UtcDay` allows, the month is 1 to 12 and the
    // day 1 to 31.
    (year as i32, month as u8, day as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(micros: i64) -> Timestamp {
        Timestamp::from_unix_micros(micros).expect("in range")
    }

    #[test]
    fn the_range_is_the_epoch_to_the_end_of_9999() {
        assert_eq!(
            Timestamp::from_unix_micros(-1),
            Err(Error::TimestampOutOfRange)
        );
        let last = (LAST_DAY + 1) * MICROS_PER_DAY - 1;
        assert_eq!(at(last).to_rfc3339_micros(), "9999-12-31T23:59:59.999999Z");
        assert_eq!(
            Timestamp::from_unix_micros(last + 1),
            Err(Error::TimestampOutOfRange)
        );
        assert_eq!(at(0).checked_add_micros(-1), None);
    }

    #[test]
    fn formats_to_the_microsecond_in_utc() {
        assert_eq!(at(0).to_string(), "1970-01-01T00:00:00.000000Z");
        // 2026-11-27T10:15:30.000042Z
        let micros = 1_795_774_530_000_042;
        assert_eq!(
            at(micros).to_rfc3339_micros(),
            "2026-11-27T10:15:30.000042Z"
        );
        assert_eq!(at(micros).utc_day().to_string(), "2026-11-27");
        assert_eq!(at(micros).utc_month().to_string(), "2026-11");
    }

    #[test]
    fn days_and_months_round_trip_across_leap_years() {
        for (y, m, d) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (2024, 2, 29),
            (2026, 12, 31),
            (2100, 3, 1),
            (9999, 12, 31),
        ] {
            let day = UtcDay::from_ymd(y, m, d).expect("a real date");
            assert_eq!(day.ymd(), (y, m, d));
        }
        assert_eq!(UtcDay::from_ymd(2026, 2, 29), None);
        assert_eq!(UtcDay::from_ymd(2100, 2, 29), None);
        assert_eq!(UtcDay::from_ymd(2026, 13, 1), None);
        assert_eq!(UtcDay::from_ymd(1969, 12, 31), None);
    }

    /// The allowance's periods and renewals are UTC months.
    #[test]
    fn periods_and_renewals_are_utc_months() {
        let day = |y, m, d| UtcDay::from_ymd(y, m, d).expect("a real date");
        let late = day(2026, 10, 31)
            .start()
            .checked_add_micros(MICROS_PER_DAY - 1);
        let late = late.expect("in range");
        assert_eq!(late.utc_month().to_string(), "2026-10");
        assert_eq!(day(2026, 11, 1).start().utc_month().to_string(), "2026-11");
        let october = UtcMonth::new(2026, 10).expect("a month");
        assert_eq!(october.end(), day(2026, 11, 1).start());
        let december = UtcMonth::new(2026, 12).expect("a month");
        assert_eq!(december.end(), day(2027, 1, 1).start());
        assert_eq!(UtcMonth::new(9999, 12).and_then(UtcMonth::next), None);
    }
}
