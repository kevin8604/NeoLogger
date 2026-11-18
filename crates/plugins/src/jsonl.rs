use std::path::Path;
use std::sync::LazyLock;

use neolog_core::matcher::Needle;
use neolog_core::plugin::{decorate_severity, row_style_for, ColumnCell, LineColumns, LogPlugin};
use neolog_core::style::{RowStyle, StyledSpan};
use neolog_core::Severity;
use regex::Regex;
use serde_json::Value;

static MODULE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(?:module|target|logger|component|name)"\s*:\s*"([^"]*)""#).unwrap()
});

const TIME_KEYS: [&str; 4] = ["timestamp", "time", "ts", "@timestamp"];
const LEVEL_KEYS: [&str; 4] = ["level", "severity", "lvl", "log.level"];

/// Newline-delimited JSON logs:
/// `{"time":1730380800123,"level":"ERROR","module":"lidar","msg":"timeout"}`
pub struct JsonLinesPlugin;

impl LogPlugin for JsonLinesPlugin {
    fn id(&self) -> &str {
        "jsonl"
    }

    fn name(&self) -> &str {
        "JSON Lines"
    }

    fn sniff(&self, _path: &Path, sample: &[&str]) -> f32 {
        let mut total = 0usize;
        let mut hits = 0usize;
        for line in sample.iter().take(50) {
            if line.trim().is_empty() {
                continue;
            }
            total += 1;
            if parse(line).is_some_and(|value| value.is_object()) {
                hits += 1;
            }
        }
        if hits > 0 && hits * 2 >= total {
            0.9
        } else {
            0.0
        }
    }

    fn timestamp(&self, line: &str, day_base: i64) -> Option<i64> {
        let value = parse(line)?;
        for key in TIME_KEYS {
            let Some(field) = value.get(key) else {
                continue;
            };
            if let Some(n) = field.as_i64() {
                return Some(normalize_epoch(n));
            }
            if let Some(f) = field.as_f64() {
                return Some(normalize_epoch_f(f));
            }
            if let Some(text) = field.as_str() {
                if let Ok(number) = text.trim().parse::<f64>() {
                    if text.trim().parse::<i64>().is_ok() {
                        return Some(normalize_epoch(text.trim().parse().ok()?));
                    }
                    return Some(normalize_epoch_f(number));
                }
                if let Some(n) = neolog_core::time::parse_common(text, day_base) {
                    return Some(n);
                }
            }
        }
        None
    }

    fn severity(&self, line: &str) -> Option<Severity> {
        let value = parse(line)?;
        for key in LEVEL_KEYS {
            let Some(field) = lookup(&value, key) else {
                continue;
            };
            if let Some(text) = field.as_str() {
                if let Some(sev) = Severity::parse(text) {
                    return Some(sev);
                }
            }
        }
        None
    }

    fn module<'a>(&self, line: &'a str) -> Option<&'a str> {
        MODULE_RE.captures(line)?.get(1).map(|m| m.as_str())
    }

    fn columns<'a>(&self, line: &'a str) -> Option<LineColumns<'a>> {
        if !parse(line)?.is_object() {
            return None;
        }
        let level = self
            .severity(line)
            .map(|severity| ColumnCell::owned(severity.label()));
        let module = MODULE_RE
            .captures(line)
            .and_then(|c| c.get(1))
            .map(|m| ColumnCell::sliced(line, (m.start(), m.end())));
        Some(LineColumns {
            level,
            module,
            message: ColumnCell::sliced(line, (0, line.len())),
        })
    }

    fn field(&self, line: &str, key: &str) -> Option<String> {
        let value = parse(line)?;
        match lookup(&value, key)? {
            Value::String(text) => Some(text.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        }
    }

    fn field_contains(&self, line: &str, key: &str, needle: &Needle) -> bool {
        self.field(line, key)
            .is_some_and(|value| needle.contains(&value))
    }

    fn decorate(&self, line: &str, out: &mut Vec<StyledSpan>) {
        decorate_severity(line, out);
    }

    fn row_style(&self, line: &str) -> RowStyle {
        row_style_for(self.severity(line))
    }
}

fn parse(line: &str) -> Option<Value> {
    serde_json::from_str(line.trim()).ok()
}

fn lookup<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(found) = value.get(key) {
        return Some(found);
    }
    let mut cursor = value;
    for part in key.split('.') {
        cursor = cursor.get(part)?;
    }
    Some(cursor)
}

fn normalize_epoch(value: i64) -> i64 {
    let abs = value.abs();
    if abs < 100_000_000_000 {
        value * 1_000_000_000
    } else if abs < 100_000_000_000_000 {
        value * 1_000_000
    } else if abs < 100_000_000_000_000_000 {
        value * 1_000
    } else {
        value
    }
}

fn normalize_epoch_f(value: f64) -> i64 {
    let abs = value.abs();
    let scale = if abs < 1e11 {
        1e9
    } else if abs < 1e14 {
        1e6
    } else if abs < 1e17 {
        1e3
    } else {
        1.0
    };
    (value * scale) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"time":1730380800123,"level":"ERROR","module":"lidar","msg":"timeout","latency_ms":12.5}"#;

    #[test]
    fn sniffs_json_lines() {
        let plugin = JsonLinesPlugin;
        assert_eq!(
            plugin.sniff(Path::new("a.jsonl"), &[LINE, r#"{"a":1}"#]),
            0.9
        );
        assert_eq!(plugin.sniff(Path::new("a.log"), &["plain text"]), 0.0);
    }

    #[test]
    fn parses_fields_and_epoch_millis() {
        let plugin = JsonLinesPlugin;
        assert_eq!(plugin.timestamp(LINE, 0), Some(1_730_380_800_123_000_000));
        assert_eq!(plugin.severity(LINE), Some(Severity::Error));
        assert_eq!(plugin.module(LINE), Some("lidar"));
        assert_eq!(plugin.field(LINE, "msg"), Some("timeout".to_string()));
        assert_eq!(plugin.field(LINE, "latency_ms"), Some("12.5".to_string()));
        assert_eq!(plugin.field(LINE, "missing"), None);
    }

    #[test]
    fn columns_use_severity_label_and_raw_message() {
        let plugin = JsonLinesPlugin;
        let cols = plugin.columns(LINE).unwrap();
        assert_eq!(cols.level.unwrap().text, "ERROR");
        let module = cols.module.unwrap();
        assert_eq!(&module.text, "lidar");
        assert_eq!(cols.message.text, LINE);
        assert_eq!(cols.message.range, Some((0, LINE.len())));
        assert!(plugin.columns("[1,2,3]").is_none());
    }

    #[test]
    fn parses_iso_string_and_nested_fields() {
        let plugin = JsonLinesPlugin;
        let line =
            r#"{"timestamp":"2026-10-31 10:00:00.123","level":"info","ctx":{"node":"camera"}}"#;
        assert_eq!(plugin.timestamp(line, 0), Some(1_793_440_800_123_000_000));
        assert_eq!(plugin.field(line, "ctx.node"), Some("camera".to_string()));
    }
}
