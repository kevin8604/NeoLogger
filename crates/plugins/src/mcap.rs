use std::path::Path;

use neolog_core::mcap::{self, parse_line};
use neolog_core::plugin::{row_style_for, ColumnCell, LineColumns, LogPlugin};
use neolog_core::style::{Rgb, RowStyle, StyledSpan, TextStyle};
use neolog_core::Severity;

pub struct McapPlugin;

impl LogPlugin for McapPlugin {
    fn id(&self) -> &str {
        "mcap"
    }

    fn name(&self) -> &str {
        "MCAP"
    }

    fn sniff(&self, _path: &Path, sample: &[&str]) -> f32 {
        let mut total = 0usize;
        let mut hits = 0usize;
        for line in sample.iter().take(50) {
            if line.trim().is_empty() {
                continue;
            }
            total += 1;
            if parse_line(line).is_some() {
                hits += 1;
            }
        }
        if total > 0 && hits * 2 >= total {
            0.99
        } else {
            0.0
        }
    }

    fn timestamp(&self, line: &str, _day_base: i64) -> Option<i64> {
        let parsed = parse_line(line)?;
        (parsed.log_ns > 0).then_some(parsed.log_ns)
    }

    fn severity(&self, line: &str) -> Option<Severity> {
        parse_line(line).and_then(|parsed| Severity::parse(parsed.level))
    }

    fn module<'a>(&self, line: &'a str) -> Option<&'a str> {
        parse_line(line).map(|parsed| parsed.topic)
    }

    fn field(&self, line: &str, key: &str) -> Option<String> {
        let parsed = parse_line(line)?;
        mcap::field_of(&parsed, key).map(str::to_string)
    }

    fn field_contains(&self, line: &str, key: &str, needle: &neolog_core::matcher::Needle) -> bool {
        self.field(line, key)
            .is_some_and(|value| needle.contains(&value))
    }

    fn field_eq(&self, line: &str, key: &str, value: &str) -> bool {
        self.field(line, key).is_some_and(|found| found == value)
    }

    fn columns<'a>(&self, line: &'a str) -> Option<LineColumns<'a>> {
        let parsed = parse_line(line)?;
        let message = if parsed.body_range.0 < parsed.body_range.1 {
            ColumnCell::sliced(line, parsed.body_range)
        } else {
            ColumnCell::owned("")
        };
        Some(LineColumns {
            level: Some(ColumnCell::sliced(line, parsed.level_range)),
            module: Some(ColumnCell::sliced(line, parsed.topic_range)),
            message,
        })
    }

    fn decorate(&self, line: &str, out: &mut Vec<StyledSpan>) {
        let Some(parsed) = parse_line(line) else {
            return;
        };
        if let Some(severity) = Severity::parse(parsed.level) {
            out.extend(StyledSpan::new(
                parsed.level_range.0,
                parsed.level_range.1,
                TextStyle {
                    fg: Some(severity.color()),
                    bold: true,
                    ..Default::default()
                },
            ));
        }
        out.extend(StyledSpan::new(
            parsed.topic_range.0,
            parsed.topic_range.1,
            TextStyle {
                fg: Some(Rgb::new(126, 214, 255)),
                bold: true,
                ..Default::default()
            },
        ));
    }

    fn row_style(&self, line: &str) -> RowStyle {
        row_style_for(self.severity(line))
    }
}
