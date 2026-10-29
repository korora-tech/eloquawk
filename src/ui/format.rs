//! Date, time and size strings for the UI. Pure functions with the clock and time zone passed
//! in, so they are tested without depending on when or where the tests run.

use jiff::{Timestamp, ToSpan, Zoned};

/// "14:05" or "2:05pm" ("2pm" on the hour).
pub fn time(z: &Zoned, clock_24h: bool) -> String {
    if clock_24h {
        z.strftime("%H:%M").to_string()
    } else if z.minute() == 0 {
        z.strftime("%-I%P").to_string()
    } else {
        z.strftime("%-I:%M%P").to_string()
    }
}

/// The thread list's date column, relative to `now` in `now`'s time zone: today shows the
/// time, the six days before it the weekday, this year "3 Oct", older "3 Oct 2025".
pub fn list_date(t: Timestamp, now: &Zoned, clock_24h: bool) -> String {
    let z = t.to_zoned(now.time_zone().clone());
    let (date, today) = (z.date(), now.date());
    if date == today {
        return time(&z, clock_24h);
    }
    let week_ago = today.checked_sub(6.days()).unwrap_or(today);
    if date < today && date >= week_ago {
        return z.strftime("%a").to_string();
    }
    if date.year() == today.year() {
        z.strftime("%-d %b").to_string()
    } else {
        z.strftime("%-d %b %Y").to_string()
    }
}

/// The reader's full date: "Mon 3 Oct 2026, 14:05".
pub fn full_date(t: Timestamp, now: &Zoned, clock_24h: bool) -> String {
    let z = t.to_zoned(now.time_zone().clone());
    format!("{}, {}", z.strftime("%a %-d %b %Y"), time(&z, clock_24h))
}

/// A human-readable size: "512 B", "1.5 KB", "12 MB".
pub fn size_human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if v < 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::tz::TimeZone;

    fn now() -> Zoned {
        // Thursday 29 October 2026, 15:30 in Berlin.
        "2026-10-29T15:30:00+01:00[Europe/Berlin]"
            .parse()
            .expect("valid zoned")
    }

    fn at(s: &str) -> Timestamp {
        s.parse().expect("valid timestamp")
    }

    #[test]
    fn list_date_today_shows_time() {
        assert_eq!(list_date(at("2026-10-29T08:05:00Z"), &now(), true), "09:05");
        assert_eq!(
            list_date(at("2026-10-29T08:05:00Z"), &now(), false),
            "9:05am"
        );
        assert_eq!(list_date(at("2026-10-29T13:00:00Z"), &now(), false), "2pm");
    }

    #[test]
    fn list_date_uses_the_given_zone() {
        // 23:30 UTC on the 28th is already the 29th in Berlin.
        assert_eq!(list_date(at("2026-10-28T23:30:00Z"), &now(), true), "00:30");
        let utc = now().with_time_zone(TimeZone::UTC);
        assert_eq!(list_date(at("2026-10-28T23:30:00Z"), &utc, true), "Wed");
    }

    #[test]
    fn list_date_this_week_shows_weekday() {
        assert_eq!(list_date(at("2026-10-28T10:00:00Z"), &now(), true), "Wed");
        assert_eq!(list_date(at("2026-10-23T10:00:00Z"), &now(), true), "Fri");
    }

    #[test]
    fn list_date_this_year_and_older() {
        assert_eq!(
            list_date(at("2026-10-22T10:00:00Z"), &now(), true),
            "22 Oct"
        );
        assert_eq!(list_date(at("2026-01-03T10:00:00Z"), &now(), true), "3 Jan");
        assert_eq!(
            list_date(at("2025-10-03T10:00:00Z"), &now(), true),
            "3 Oct 2025"
        );
        // A date in the future (clock skew) is not a weekday.
        assert_eq!(
            list_date(at("2026-10-30T10:00:00Z"), &now(), true),
            "30 Oct"
        );
    }

    #[test]
    fn full_date_reads_naturally() {
        assert_eq!(
            full_date(at("2026-10-26T13:05:00Z"), &now(), true),
            "Mon 26 Oct 2026, 14:05"
        );
        assert_eq!(
            full_date(at("2026-10-26T13:05:00Z"), &now(), false),
            "Mon 26 Oct 2026, 2:05pm"
        );
    }

    #[test]
    fn sizes() {
        assert_eq!(size_human(0), "0 B");
        assert_eq!(size_human(1023), "1023 B");
        assert_eq!(size_human(1536), "1.5 KB");
        assert_eq!(size_human(200 * 1024), "200 KB");
        assert_eq!(size_human(12 * 1024 * 1024), "12 MB");
        assert_eq!(size_human(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
