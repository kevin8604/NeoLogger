use std::sync::LazyLock;

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use regex::Regex;

pub const UNKNOWN: i64 = i64::MIN;

static DATE_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\[?(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2}):(\d{2})(?:[.,](\d{1,9}))?")
        .unwrap()
});

static TIME_TS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\[?(\d{2}):(\d{2}):(\d{2})(?:[.,](\d{1,9}))?").unwrap());

enum Stamp {
    Epoch(i64),
    TimeOfDay(i64),
}

pub fn local_day_base() -> i64 {
    let midnight = Local::now().date_naive().and_hms_opt(0, 0, 0).unwrap();
    midnight.and_utc().timestamp_nanos_opt().unwrap_or(0)
}

pub fn parse_common(line: &str, day_base: i64) -> Option<i64> {
    if let Some((stamp, _)) = scan_prefix(line) {
        return Some(match stamp {
            Stamp::Epoch(nanos) => nanos,
            Stamp::TimeOfDay(nanos) => day_base + nanos,
        });
    }
    if let Some(c) = DATE_TS.captures(line) {
        let date = NaiveDate::from_ymd_opt(num(&c, 1)?, num(&c, 2)?, num(&c, 3)?)?;
        let time = NaiveTime::from_hms_nano_opt(
            num(&c, 4)?,
            num(&c, 5)?,
            num(&c, 6)?,
            frac_nanos(c.get(7).map(|m| m.as_str())),
        )?;
        return date.and_time(time).and_utc().timestamp_nanos_opt();
    }
    if let Some(c) = TIME_TS.captures(line) {
        let time = NaiveTime::from_hms_nano_opt(
            num(&c, 1)?,
            num(&c, 2)?,
            num(&c, 3)?,
            frac_nanos(c.get(4).map(|m| m.as_str())),
        )?;
        let offset =
            time.num_seconds_from_midnight() as i64 * 1_000_000_000 + time.nanosecond() as i64;
        return Some(day_base + offset);
    }
    None
}

/// Digit-based fast path for the two timestamp shapes accepted by the regexes.
/// Returns the stamp and the byte offset just past the fraction. Unicode
/// whitespace falls through to the regex fallback.
fn scan_prefix(line: &str) -> Option<(Stamp, usize)> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if b.get(i) == Some(&b'[') {
        i += 1;
    }
    if !b.get(i)?.is_ascii_digit() {
        return None;
    }
    if b.get(i + 4) == Some(&b'-') {
        let year = digits(b, i, 4)? as i32;
        if b.get(i + 7) != Some(&b'-') {
            return None;
        }
        let month = digits(b, i + 5, 2)?;
        let day = digits(b, i + 8, 2)?;
        if !matches!(b.get(i + 10), Some(&b' ') | Some(&b'T')) {
            return None;
        }
        let hour = digits(b, i + 11, 2)?;
        if b.get(i + 13) != Some(&b':') {
            return None;
        }
        let minute = digits(b, i + 14, 2)?;
        if b.get(i + 16) != Some(&b':') {
            return None;
        }
        let second = digits(b, i + 17, 2)?;
        let (nanos, end) = frac_at(b, i + 19);
        let date = NaiveDate::from_ymd_opt(year, month, day)?;
        let time = NaiveTime::from_hms_nano_opt(hour, minute, second, nanos)?;
        let epoch = date.and_time(time).and_utc().timestamp_nanos_opt()?;
        return Some((Stamp::Epoch(epoch), end));
    }
    if b.get(i + 2) != Some(&b':') {
        return None;
    }
    let hour = digits(b, i, 2)?;
    let minute = digits(b, i + 3, 2)?;
    if b.get(i + 5) != Some(&b':') {
        return None;
    }
    let second = digits(b, i + 6, 2)?;
    let (nanos, end) = frac_at(b, i + 8);
    let time = NaiveTime::from_hms_nano_opt(hour, minute, second, nanos)?;
    let offset = time.num_seconds_from_midnight() as i64 * 1_000_000_000 + time.nanosecond() as i64;
    Some((Stamp::TimeOfDay(offset), end))
}

fn digits(b: &[u8], start: usize, count: usize) -> Option<u32> {
    let slice = b.get(start..start + count)?;
    let mut value = 0u32;
    for &c in slice {
        if !c.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (c - b'0') as u32;
    }
    Some(value)
}

fn frac_at(b: &[u8], start: usize) -> (u32, usize) {
    if !matches!(b.get(start), Some(&b'.') | Some(&b',')) {
        return (0, start);
    }
    let mut value = 0u32;
    let mut digits = 0u32;
    let mut i = start + 1;
    while digits < 9 && i < b.len() && b[i].is_ascii_digit() {
        value = value * 10 + (b[i] - b'0') as u32;
        digits += 1;
        i += 1;
    }
    if digits == 0 {
        return (0, start);
    }
    while digits < 9 {
        value *= 10;
        digits += 1;
    }
    (value, i)
}

fn num<T: std::str::FromStr>(c: &regex::Captures, i: usize) -> Option<T> {
    c.get(i)?.as_str().parse().ok()
}

/// Byte offset just past a leading timestamp token (including a closing `]`),
/// covering the same formats as [`parse_common`]. Used to strip the timestamp
/// prefix in the aligned column view.
pub fn timestamp_prefix_end(line: &str) -> Option<usize> {
    let end = match scan_prefix(line) {
        Some((_, end)) => end,
        None => DATE_TS
            .captures(line)
            .and_then(|c| c.get(0).map(|m| m.end()))
            .or_else(|| {
                TIME_TS
                    .captures(line)
                    .and_then(|c| c.get(0).map(|m| m.end()))
            })?,
    };
    let mut idx = end;
    while idx < line.len() && line.as_bytes()[idx] == b' ' {
        idx += 1;
    }
    if line[idx..].starts_with(']') {
        Some(idx + 1)
    } else {
        Some(end)
    }
}

fn frac_nanos(s: Option<&str>) -> u32 {
    let Some(s) = s else { return 0 };
    let mut value = 0u32;
    let mut digits = 0u32;
    for b in s.bytes().take(9) {
        value = value * 10 + (b - b'0') as u32;
        digits += 1;
    }
    while digits < 9 {
        value *= 10;
        digits += 1;
    }
    value
}

/// Filled timestamps plus which lines parsed a timestamp of their own.
/// Continuation lines keep `owned == false` so callers can group an event.
pub struct Timeline {
    pub times: Vec<i64>,
    pub owned: Vec<bool>,
}

/// Fills timestamps for every line. Lines without their own timestamp inherit the
/// previous one (multi-line log events); leading lines inherit the first known one.
pub fn build_times(n: usize, parse: impl FnMut(usize) -> Option<i64>) -> Vec<i64> {
    build_timeline(n, parse).times
}

pub fn build_timeline(n: usize, mut parse: impl FnMut(usize) -> Option<i64>) -> Timeline {
    let mut times = vec![UNKNOWN; n];
    let mut owned = vec![false; n];
    for i in 0..n {
        if let Some(stamp) = parse(i) {
            times[i] = stamp;
            owned[i] = true;
        }
    }
    let Some(first) = owned.iter().position(|flag| *flag) else {
        return Timeline {
            times: vec![0; n],
            owned,
        };
    };
    let mut last = times[first];
    for (idx, stamp) in times.iter_mut().enumerate() {
        if owned[idx] {
            last = *stamp;
        } else {
            *stamp = last;
        }
    }
    Timeline { times, owned }
}

/// Absolute instant: `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SS[.fff]`, UTC.
pub fn parse_instant(text: &str) -> Option<i64> {
    let text = text.trim().trim_end_matches('Z');
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_nanos_opt();
    }
    let normalized = text.replace('T', " ");
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(normalized.trim(), fmt) {
            return dt.and_utc().timestamp_nanos_opt();
        }
    }
    None
}

pub fn format_ts(nanos: i64) -> String {
    DateTime::<Utc>::from_timestamp_nanos(nanos)
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

pub const NS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// Parses `HH:MM:SS[.fff]` into nanoseconds since midnight.
pub fn parse_time_of_day(text: &str) -> Option<i64> {
    static TOD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\s*(\d{1,2}):(\d{2}):(\d{2})(?:[.,](\d{1,9}))?\s*$").unwrap()
    });
    let c = TOD.captures(text)?;
    let hours: i64 = num(&c, 1)?;
    let minutes: i64 = num(&c, 2)?;
    let seconds: i64 = num(&c, 3)?;
    if hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }
    Some(
        (hours * 3600 + minutes * 60 + seconds) * 1_000_000_000
            + frac_nanos(c.get(4).map(|m| m.as_str())) as i64,
    )
}

/// Formats nanoseconds since epoch (or since midnight) as `HH:MM:SS.mmm`.
pub fn format_time_of_day(nanos: i64) -> String {
    let ns = nanos.rem_euclid(NS_PER_DAY);
    let total_ms = ns / 1_000_000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        total_ms / 3_600_000,
        total_ms / 60_000 % 60,
        total_ms / 1_000 % 60,
        total_ms % 1_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regex_parse_common(line: &str, day_base: i64) -> Option<i64> {
        if let Some(c) = DATE_TS.captures(line) {
            let date = NaiveDate::from_ymd_opt(num(&c, 1)?, num(&c, 2)?, num(&c, 3)?)?;
            let time = NaiveTime::from_hms_nano_opt(
                num(&c, 4)?,
                num(&c, 5)?,
                num(&c, 6)?,
                frac_nanos(c.get(7).map(|m| m.as_str())),
            )?;
            return date.and_time(time).and_utc().timestamp_nanos_opt();
        }
        if let Some(c) = TIME_TS.captures(line) {
            let time = NaiveTime::from_hms_nano_opt(
                num(&c, 1)?,
                num(&c, 2)?,
                num(&c, 3)?,
                frac_nanos(c.get(4).map(|m| m.as_str())),
            )?;
            let offset =
                time.num_seconds_from_midnight() as i64 * 1_000_000_000 + time.nanosecond() as i64;
            return Some(day_base + offset);
        }
        None
    }

    #[test]
    fn parses_iso_variants() {
        let a = parse_common("2026-10-31 12:34:56.789 hello", 0).unwrap();
        let b = parse_common("[2026-10-31T12:34:56,789] hello", 0).unwrap();
        assert_eq!(a, b);
        assert_eq!(format_ts(a), "2026-10-31 12:34:56.789");
    }

    #[test]
    fn parses_time_only_against_day_base() {
        let day = 1_000_000_000_000_000_000;
        let t = parse_common("[12:00:01.5] boot", day).unwrap();
        assert_eq!(t - day, 12 * 3600 * 1_000_000_000 + 1_500_000_000);
    }

    #[test]
    fn rejects_plain_text() {
        assert_eq!(parse_common("just a message", 0), None);
    }

    #[test]
    fn fast_path_agrees_with_regex_fallback() {
        let day = 1_000_000_000_000_000_000;
        for line in [
            "2026-10-31 12:34:56.789 hello",
            "[2026-10-31T12:34:56,789] hello",
            "2026-10-31 23:59:59.123456789 tail",
            "2026-02-29 10:00:00 leap",
            "2025-02-29 10:00:00 invalid",
            "2026-13-01 10:00:00 bad month",
            "2026-10-31 24:00:00 bad hour",
            "12:00:01.5 boot",
            "[12:00:01] boot",
            "  09:30:00 indented",
            "\u{3000}10:00:00 unicode space",
            "99:99:99 nope",
            "no timestamp",
        ] {
            assert_eq!(
                parse_common(line, day),
                regex_parse_common(line, day),
                "{line:?}"
            );
        }
    }

    #[test]
    fn carries_timestamps_forward() {
        let times = build_times(4, |i| match i {
            0 => Some(10),
            2 => Some(30),
            _ => None,
        });
        assert_eq!(times, vec![10, 10, 30, 30]);
    }

    #[test]
    fn leading_unknowns_use_first_known_time() {
        let times = build_times(3, |i| if i == 1 { Some(5) } else { None });
        assert_eq!(times, vec![5, 5, 5]);
    }

    #[test]
    fn no_timestamps_yields_zeros() {
        assert_eq!(build_times(3, |_| None), vec![0, 0, 0]);
    }

    #[test]
    fn strips_timestamp_prefix() {
        assert_eq!(
            timestamp_prefix_end("2026-10-31 12:34:56.789 hello"),
            Some(23)
        );
        assert_eq!(
            timestamp_prefix_end("[2026-10-31T12:34:56,789] hello"),
            Some(25)
        );
        assert_eq!(timestamp_prefix_end("[12:00:01.5] boot"), Some(12));
        assert_eq!(timestamp_prefix_end("just a message"), None);
    }

    #[test]
    fn time_of_day_roundtrip() {
        let ns = parse_time_of_day("10:00:01.250").unwrap();
        assert_eq!(ns, 10 * 3_600_000_000_000 + 1_250_000_000);
        assert_eq!(format_time_of_day(ns), "10:00:01.250");
        assert_eq!(format_time_of_day(ns + NS_PER_DAY * 3), "10:00:01.250");
    }

    #[test]
    fn time_of_day_rejects_out_of_range() {
        assert_eq!(parse_time_of_day("24:00:00"), None);
        assert_eq!(parse_time_of_day("10:60:00"), None);
        assert_eq!(parse_time_of_day("noon"), None);
    }
}
