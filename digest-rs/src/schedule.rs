// Port of digest/schedule.mjs. Next-run calculation in a fixed IANA time zone.
//
// chrono-tz only answers "what wall clock is it at this instant" (the JS `parts()`); the rest is the
// JS algorithm line for line, so DST gaps/overlaps resolve to the same instant as Node.
use chrono::{DateTime, Datelike, Days, FixedOffset, NaiveDate, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use std::sync::LazyLock;

use crate::js::js_trim;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spec {
    /// 0 = SUN .. 6 = SAT; None for a daily spec.
    pub day: Option<u32>,
    pub hour: u32,
    pub minute: u32,
}

const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
// JS `\s` (see mask.rs); `\d` is ASCII in JS.
static DAILY: LazyLock<Result<regex::Regex, regex::Error>> =
    LazyLock::new(|| regex::Regex::new(r"^([0-9]{1,2}):([0-9]{2})$"));
static WEEKLY: LazyLock<Result<regex::Regex, regex::Error>> = LazyLock::new(|| {
    regex::Regex::new(
        r"^([A-Z]{3})[\t\n\x0B\x0C\r \u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]+([0-9]{1,2}):([0-9]{2})$",
    )
});

pub fn parse_spec(spec: &str, weekly: bool) -> Result<Spec, String> {
    let re = if weekly { &*WEEKLY } else { &*DAILY };
    let re = re.as_ref().map_err(|e| e.to_string())?;
    // JS toUpperCase is full Unicode (e.g. 'ı' -> 'I'), same as Rust's.
    let upper = js_trim(spec).to_uppercase();
    let invalid = || {
        format!(
            "invalid schedule \"{spec}\" (expected {})",
            if weekly { "MON 09:00" } else { "09:00" }
        )
    };
    let caps = re.captures(&upper).ok_or_else(invalid)?;
    let field = |i: usize| caps.get(i).map_or("", |m| m.as_str());
    let num = |i: usize| field(i).parse::<u32>().map_err(|_| invalid());
    let (hour, minute) = if weekly {
        (num(2)?, num(3)?)
    } else {
        (num(1)?, num(2)?)
    };
    let day = if weekly {
        let d = DAYS
            .iter()
            .position(|d| *d == field(1))
            .ok_or_else(|| format!("invalid weekday in \"{spec}\""))?;
        Some(d as u32)
    } else {
        None
    };
    if hour > 23 || minute > 59 {
        return Err(format!("invalid time in \"{spec}\""));
    }
    Ok(Spec { day, hour, minute })
}

/// A time zone as `Intl.DateTimeFormat` accepts it: IANA name (case-insensitive, links included) or a `±HH[:MM]` offset.
#[derive(Debug, Clone, Copy)]
pub enum Zone {
    Named(Tz),
    Fixed(FixedOffset),
}

impl Zone {
    pub fn parse(tz: &str) -> Result<Zone, String> {
        Tz::from_str_insensitive(tz)
            .map(Zone::Named)
            .ok()
            .or_else(|| parse_offset(tz).map(Zone::Fixed))
            .ok_or_else(|| format!("Invalid time zone specified: {tz}"))
    }

    /// Wall-clock fields at `ms` (JS `parts()`; Intl formats whole seconds, so sub-second is dropped).
    pub fn wall(&self, ms: i64) -> Option<Wall> {
        let utc = DateTime::<Utc>::from_timestamp_millis(ms)?;
        let offset = match self {
            Zone::Named(tz) => tz.offset_from_utc_datetime(&utc.naive_utc()).fix(),
            Zone::Fixed(o) => *o,
        };
        let t = utc.with_timezone(&offset);
        Some(Wall {
            year: t.year(),
            month: t.month(),
            day: t.day(),
            hour: t.hour(),
            minute: t.minute(),
            second: t.second(),
        })
    }
}

fn parse_offset(s: &str) -> Option<FixedOffset> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits = rest.replacen(
        ':',
        "",
        usize::from(rest.len() == 5 && rest.as_bytes()[2] == b':'),
    );
    if !(digits.len() == 2 || digits.len() == 4) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i32 = digits[..2].parse().ok()?;
    let m: i32 = digits
        .get(2..)
        .filter(|m| !m.is_empty())
        .map_or(Some(0), |m| m.parse().ok())?;
    if h > 23 || m > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Wall {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// JS `Date.UTC(y, mo - 1, d, h, mi, s)` with day overflow normalised.
fn date_utc(y: i32, mo: u32, d: i64, h: u32, mi: u32, s: u32) -> Option<i64> {
    let date = add_days(NaiveDate::from_ymd_opt(y, mo, 1)?, d - 1)?;
    Some(date.and_hms_opt(h, mi, s)?.and_utc().timestamp_millis())
}

fn add_days(date: NaiveDate, n: i64) -> Option<NaiveDate> {
    if n >= 0 {
        date.checked_add_days(Days::new(n.unsigned_abs()))
    } else {
        date.checked_sub_days(Days::new(n.unsigned_abs()))
    }
}

fn offset_ms(ms: i64, zone: &Zone) -> Option<i64> {
    let p = zone.wall(ms)?;
    Some(
        date_utc(
            p.year,
            p.month,
            i64::from(p.day),
            p.hour,
            p.minute,
            p.second,
        )? - ms.div_euclid(1000) * 1000,
    )
}

fn zoned_to_utc(date: NaiveDate, h: u32, mi: u32, zone: &Zone) -> Option<i64> {
    let wall = date_utc(date.year(), date.month(), i64::from(date.day()), h, mi, 0)?;
    let guess = wall - offset_ms(wall, zone)?;
    Some(wall - offset_ms(guess, zone)?)
}

/// Next instant strictly after `now_ms` matching the spec in `tz`. Recomputed on every call, so no drift accumulates.
pub fn next_run(now_ms: i64, spec: Spec, tz: &str) -> Result<i64, String> {
    let zone = Zone::parse(tz)?;
    let out_of_range = || "Invalid time value".to_string();
    let today = zone.wall(now_ms).ok_or_else(out_of_range)?;
    let start =
        NaiveDate::from_ymd_opt(today.year, today.month, today.day).ok_or_else(out_of_range)?;
    for i in 0..=8 {
        let base = add_days(start, i).ok_or_else(out_of_range)?;
        if spec
            .day
            .is_some_and(|d| base.weekday().num_days_from_sunday() != d)
        {
            continue;
        }
        let at = zoned_to_utc(base, spec.hour, spec.minute, &zone).ok_or_else(out_of_range)?;
        if at > now_ms {
            return Ok(at);
        }
    }
    Err("no next run found".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> i64 {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.timestamp_millis())
            .unwrap_or_else(|e| panic!("{s}: {e}"))
    }
    fn kst(s: &str) -> i64 {
        ms(&format!("{s}+09:00"))
    }
    /// `new Date(ms + 9h).toISOString().slice(0, 16)`
    fn iso(t: i64) -> String {
        DateTime::<Utc>::from_timestamp_millis(t + 9 * 3_600_000)
            .map(|d| d.format("%Y-%m-%dT%H:%M").to_string())
            .unwrap_or_default()
    }
    fn daily() -> Spec {
        parse_spec("09:00", false).unwrap()
    }
    fn next(now: i64, spec: Spec) -> i64 {
        next_run(now, spec, "Asia/Seoul").unwrap()
    }

    // ---- schedule.test.mjs ----
    #[test]
    fn daily_before_and_after_the_time_today() {
        assert_eq!(
            iso(next(kst("2026-10-21T08:00:00"), daily())),
            "2026-10-21T09:00"
        );
        assert_eq!(
            iso(next(kst("2026-10-21T09:00:00"), daily())),
            "2026-10-22T09:00"
        );
        assert_eq!(
            iso(next(kst("2026-10-21T23:59:59"), daily())),
            "2026-10-22T09:00"
        );
    }

    #[test]
    fn daily_rolls_over_month_and_year_end() {
        assert_eq!(
            iso(next(kst("2026-10-31T10:00:00"), daily())),
            "2026-11-01T09:00"
        );
        assert_eq!(
            iso(next(kst("2026-12-31T10:00:00"), daily())),
            "2027-01-01T09:00"
        );
        assert_eq!(
            iso(next(kst("2028-02-28T10:00:00"), daily())),
            "2028-02-29T09:00"
        );
    }

    #[test]
    fn weekly_mon_0900() {
        let spec = parse_spec("MON 09:00", true).unwrap();
        assert_eq!(
            iso(next(kst("2026-10-21T12:00:00"), spec)),
            "2026-10-26T09:00"
        );
        assert_eq!(
            iso(next(kst("2026-10-26T08:59:00"), spec)),
            "2026-10-26T09:00"
        );
        assert_eq!(
            iso(next(kst("2026-10-26T09:00:00"), spec)),
            "2026-11-02T09:00"
        );
        assert_eq!(
            iso(next(kst("2026-12-28T10:00:00"), spec)),
            "2027-01-04T09:00"
        );
    }

    #[test]
    fn uses_the_zone_not_the_host_zone() {
        assert_eq!(
            next(ms("2026-10-21T20:00:00Z"), daily()),
            ms("2026-10-22T00:00:00Z")
        );
    }

    #[test]
    fn other_zone_with_dst_resolves_the_wall_time() {
        assert_eq!(
            next_run(ms("2026-10-21T00:00:00Z"), daily(), "America/New_York"),
            Ok(ms("2026-10-21T13:00:00Z"))
        );
        assert_eq!(
            next_run(ms("2026-11-01T14:30:00Z"), daily(), "America/New_York"),
            Ok(ms("2026-11-02T14:00:00Z"))
        );
    }

    #[test]
    fn repeated_recomputation_does_not_drift() {
        let mut t = kst("2026-10-21T00:00:00");
        for _ in 0..400 {
            t = next(t + 1234, daily());
        }
        assert_eq!(&iso(t)[11..], "09:00");
    }

    #[test]
    fn invalid_specs_fail() {
        assert!(parse_spec("25:00", false).is_err());
        assert!(parse_spec("MON 09:00", false).is_err());
        assert!(parse_spec("XYZ 09:00", true).is_err());
    }

    // ---- error text and edges, expected values from Node ----
    #[test]
    fn error_messages_match_node() {
        assert_eq!(
            parse_spec(" 9:5 ", false),
            Err("invalid schedule \" 9:5 \" (expected 09:00)".into())
        );
        assert_eq!(
            parse_spec("09:00", true),
            Err("invalid schedule \"09:00\" (expected MON 09:00)".into())
        );
        assert_eq!(
            parse_spec("XYZ 09:00", true),
            Err("invalid weekday in \"XYZ 09:00\"".into())
        );
        assert_eq!(
            parse_spec("24:00", false),
            Err("invalid time in \"24:00\"".into())
        );
        assert_eq!(
            parse_spec("mon\t 7:05", true),
            Ok(Spec {
                day: Some(1),
                hour: 7,
                minute: 5
            })
        );
        assert_eq!(
            next_run(0, daily(), "Foo"),
            Err("Invalid time zone specified: Foo".into())
        );
    }

    #[test]
    fn zone_names_like_intl() {
        for tz in [
            "asia/seoul",
            "UTC",
            "utc",
            "US/Eastern",
            "+09:00",
            "-0330",
            "+05",
            "Etc/GMT-9",
        ] {
            assert!(Zone::parse(tz).is_ok(), "{tz}");
        }
        for tz in ["", " Asia/Seoul", "+9", "+24:00", "09:00"] {
            assert!(Zone::parse(tz).is_err(), "{tz}");
        }
        assert_eq!(
            next_run(ms("2026-10-21T00:00:00Z"), daily(), "+09:00"),
            Ok(ms("2026-10-22T00:00:00Z"))
        );
    }
}
