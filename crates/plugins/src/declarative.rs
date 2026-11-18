use std::path::{Path, PathBuf};
use std::sync::Arc;

use neolog_core::plugin::{row_style_for, skip_separators, ColumnCell, LineColumns, LogPlugin};
use neolog_core::style::{Rgb, RowStyle, StyledSpan, TextStyle};
use neolog_core::Severity;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

/// A plugin described by JSON. Loaded from `<config>/plugins/*.json` at startup,
/// so a new log format needs no recompilation.
pub struct DeclarativePlugin {
    id: String,
    name: String,
    sniff: Vec<String>,
    sniff_score: f32,
    timestamp_re: Option<Regex>,
    timestamp_mode: TimestampMode,
    sec_group: usize,
    nanos_group: usize,
    severity: Option<Capture>,
    module: Option<Capture>,
    decorate: Vec<DecorateRule>,
    severity_styles: Vec<(Severity, RowStyle)>,
}

struct Capture {
    regex: Regex,
    group: usize,
}

struct DecorateRule {
    regex: Regex,
    style: TextStyle,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Default)]
enum TimestampMode {
    /// Pass the regex match to the generic ISO / `HH:MM:SS` parser.
    #[default]
    #[serde(rename = "common")]
    Common,
    /// `sec` and `nanos` capture groups produce `sec * 1e9 + nanos`.
    #[serde(rename = "epoch")]
    Epoch,
}

#[derive(Deserialize)]
struct PluginConfig {
    id: String,
    name: String,
    #[serde(default)]
    sniff: Vec<String>,
    #[serde(default = "default_sniff_score")]
    sniff_score: f32,
    #[serde(default)]
    timestamp: Option<TimestampConfig>,
    #[serde(default)]
    severity: Option<CaptureConfig>,
    #[serde(default)]
    module: Option<CaptureConfig>,
    #[serde(default)]
    decorate: Vec<DecorateConfig>,
    #[serde(default)]
    severity_styles: Vec<SeverityStyleConfig>,
}

#[derive(Deserialize)]
struct TimestampConfig {
    regex: String,
    #[serde(default)]
    mode: TimestampMode,
    #[serde(default = "one")]
    sec_group: usize,
    #[serde(default = "two")]
    nanos_group: usize,
}

#[derive(Deserialize)]
struct CaptureConfig {
    regex: String,
    #[serde(default = "one")]
    group: usize,
}

#[derive(Deserialize)]
struct DecorateConfig {
    regex: String,
    #[serde(default)]
    fg: Option<[u8; 3]>,
    #[serde(default)]
    bg: Option<[u8; 3]>,
    #[serde(default)]
    bold: bool,
}

#[derive(Deserialize)]
struct SeverityStyleConfig {
    severity: String,
    #[serde(default)]
    fg: Option<[u8; 3]>,
    #[serde(default)]
    bg: Option<[u8; 3]>,
    #[serde(default)]
    bold: bool,
}

pub struct LoadReport {
    pub plugins: Vec<Arc<dyn LogPlugin>>,
    pub errors: Vec<String>,
}

impl DeclarativePlugin {
    pub fn from_json(text: &str) -> Result<Self, String> {
        let config: PluginConfig =
            serde_json::from_str(text).map_err(|error| format!("JSON 解析失败: {error}"))?;
        Self::from_config(config)
    }

    fn from_config(config: PluginConfig) -> Result<Self, String> {
        if config.id.trim().is_empty() || config.name.trim().is_empty() {
            return Err("id 和 name 不能为空".to_string());
        }
        let compile = |pattern: &str| {
            RegexBuilder::new(pattern)
                .build()
                .map_err(|error| format!("正则 {pattern:?} 无效: {error}"))
        };
        let timestamp_re = config
            .timestamp
            .as_ref()
            .map(|ts| compile(&ts.regex))
            .transpose()?;
        let (timestamp_mode, sec_group, nanos_group) = config
            .timestamp
            .as_ref()
            .map(|ts| (ts.mode, ts.sec_group, ts.nanos_group))
            .unwrap_or((TimestampMode::Common, 1, 2));
        let severity = config
            .severity
            .as_ref()
            .map(|capture| {
                Ok::<_, String>(Capture {
                    regex: compile(&capture.regex)?,
                    group: capture.group,
                })
            })
            .transpose()?;
        let module = config
            .module
            .as_ref()
            .map(|capture| {
                Ok::<_, String>(Capture {
                    regex: compile(&capture.regex)?,
                    group: capture.group,
                })
            })
            .transpose()?;
        let mut decorate = Vec::new();
        for rule in config.decorate {
            decorate.push(DecorateRule {
                regex: compile(&rule.regex)?,
                style: TextStyle {
                    fg: rule.fg.map(|[r, g, b]| Rgb(r, g, b)),
                    bg: rule.bg.map(|[r, g, b]| Rgb(r, g, b)),
                    bold: rule.bold,
                },
            });
        }
        let mut severity_styles = Vec::new();
        for style in config.severity_styles {
            let severity = Severity::parse(&style.severity)
                .ok_or_else(|| format!("未知日志级别: {}", style.severity))?;
            severity_styles.push((
                severity,
                RowStyle {
                    bg: style.bg.map(|[r, g, b]| Rgb(r, g, b)),
                    fg: style.fg.map(|[r, g, b]| Rgb(r, g, b)),
                    bold: style.bold,
                },
            ));
        }
        Ok(Self {
            id: config.id,
            name: config.name,
            sniff: config.sniff,
            sniff_score: config.sniff_score.clamp(0.01, 1.0),
            timestamp_re,
            timestamp_mode,
            sec_group,
            nanos_group,
            severity,
            module,
            decorate,
            severity_styles,
        })
    }
}

enum Field {
    Timestamp,
    Level,
    Module,
}

/// Only punctuation may sit between accepted prefix fields; anything else means
/// the next regex match belongs to the message body.
fn prefix_gap(text: &str) -> bool {
    text.chars().all(|ch| {
        ch.is_whitespace()
            || matches!(
                ch,
                ':' | '-' | '|' | ';' | ',' | '[' | ']' | '(' | ')' | '<' | '>'
            )
    })
}

impl LogPlugin for DeclarativePlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn sniff(&self, _path: &Path, sample: &[&str]) -> f32 {
        let hit = self
            .sniff
            .iter()
            .any(|needle| sample.iter().any(|line| line.contains(needle.as_str())));
        if hit {
            self.sniff_score
        } else {
            0.0
        }
    }

    fn timestamp(&self, line: &str, day_base: i64) -> Option<i64> {
        let regex = self.timestamp_re.as_ref()?;
        let captures = regex.captures(line)?;
        match self.timestamp_mode {
            TimestampMode::Common => {
                neolog_core::time::parse_common(captures.get(0)?.as_str(), day_base)
            }
            TimestampMode::Epoch => {
                let secs: i64 = captures.get(self.sec_group)?.as_str().parse().ok()?;
                let nanos = captures
                    .get(self.nanos_group)
                    .map(|m| frac_nanos(m.as_str()))
                    .unwrap_or(0);
                Some(secs * 1_000_000_000 + nanos)
            }
        }
    }

    fn severity(&self, line: &str) -> Option<Severity> {
        let capture = self.severity.as_ref()?;
        let text = capture.regex.captures(line)?.get(capture.group)?.as_str();
        Severity::parse(text.trim())
    }

    fn module<'a>(&self, line: &'a str) -> Option<&'a str> {
        let capture = self.module.as_ref()?;
        capture
            .regex
            .captures(line)?
            .get(capture.group)
            .map(|m| m.as_str())
    }

    fn columns<'a>(&self, line: &'a str) -> Option<LineColumns<'a>> {
        let mut found: Vec<(usize, usize, Field)> = Vec::new();
        if let Some(regex) = &self.timestamp_re {
            if let Some(m) = regex.find(line) {
                found.push((m.start(), m.end(), Field::Timestamp));
            }
        }
        if let Some(capture) = &self.severity {
            if let Some(m) = capture
                .regex
                .captures(line)
                .and_then(|c| c.get(capture.group))
            {
                found.push((m.start(), m.end(), Field::Level));
            }
        }
        if let Some(capture) = &self.module {
            if let Some(m) = capture
                .regex
                .captures(line)
                .and_then(|c| c.get(capture.group))
            {
                found.push((m.start(), m.end(), Field::Module));
            }
        }
        found.sort_by_key(|(start, _, _)| *start);
        let mut level = None;
        let mut module = None;
        let mut cursor = 0usize;
        let mut accepted = false;
        for (start, end, field) in found {
            if start < cursor || !prefix_gap(&line[cursor..start]) {
                break;
            }
            match field {
                Field::Timestamp => {}
                Field::Level => level = Some(ColumnCell::sliced(line, (start, end))),
                Field::Module => module = Some(ColumnCell::sliced(line, (start, end))),
            }
            cursor = end;
            accepted = true;
        }
        if !accepted {
            return None;
        }
        let message_start = skip_separators(line, cursor);
        Some(LineColumns {
            level,
            module,
            message: ColumnCell::sliced(line, (message_start, line.len())),
        })
    }

    fn decorate(&self, line: &str, out: &mut Vec<StyledSpan>) {
        if let Some(capture) = &self.severity {
            if let Some(found) = capture
                .regex
                .captures(line)
                .and_then(|c| c.get(capture.group))
            {
                let mut style = TextStyle {
                    bold: true,
                    ..Default::default()
                };
                if let Some(severity) = Severity::parse(found.as_str().trim()) {
                    style.fg = Some(severity.color());
                }
                out.extend(StyledSpan::new(found.start(), found.end(), style));
            }
        }
        for rule in &self.decorate {
            for found in rule.regex.find_iter(line) {
                out.extend(StyledSpan::new(found.start(), found.end(), rule.style));
            }
        }
    }

    fn row_style(&self, line: &str) -> RowStyle {
        let severity = self.severity(line);
        if let Some((_, style)) = self
            .severity_styles
            .iter()
            .find(|(level, _)| Some(*level) == severity)
        {
            return *style;
        }
        row_style_for(severity)
    }
}

fn frac_nanos(fraction: &str) -> i64 {
    let mut value = 0i64;
    let mut digits = 0;
    for b in fraction.bytes().take(9) {
        if !b.is_ascii_digit() {
            break;
        }
        value = value * 10 + (b - b'0') as i64;
        digits += 1;
    }
    while digits < 9 {
        value *= 10;
        digits += 1;
    }
    value
}

fn default_sniff_score() -> f32 {
    0.75
}

fn one() -> usize {
    1
}

fn two() -> usize {
    2
}

/// Reads `*.json` from a directory. Missing directories are not an error:
/// dynamic plugins are optional.
pub fn load_dir(dir: &Path) -> LoadReport {
    let mut report = LoadReport {
        plugins: Vec::new(),
        errors: Vec::new(),
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return report;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .collect();
    files.sort();
    for path in files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        match std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| DeclarativePlugin::from_json(&text))
        {
            Ok(plugin) => report.plugins.push(Arc::new(plugin)),
            Err(error) => report.errors.push(format!("{name}: {error}")),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"{
      "id": "mylog",
      "name": "My Service",
      "sniff": ["MYLOG"],
      "timestamp": {
        "mode": "common",
        "regex": "\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}\\.\\d{3}"
      },
      "severity": { "regex": "\\b(TRACE|DEBUG|INFO|WARN|ERROR|FATAL)\\b", "group": 1 },
      "module": { "regex": "\\[([a-z_]+)\\]", "group": 1 },
      "decorate": [
        { "regex": "(?i)\\b(timeout|failed)\\b", "fg": [255, 153, 102], "bold": true }
      ],
      "severity_styles": [
        { "severity": "error", "bg": [80, 20, 24], "bold": true }
      ]
    }"#;

    #[test]
    fn parses_fields_and_styles() {
        let plugin = DeclarativePlugin::from_json(CONFIG).unwrap();
        assert_eq!(plugin.id(), "mylog");
        assert_eq!(plugin.sniff(Path::new("x.log"), &["MYLOG 2026-..."]), 0.75);
        assert_eq!(plugin.sniff(Path::new("x.log"), &["other"]), 0.0);
        let line = "2026-10-31T10:00:01.250 INFO [planner] timeout while calling service";
        assert_eq!(plugin.timestamp(line, 0), Some(1_793_440_801_250_000_000));
        assert_eq!(plugin.severity(line), Some(Severity::Info));
        assert_eq!(plugin.module(line), Some("planner"));
        let mut spans = Vec::new();
        plugin.decorate(line, &mut spans);
        assert!(spans.iter().any(|sp| sp.start == 24 && sp.end == 28));
        let error_line = "2026-10-31T10:00:02.000 ERROR [planner] failed";
        assert_eq!(plugin.row_style(error_line).bg, Some(Rgb(80, 20, 24)));
    }

    #[test]
    fn example_plugin_parses() {
        let text = include_str!("../../../examples/plugins/service-log.json");
        let plugin = DeclarativePlugin::from_json(text).unwrap();
        assert_eq!(plugin.id(), "service-log");
        let line = "2026-10-31 10:00:00.123 [INFO] planner: connected to 10.0.0.2";
        assert_eq!(plugin.timestamp(line, 0), Some(1_793_440_800_123_000_000));
        assert_eq!(plugin.severity(line), Some(Severity::Info));
        assert_eq!(plugin.module(line), Some("planner"));
        let mut spans = Vec::new();
        plugin.decorate(line, &mut spans);
        assert!(spans.iter().any(|sp| sp.start == 25 && sp.end == 29));
        let error_line = "2026-10-31 10:00:02.000 [ERROR] lidar: timeout";
        assert_eq!(plugin.row_style(error_line).bg, Some(Rgb(80, 20, 24)));
    }

    #[test]
    fn columns_follow_prefix_order() {
        let plugin = DeclarativePlugin::from_json(CONFIG).unwrap();
        let line = "2026-10-31T10:00:01.250 INFO [planner] timeout while calling service";
        let cols = plugin.columns(line).unwrap();
        assert_eq!(&cols.level.unwrap().text, "INFO");
        let module = cols.module.unwrap();
        assert_eq!(&module.text, "planner");
        assert_eq!(module.range, Some((30, 37)));
        assert_eq!(cols.message.range, Some((39, line.len())));
        assert!(plugin.columns("    at frame loop").is_none());
        // A level word inside the message must not become a column.
        let plain = "2026-10-31T10:00:02.000 worker reported INFO twice";
        let cols = plugin.columns(plain).unwrap();
        assert!(cols.level.is_none());
        assert_eq!(cols.message.text, "worker reported INFO twice");
    }

    #[test]
    fn epoch_mode_and_errors() {
        let config = r#"{
          "id": "e", "name": "E",
          "timestamp": { "mode": "epoch", "regex": "T(\\d+)\\.(\\d+)", "sec_group": 1, "nanos_group": 2 }
        }"#;
        let plugin = DeclarativePlugin::from_json(config).unwrap();
        assert_eq!(
            plugin.timestamp("T1700000000.5 x", 0),
            Some(1_700_000_000_500_000_000)
        );
        assert!(DeclarativePlugin::from_json(
            r#"{ "id": "x", "name": "x", "severity": { "regex": "[" } }"#
        )
        .is_err());
    }
}
