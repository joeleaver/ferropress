//! WordPress-style date formatting for the public theme.
//!
//! Post/Page dates are stored as `Value::DateTime` — epoch millis, UTC — and the
//! Settings page lets an author pick a **PHP `date()`-style** pattern
//! (`F j, Y`, `Y-m-d`, `d/m/Y`, or a custom string) plus a `site.timezone`. The
//! serve layer formats each date *live*, at request time, so a change to the
//! format or timezone is reflected without touching the prerender cache (the
//! serving model's "don't couple a global setting to every cached page" rule).
//!
//! [`format_php_date`] is the pure token engine (operates on an already-zoned
//! [`OffsetDateTime`], so it is trivially unit-testable). [`format_millis_utc`]
//! is the UTC convenience; the timezone-aware wrapper that resolves
//! `site.timezone` lives alongside once the tz database is wired.
//!
//! The supported tokens are the WordPress-common subset of PHP `date()`; an
//! unrecognized character is emitted verbatim (as PHP does), and a backslash
//! escapes the next character so it is emitted literally (`\Y` → `Y`).

use std::fmt::Write as _;

use time::{Month, OffsetDateTime, Weekday};
use time_tz::{OffsetDateTimeExt, timezones};

/// Format an already-zoned [`OffsetDateTime`] with a PHP `date()`-style pattern.
///
/// This is pure: the caller is responsible for having converted the instant into
/// the desired timezone (the offset carried by `dt` is what `O`/`P` report). See
/// the module docs for the token table.
pub fn format_php_date(dt: OffsetDateTime, format: &str) -> String {
    let mut out = String::with_capacity(format.len() + 8);
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        match c {
            // A backslash escapes the next character (emit it literally). A
            // trailing backslash with nothing after it is dropped, as in PHP.
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }

            // ── Day ──────────────────────────────────────────────────────
            'd' => {
                let _ = write!(out, "{:02}", dt.day());
            }
            'j' => {
                let _ = write!(out, "{}", dt.day());
            }
            'D' => out.push_str(weekday_short(dt.weekday())),
            'l' => out.push_str(weekday_name(dt.weekday())),
            'N' => {
                let _ = write!(out, "{}", dt.weekday().number_from_monday());
            }
            'w' => {
                let _ = write!(out, "{}", dt.weekday().number_days_from_sunday());
            }
            'S' => out.push_str(ordinal_suffix(dt.day())),
            'z' => {
                // PHP: day of year, 0-indexed. `ordinal()` is 1-based.
                let _ = write!(out, "{}", dt.ordinal() - 1);
            }

            // ── Month ────────────────────────────────────────────────────
            'F' => out.push_str(month_name(dt.month())),
            'M' => out.push_str(month_short(dt.month())),
            'm' => {
                let _ = write!(out, "{:02}", u8::from(dt.month()));
            }
            'n' => {
                let _ = write!(out, "{}", u8::from(dt.month()));
            }
            't' => {
                let _ = write!(out, "{}", dt.month().length(dt.year()));
            }

            // ── Year ─────────────────────────────────────────────────────
            'L' => out.push(if time::util::is_leap_year(dt.year()) {
                '1'
            } else {
                '0'
            }),
            'Y' => {
                let _ = write!(out, "{}", dt.year());
            }
            'y' => {
                // Last two digits, zero-padded. `rem_euclid` keeps it non-negative
                // for pre-year-0 dates.
                let _ = write!(out, "{:02}", dt.year().rem_euclid(100));
            }

            // ── Time ─────────────────────────────────────────────────────
            'a' => out.push_str(if dt.hour() < 12 { "am" } else { "pm" }),
            'A' => out.push_str(if dt.hour() < 12 { "AM" } else { "PM" }),
            'g' => {
                let _ = write!(out, "{}", hour12(dt.hour()));
            }
            'G' => {
                let _ = write!(out, "{}", dt.hour());
            }
            'h' => {
                let _ = write!(out, "{:02}", hour12(dt.hour()));
            }
            'H' => {
                let _ = write!(out, "{:02}", dt.hour());
            }
            'i' => {
                let _ = write!(out, "{:02}", dt.minute());
            }
            's' => {
                let _ = write!(out, "{:02}", dt.second());
            }

            // ── Timezone / epoch ─────────────────────────────────────────
            'U' => {
                let _ = write!(out, "{}", dt.unix_timestamp());
            }
            'O' => out.push_str(&offset_hm(dt, false)),
            'P' => out.push_str(&offset_hm(dt, true)),

            // Any other character is a literal (PHP emits unrecognized chars
            // as-is), which is how separators like `-`, `/`, `,`, and spaces work.
            other => out.push(other),
        }
    }
    out
}

/// Format an epoch-millis instant with a PHP `date()`-style pattern, in **UTC**.
///
/// Out-of-range millis fall back to the Unix epoch rather than panicking — a
/// stored date can never crash a page render.
pub fn format_millis_utc(millis: i64, format: &str) -> String {
    format_php_date(millis_to_utc(millis), format)
}

/// Format an epoch-millis instant with a PHP `date()`-style pattern, converted
/// into the named IANA timezone (`site.timezone`) first, so the wall-clock and
/// the `O`/`P` offset tokens reflect the reader's local time (DST included).
///
/// An unknown or empty `tz_name` (or literally `"UTC"`) leaves the instant in
/// UTC — a stored setting can never crash or mislocate a render. The zone
/// database is bundled (see the `time-tz` dep note), so this does not depend on
/// the host's `/usr/share/zoneinfo`.
pub fn format_datetime(millis: i64, format: &str, tz_name: &str) -> String {
    let utc = millis_to_utc(millis);
    let dt = match timezones::get_by_name(tz_name) {
        Some(tz) => utc.to_timezone(tz),
        None => utc,
    };
    format_php_date(dt, format)
}

/// Epoch-millis → a UTC [`OffsetDateTime`], clamped to the epoch on overflow.
fn millis_to_utc(millis: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos((millis as i128) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn hour12(hour24: u8) -> u8 {
    match hour24 % 12 {
        0 => 12,
        h => h,
    }
}

fn ordinal_suffix(day: u8) -> &'static str {
    match day % 100 {
        11..=13 => "th",
        _ => match day % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    }
}

fn offset_hm(dt: OffsetDateTime, colon: bool) -> String {
    let secs = dt.offset().whole_seconds();
    let sign = if secs < 0 { '-' } else { '+' };
    let secs = secs.unsigned_abs();
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if colon {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}{m:02}")
    }
}

fn month_name(m: Month) -> &'static str {
    match m {
        Month::January => "January",
        Month::February => "February",
        Month::March => "March",
        Month::April => "April",
        Month::May => "May",
        Month::June => "June",
        Month::July => "July",
        Month::August => "August",
        Month::September => "September",
        Month::October => "October",
        Month::November => "November",
        Month::December => "December",
    }
}

/// Three-letter month abbreviation. All English month names are ASCII and ≥ 3
/// bytes, so a byte slice of the first three is safe and correct.
fn month_short(m: Month) -> &'static str {
    &month_name(m)[..3]
}

fn weekday_name(w: Weekday) -> &'static str {
    match w {
        Weekday::Monday => "Monday",
        Weekday::Tuesday => "Tuesday",
        Weekday::Wednesday => "Wednesday",
        Weekday::Thursday => "Thursday",
        Weekday::Friday => "Friday",
        Weekday::Saturday => "Saturday",
        Weekday::Sunday => "Sunday",
    }
}

/// Three-letter weekday abbreviation (ASCII, so byte-slicing is safe).
fn weekday_short(w: Weekday) -> &'static str {
    &weekday_name(w)[..3]
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

    /// Build a UTC datetime without the `time` `macros` feature (the workspace
    /// dep enables only serde/formatting/parsing).
    fn utc(y: i32, mo: Month, d: u8, h: u8, mi: u8, s: u8) -> OffsetDateTime {
        PrimitiveDateTime::new(
            Date::from_calendar_date(y, mo, d).unwrap(),
            Time::from_hms(h, mi, s).unwrap(),
        )
        .assume_utc()
    }

    #[test]
    fn formats_the_three_settings_presets() {
        // 2026-01-08 is a Thursday.
        let dt = utc(2026, Month::January, 8, 15, 4, 5);
        assert_eq!(format_php_date(dt, "F j, Y"), "January 8, 2026");
        assert_eq!(format_php_date(dt, "Y-m-d"), "2026-01-08");
        assert_eq!(format_php_date(dt, "d/m/Y"), "08/01/2026");
    }

    #[test]
    fn day_and_month_tokens() {
        let dt = utc(2026, Month::January, 8, 15, 4, 5);
        assert_eq!(format_php_date(dt, "j"), "8");
        assert_eq!(format_php_date(dt, "d"), "08");
        assert_eq!(format_php_date(dt, "n"), "1");
        assert_eq!(format_php_date(dt, "m"), "01");
        assert_eq!(format_php_date(dt, "M"), "Jan");
        assert_eq!(format_php_date(dt, "F"), "January");
        assert_eq!(format_php_date(dt, "t"), "31"); // January has 31 days
        // June is only 3 letters — the short-name slice must not panic.
        assert_eq!(
            format_php_date(utc(2026, Month::June, 1, 0, 0, 0), "M"),
            "Jun"
        );
        assert_eq!(
            format_php_date(utc(2026, Month::May, 1, 0, 0, 0), "M"),
            "May"
        );
    }

    #[test]
    fn weekday_tokens() {
        let thu = utc(2026, Month::January, 8, 12, 0, 0);
        assert_eq!(format_php_date(thu, "l"), "Thursday");
        assert_eq!(format_php_date(thu, "D"), "Thu");
        assert_eq!(format_php_date(thu, "N"), "4"); // ISO Mon=1 → Thu=4
        assert_eq!(format_php_date(thu, "w"), "4"); // Sun=0 → Thu=4
    }

    #[test]
    fn ordinal_suffixes() {
        let mk = |d: u8| format_php_date(utc(2026, Month::January, d, 0, 0, 0), "jS");
        assert_eq!(mk(1), "1st");
        assert_eq!(mk(2), "2nd");
        assert_eq!(mk(3), "3rd");
        assert_eq!(mk(4), "4th");
        assert_eq!(mk(11), "11th");
        assert_eq!(mk(12), "12th");
        assert_eq!(mk(13), "13th");
        assert_eq!(mk(21), "21st");
        assert_eq!(mk(22), "22nd");
        assert_eq!(mk(23), "23rd");
    }

    #[test]
    fn twelve_and_twenty_four_hour_time() {
        let afternoon = utc(2026, Month::January, 8, 15, 4, 5);
        assert_eq!(format_php_date(afternoon, "G:i"), "15:04");
        assert_eq!(format_php_date(afternoon, "H:i:s"), "15:04:05");
        assert_eq!(format_php_date(afternoon, "g:i A"), "3:04 PM");
        assert_eq!(format_php_date(afternoon, "h:i a"), "03:04 pm");

        let midnight = utc(2026, Month::January, 8, 0, 30, 0);
        assert_eq!(format_php_date(midnight, "g A"), "12 AM");
        assert_eq!(format_php_date(midnight, "h"), "12");

        let noon = utc(2026, Month::January, 8, 12, 0, 0);
        assert_eq!(format_php_date(noon, "g A"), "12 PM");
    }

    #[test]
    fn year_tokens_and_leap_year() {
        assert_eq!(
            format_php_date(utc(2026, Month::January, 8, 0, 0, 0), "Y"),
            "2026"
        );
        assert_eq!(
            format_php_date(utc(2005, Month::January, 8, 0, 0, 0), "y"),
            "05"
        );
        assert_eq!(
            format_php_date(utc(2024, Month::February, 1, 0, 0, 0), "L t"),
            "1 29" // 2024 is a leap year → February has 29 days
        );
        assert_eq!(
            format_php_date(utc(2026, Month::February, 1, 0, 0, 0), "L t"),
            "0 28"
        );
    }

    #[test]
    fn backslash_escapes_a_literal_token_char() {
        let dt = utc(2026, Month::January, 8, 15, 4, 5);
        // `\Y` → literal "Y"; the following `-m` still formats.
        assert_eq!(format_php_date(dt, r"\Y-m"), "Y-01");
        // A word made of format chars, fully escaped, survives verbatim.
        assert_eq!(format_php_date(dt, r"\j\S\o\n"), "jSon");
        // A trailing lone backslash is dropped (PHP behavior).
        assert_eq!(format_php_date(dt, r"Y\"), "2026");
    }

    #[test]
    fn unrecognized_characters_pass_through() {
        let dt = utc(2026, Month::January, 8, 15, 4, 5);
        // Punctuation / separators / non-token letters emit verbatim.
        assert_eq!(format_php_date(dt, "Y. // (x)"), "2026. // (x)");
    }

    #[test]
    fn timezone_offset_tokens_read_the_carried_offset() {
        // Same wall-clock instant rendered at a -05:00 offset.
        let dt = PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::January, 8).unwrap(),
            Time::from_hms(10, 0, 0).unwrap(),
        )
        .assume_offset(UtcOffset::from_hms(-5, 0, 0).unwrap());
        assert_eq!(format_php_date(dt, "O"), "-0500");
        assert_eq!(format_php_date(dt, "P"), "-05:00");
        assert_eq!(format_php_date(dt, "H"), "10"); // hour is as-carried

        let plus = PrimitiveDateTime::new(
            Date::from_calendar_date(2026, Month::January, 8).unwrap(),
            Time::from_hms(10, 0, 0).unwrap(),
        )
        .assume_offset(UtcOffset::from_hms(5, 30, 0).unwrap());
        assert_eq!(format_php_date(plus, "P"), "+05:30");
    }

    #[test]
    fn format_datetime_resolves_named_zone_with_dst() {
        // A winter and a summer instant, both 15:00 UTC, rendered in New York:
        // EST (UTC-5) in January → 10:00; EDT (UTC-4) in July → 11:00. This only
        // holds if the tz database applies DST, not a fixed offset.
        let winter = utc(2026, Month::January, 8, 15, 0, 0).unix_timestamp() * 1000;
        let summer = utc(2026, Month::July, 8, 15, 0, 0).unix_timestamp() * 1000;
        assert_eq!(format_datetime(winter, "H:i", "America/New_York"), "10:00");
        assert_eq!(format_datetime(summer, "H:i", "America/New_York"), "11:00");
        // The date can roll back across midnight in a western zone.
        let just_after_midnight_utc = utc(2026, Month::January, 8, 2, 0, 0).unix_timestamp() * 1000;
        assert_eq!(
            format_datetime(just_after_midnight_utc, "Y-m-d", "America/Los_Angeles"),
            "2026-01-07"
        );
    }

    #[test]
    fn format_datetime_unknown_or_utc_zone_stays_utc() {
        let instant = utc(2026, Month::January, 8, 15, 0, 0).unix_timestamp() * 1000;
        assert_eq!(format_datetime(instant, "H:i", "UTC"), "15:00");
        assert_eq!(format_datetime(instant, "H:i", ""), "15:00");
        // A bogus stored zone must not crash — it falls back to UTC.
        assert_eq!(
            format_datetime(instant, "H:i", "Mars/Olympus_Mons"),
            "15:00"
        );
    }

    #[test]
    fn out_of_range_millis_falls_back_to_epoch_not_panic() {
        // i64::MAX millis overflows the representable range → epoch, not a panic.
        assert_eq!(format_millis_utc(i64::MAX, "Y-m-d"), "1970-01-01");
        assert_eq!(format_millis_utc(0, "Y-m-d"), "1970-01-01");
    }
}
