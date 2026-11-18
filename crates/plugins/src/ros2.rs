use std::path::Path;
use std::sync::LazyLock;

use neolog_core::plugin::{
    decorate_severity, row_style_for, skip_separators, ColumnCell, LineColumns, LogPlugin,
};
use neolog_core::style::{Rgb, RowStyle, StyledSpan, TextStyle};
use neolog_core::Severity;
use regex::Regex;

static KEYWORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(timeout|timed out|failed|failure|lost|disconnected|dropped|denied|nan|overflow)\b",
    )
    .unwrap()
});

/// ROS 2 console format:
/// `[ERROR] [1730380801.123456789] [lidar_node]: msg`
pub struct Ros2Plugin;

struct RosPrefix<'a> {
    severity: Severity,
    level_range: (usize, usize),
    secs: &'a str,
    nanos: &'a str,
    module_range: (usize, usize),
    end: usize,
}

impl<'a> RosPrefix<'a> {
    fn module<'s>(&self, line: &'s str) -> &'s str {
        &line[self.module_range.0..self.module_range.1]
    }
}

/// Hand-parses the fixed ROS 2 prefix. Used by every hot path (sniff, timestamp,
/// severity, module, columns, decorate) so a single pass replaces the captures
/// the plugin used to run several times per row.
fn parse_ros(line: &str) -> Option<RosPrefix<'_>> {
    let rest = line.strip_prefix('[')?;
    let level_end = rest.find(']')?;
    if level_end == 0 {
        return None;
    }
    let level = &rest[..level_end];
    if !level.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let severity = Severity::parse(level)?;
    let after_level = 1 + level_end + 1;

    if !line.get(after_level..)?.starts_with(" [") {
        return None;
    }
    let ts_start = after_level + 2;
    let dot = line.get(ts_start..)?.find('.')? + ts_start;
    let secs = &line[ts_start..dot];
    if secs.is_empty() || !secs.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let nanos_end = line.get(dot + 1..)?.find(']')? + dot + 1;
    let nanos = &line[dot + 1..nanos_end];
    if nanos.is_empty() || !nanos.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let after_ts = nanos_end + 1;
    if !line.get(after_ts..)?.starts_with(" [") {
        return None;
    }
    let module_start = after_ts + 2;
    let module_end = line.get(module_start..)?.find(']')? + module_start;
    if module_end == module_start {
        return None;
    }

    Some(RosPrefix {
        severity,
        level_range: (1, 1 + level_end),
        secs,
        nanos,
        module_range: (module_start, module_end),
        end: module_end + 1,
    })
}

impl LogPlugin for Ros2Plugin {
    fn id(&self) -> &str {
        "ros2"
    }

    fn name(&self) -> &str {
        "ROS 2"
    }

    fn sniff(&self, _path: &Path, sample: &[&str]) -> f32 {
        let mut total = 0usize;
        let mut hits = 0usize;
        for line in sample.iter().take(50) {
            if line.trim().is_empty() {
                continue;
            }
            total += 1;
            if parse_ros(line).is_some() {
                hits += 1;
            }
        }
        if total > 0 && hits * 2 >= total {
            0.95
        } else {
            0.0
        }
    }

    fn timestamp(&self, line: &str, _day_base: i64) -> Option<i64> {
        let prefix = parse_ros(line)?;
        let secs: i64 = prefix.secs.parse().ok()?;
        Some(secs.checked_mul(1_000_000_000)? + parse_nanos(prefix.nanos)?)
    }

    fn severity(&self, line: &str) -> Option<Severity> {
        parse_ros(line).map(|prefix| prefix.severity)
    }

    fn module<'a>(&self, line: &'a str) -> Option<&'a str> {
        parse_ros(line).map(|prefix| prefix.module(line))
    }

    fn columns<'a>(&self, line: &'a str) -> Option<LineColumns<'a>> {
        let prefix = parse_ros(line)?;
        let start = skip_separators(line, prefix.end);
        Some(LineColumns {
            level: Some(ColumnCell::sliced(line, prefix.level_range)),
            module: Some(ColumnCell::sliced(line, prefix.module_range)),
            message: ColumnCell::sliced(line, (start, line.len())),
        })
    }

    fn decorate(&self, line: &str, out: &mut Vec<StyledSpan>) {
        if let Some(prefix) = parse_ros(line) {
            let style = TextStyle {
                fg: Some(prefix.severity.color()),
                bold: true,
                ..Default::default()
            };
            out.extend(StyledSpan::new(
                prefix.level_range.0,
                prefix.level_range.1,
                style,
            ));
            out.extend(StyledSpan::new(
                prefix.module_range.0,
                prefix.module_range.1,
                TextStyle {
                    fg: Some(Rgb::new(126, 214, 255)),
                    bold: true,
                    ..Default::default()
                },
            ));
        } else {
            decorate_severity(line, out);
        }
        for m in KEYWORDS.find_iter(line) {
            out.extend(StyledSpan::new(
                m.start(),
                m.end(),
                TextStyle {
                    fg: Some(Rgb::new(255, 153, 102)),
                    bold: true,
                    ..Default::default()
                },
            ));
        }
    }

    fn row_style(&self, line: &str) -> RowStyle {
        let severity = self.severity(line);
        let mut style = row_style_for(severity);
        if matches!(severity, Some(Severity::Error)) {
            style.bg = Some(Rgb::new(80, 20, 24));
        }
        style
    }
}

fn parse_nanos(fraction: &str) -> Option<i64> {
    let mut value = 0i64;
    let mut digits = 0;
    for b in fraction.bytes().take(9) {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (b - b'0') as i64;
        digits += 1;
    }
    while digits < 9 {
        value *= 10;
        digits += 1;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "[ERROR] [1730380801.123456789] [lidar_node]: packet timeout";

    #[test]
    fn parses_ros_prefix_once() {
        let prefix = parse_ros(LINE).unwrap();
        assert_eq!(prefix.severity, Severity::Error);
        assert_eq!(&LINE[prefix.level_range.0..prefix.level_range.1], "ERROR");
        assert_eq!(prefix.module(LINE), "lidar_node");
        assert_eq!(prefix.end, LINE.find("]: ").unwrap() + 1);
    }

    #[test]
    fn rejects_malformed_prefixes() {
        assert!(parse_ros("no prefix").is_none());
        assert!(parse_ros("[error] [1.2] [node]: x").is_none());
        assert!(parse_ros("[BOGUS] [1.2] [node]: x").is_none());
        assert!(parse_ros("[ERROR] [1.2] []: x").is_none());
        assert!(parse_ros("[ERROR] [1.2] [node: x").is_none());
        assert!(parse_ros("[ERROR] [1] [node]: x").is_none());
    }

    #[test]
    fn timestamp_handles_short_fractions() {
        let plugin = Ros2Plugin;
        assert_eq!(
            plugin.timestamp("[WARN] [12.5] [m]: x", 0),
            Some(12_500_000_000)
        );
        assert_eq!(plugin.timestamp("garbage", 0), None);
    }
}
