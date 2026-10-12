//! UTC timestamps, as RFC 3339 text: what a CRI log record starts with, and
//! what `logs --since` takes.
//!
//! Our own, not a crate's (H8: the ROADMAP's owned components): the kernel
//! gives seconds since 1970 (`CLOCK_REALTIME`, TLPI §10.1), and turning a
//! day count into a calendar date is a few lines of arithmetic, Howard
//! Hinnant's `civil_from_days` and `days_from_civil`. Years 0000 to 9999 (RFC
//! 3339's four digits); no leap seconds (`CLOCK_REALTIME` has none).

use std::fmt;

/// An instant, in UTC: seconds since 1970-01-01T00:00:00Z and nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    secs: i64,
    nanos: u32,
}

/// Text that isn't an RFC 3339 time.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not an RFC 3339 time (like 2026-10-10T06:16:19Z): {text:?}: {why}")]
pub struct TimeError {
    /// The text given.
    pub text: String,
    /// What's wrong with it.
    pub why: &'static str,
}

/// Seconds in a day.
const DAY: i64 = 86_400;

/// 0000-01-01T00:00:00Z and 9999-12-31T23:59:59Z, in seconds since 1970.
const FIRST: i64 = -62_167_219_200;
const LAST: i64 = 253_402_300_799;

impl Timestamp {
    /// Now, from the system's clock.
    #[must_use]
    pub fn now() -> Self {
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            secs: i64::try_from(since.as_secs()).unwrap_or(LAST).min(LAST),
            nanos: since.subsec_nanos(),
        }
    }

    /// The instant `secs` seconds and `nanos` nanoseconds after 1970, if it's
    /// in years 0000 to 9999 and `nanos` is under a second.
    #[must_use]
    pub fn from_unix(secs: i64, nanos: u32) -> Option<Self> {
        ((FIRST..=LAST).contains(&secs) && nanos < 1_000_000_000).then_some(Self { secs, nanos })
    }

    /// Seconds since 1970.
    #[must_use]
    pub fn secs(self) -> i64 {
        self.secs
    }

    /// The nanoseconds past [`secs`](Self::secs).
    #[must_use]
    pub fn nanos(self) -> u32 {
        self.nanos
    }

    /// Parses RFC 3339: `YYYY-MM-DDTHH:MM:SS`, an optional fraction (1 to 9
    /// digits), then `Z` or an offset (`+02:00`).
    ///
    /// # Errors
    ///
    /// [`TimeError`] for anything else, or a date that doesn't exist.
    pub fn parse(text: &str) -> Result<Self, TimeError> {
        let fail = |why| TimeError {
            text: text.to_owned(),
            why,
        };
        let bytes = text.as_bytes();
        let number = |at: usize, len: usize| -> Result<i64, TimeError> {
            let digits = bytes.get(at..at + len).ok_or_else(|| fail("too short"))?;
            if !digits.iter().all(u8::is_ascii_digit) {
                return Err(fail("a digit was expected"));
            }
            Ok(digits
                .iter()
                .fold(0, |sum, digit| sum * 10 + i64::from(digit - b'0')))
        };
        let expect = |at: usize, what: &[u8], why| -> Result<(), TimeError> {
            match bytes.get(at) {
                Some(found) if what.contains(found) => Ok(()),
                _ => Err(fail(why)),
            }
        };
        let year = number(0, 4)?;
        expect(4, b"-", "a '-' after the year")?;
        let month = number(5, 2)?;
        expect(7, b"-", "a '-' after the month")?;
        let day = number(8, 2)?;
        expect(10, b"Tt", "a 'T' between the date and the time")?;
        let hour = number(11, 2)?;
        expect(13, b":", "a ':' after the hour")?;
        let minute = number(14, 2)?;
        expect(16, b":", "a ':' after the minutes")?;
        let second = number(17, 2)?;
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return Err(fail("no such date"));
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Err(fail("no such time of day"));
        }
        let mut at = 19;
        let mut nanos = 0_i64;
        if bytes.get(at) == Some(&b'.') {
            let digits = bytes
                .get(at + 1..)
                .unwrap_or_default()
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits == 0 || digits > 9 {
                return Err(fail("a fraction of 1 to 9 digits"));
            }
            nanos = number(at + 1, digits)? * 10_i64.pow(9 - u32::try_from(digits).unwrap_or(9));
            at += 1 + digits;
        }
        let offset = match bytes.get(at) {
            Some(b'Z' | b'z') if bytes.len() == at + 1 => 0,
            Some(sign @ (b'+' | b'-')) if bytes.len() == at + 6 => {
                let hours = number(at + 1, 2)?;
                expect(at + 3, b":", "an offset like +02:00")?;
                let minutes = number(at + 4, 2)?;
                if hours > 23 || minutes > 59 {
                    return Err(fail("no such offset"));
                }
                let offset = hours * 3600 + minutes * 60;
                if *sign == b'-' { -offset } else { offset }
            }
            _ => return Err(fail("'Z' or an offset like +02:00 at the end")),
        };
        let secs =
            days_from_civil(year, month, day) * DAY + hour * 3600 + minute * 60 + second - offset;
        Self::from_unix(secs, u32::try_from(nanos).unwrap_or(0))
            .ok_or_else(|| fail("outside years 0000 to 9999 in UTC"))
    }
}

/// RFC 3339 in UTC, always with nine digits of fraction
/// (`2026-10-10T06:16:19.123456789Z`): the same length for every instant,
/// so records sort as text.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.secs.div_euclid(DAY);
        let of_day = self.secs.rem_euclid(DAY);
        let (year, month, day) = civil_from_days(days);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:09}Z",
            of_day / 3600,
            of_day % 3600 / 60,
            of_day % 60,
            self.nanos
        )
    }
}

/// Whether `year` has a 29th of February: every 4th, but not every 100th,
/// unless every 400th.
fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a date (proleptic Gregorian). Years are counted
/// from March, so that the leap day is a year's last: then a year's first
/// day is a simple formula of its number (`yoe * 365 + yoe / 4 - yoe / 100`
/// within a 400-year era of 146 097 days), and a day of the year one of the
/// month (`(153 * month + 2) / 5`, months of 31 and 30 days alternating
/// from March).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_from_march = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    // 719 468: days from 0000-03-01 to 1970-01-01.
    era * 146_097 + day_of_era - 719_468
}

/// The date of day `days` since 1970-01-01: [`days_from_civil`] backwards.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn at(secs: i64, nanos: u32) -> Timestamp {
        Timestamp::from_unix(secs, nanos).unwrap()
    }

    #[test]
    fn known_instants_format_as_rfc_3339() {
        for (secs, nanos, text) in [
            (0, 0, "1970-01-01T00:00:00.000000000Z"),
            (1, 5, "1970-01-01T00:00:01.000000005Z"),
            (-1, 999_999_999, "1969-12-31T23:59:59.999999999Z"),
            // Leap days: 2000 is one (every 400th), 2100 isn't (every 100th).
            (951_782_400, 0, "2000-02-29T00:00:00.000000000Z"),
            (4_107_542_400, 0, "2100-03-01T00:00:00.000000000Z"),
            (1_791_440_179, 123_456_789, "2026-10-08T06:16:19.123456789Z"),
            (FIRST, 0, "0000-01-01T00:00:00.000000000Z"),
            (LAST, 999_999_999, "9999-12-31T23:59:59.999999999Z"),
        ] {
            assert_eq!(at(secs, nanos).to_string(), text);
            assert_eq!(Timestamp::parse(text), Ok(at(secs, nanos)), "{text}");
        }
    }

    #[test]
    fn offsets_and_short_fractions_are_read() {
        let utc = Timestamp::parse("2026-10-10T06:16:19Z").unwrap();
        assert_eq!(Timestamp::parse("2026-10-10T08:16:19+02:00"), Ok(utc));
        assert_eq!(Timestamp::parse("2026-10-10T01:46:19-04:30"), Ok(utc));
        assert_eq!(Timestamp::parse("2026-10-10t06:16:19z"), Ok(utc));
        let half = Timestamp::parse("2026-10-10T06:16:19.5Z").unwrap();
        assert_eq!((half.secs(), half.nanos()), (utc.secs(), 500_000_000));
    }

    #[test]
    fn what_isnt_rfc_3339_is_refused() {
        for text in [
            "",
            "2026-10-10",
            "2026-10-10 06:16:19Z",
            "2026-10-10T06:16:19",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-10-10T24:00:00Z",
            "2026-10-10T06:60:00Z",
            "2026-10-10T06:16:60Z",
            "2026-10-10T06:16:19.Z",
            "2026-10-10T06:16:19.1234567890Z",
            "2026-10-10T06:16:19+2:00",
            "2026-10-10T06:16:19+24:00",
            "2026-10-10T06:16:19Zx",
            "0000-01-01T00:00:00+00:01",
            "２026-10-10T06:16:19Z",
        ] {
            assert!(Timestamp::parse(text).is_err(), "{text:?}");
        }
        assert_eq!(Timestamp::from_unix(LAST + 1, 0), None);
        assert_eq!(Timestamp::from_unix(0, 1_000_000_000), None);
    }

    #[test]
    fn every_day_of_four_centuries_round_trips() {
        let mut previous = None;
        for days in days_from_civil(1900, 1, 1)..days_from_civil(2300, 1, 1) {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
            assert!((1..=days_in_month(year, month)).contains(&day));
            if let Some((y, m, d)) = previous {
                // The next day of the calendar.
                let expected = if d < days_in_month(y, m) {
                    (y, m, d + 1)
                } else if m < 12 {
                    (y, m + 1, 1)
                } else {
                    (y + 1, 1, 1)
                };
                assert_eq!((year, month, day), expected);
            }
            previous = Some((year, month, day));
        }
    }

    /// The same text as GNU `date` gives for each of a spread of instants,
    /// years 0000 to 9999 (H8: our own calendar arithmetic against the
    /// standard tool's).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_same_as_date_says() {
        if !shipyard_test_support::reference_tool("date") {
            return;
        }
        // A fixed spread: a linear congruential generator, so a failure
        // can be run again.
        let mut state: u64 = 0x5eed;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            state >> 11
        };
        let span = u64::try_from(LAST - FIRST).unwrap();
        for _ in 0..200 {
            let secs = FIRST + i64::try_from(next() % span).unwrap();
            let nanos = u32::try_from(next() % 1_000_000_000).unwrap();
            // `@-5.25` is 5.25 s before 1970: an instant before it is
            // written as its distance back.
            let since = if secs < 0 && nanos > 0 {
                format!("-{}.{:09}", -(secs + 1), 1_000_000_000 - nanos)
            } else {
                format!("{secs}.{nanos:09}")
            };
            let out = std::process::Command::new("date")
                .env("LC_ALL", "C")
                .args(["-u", &format!("-d@{since}"), "+%4Y-%m-%dT%H:%M:%S.%NZ"])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let theirs = String::from_utf8(out.stdout).unwrap();
            assert_eq!(
                at(secs, nanos).to_string(),
                theirs.trim_end(),
                "{secs}.{nanos:09}"
            );
        }
    }

    proptest! {
        #[test]
        fn any_instant_round_trips(secs in FIRST..=LAST, nanos in 0..1_000_000_000_u32) {
            let time = at(secs, nanos);
            let text = time.to_string();
            prop_assert_eq!(text.len(), 30);
            prop_assert_eq!(Timestamp::parse(&text), Ok(time));
        }

        #[test]
        fn text_order_is_time_order(a in FIRST..=LAST, b in FIRST..=LAST) {
            prop_assert_eq!(at(a, 0).to_string().cmp(&at(b, 0).to_string()), a.cmp(&b));
        }

        #[test]
        fn any_text_is_parsed_or_refused(text in "\\PC{0,40}") {
            let _ = Timestamp::parse(&text);
        }
    }
}
