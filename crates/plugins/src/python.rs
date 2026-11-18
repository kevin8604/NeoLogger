use std::path::Path;
use std::sync::LazyLock;

use neolog_core::plugin::{skip_separators, ColumnCell, LineColumns, LogPlugin};
use neolog_core::style::{Rgb, RowStyle, StyledSpan, TextStyle};
use neolog_core::Severity;
use regex::Regex;

static PY_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}[,.]\d{1,9}) - ([^-]+?) - ([A-Za-z]+) - ")
        .unwrap()
});

/// Python `logging` default format:
/// `2026-10-31 10:00:00,123 - camera - ERROR - msg`
pub struct PythonLogPlugin;

impl LogPlugin for PythonLogPlugin {
    fn id(&self) -> &str {
        "python"
    }

    fn name(&self) -> &str {
        "Python logging"
    }

    fn sniff(&self, _path: &Path, sample: &[&str]) -> f32 {
        let mut total = 0usize;
        let mut hits = 0usize;
        for line in sample.iter().take(50) {
            if line.trim().is_empty() {
                continue;
            }
            total += 1;
            if PY_LINE.is_match(line) {
                hits += 1;
            }
        }
        if total > 0 && hits * 2 >= total {
            0.9
        } else {
            0.0
        }
    }

    fn timestamp(&self, line: &str, day_base: i64) -> Option<i64> {
        PY_LINE
            .captures(line)
            .and_then(|c| neolog_core::time::parse_common(c.get(1)?.as_str(), day_base))
    }

    fn severity(&self, line: &str) -> Option<Severity> {
        PY_LINE
            .captures(line)
            .and_then(|c| Severity::parse(c.get(3)?.as_str()))
    }

    fn module<'a>(&self, line: &'a str) -> Option<&'a str> {
        PY_LINE.captures(line)?.get(2).map(|m| m.as_str().trim())
    }

    fn columns<'a>(&self, line: &'a str) -> Option<LineColumns<'a>> {
        let c = PY_LINE.captures(line)?;
        let level = c.get(3)?;
        let module = trim_range(c.get(2)?);
        let start = skip_separators(line, c.get(0)?.end());
        Some(LineColumns {
            level: Some(ColumnCell::sliced(line, (level.start(), level.end()))),
            module: module.map(|range| ColumnCell::sliced(line, range)),
            message: ColumnCell::sliced(line, (start, line.len())),
        })
    }

    fn decorate(&self, line: &str, out: &mut Vec<StyledSpan>) {
        let Some(c) = PY_LINE.captures(line) else {
            return;
        };
        for idx in [2, 3] {
            if let Some(m) = c.get(idx) {
                let style = if idx == 3 {
                    TextStyle {
                        fg: Severity::parse(m.as_str()).map(|s| s.color()),
                        bold: true,
                        ..Default::default()
                    }
                } else {
                    TextStyle {
                        fg: Some(Rgb::new(140, 200, 255)),
                        ..Default::default()
                    }
                };
                out.extend(StyledSpan::new(m.start(), m.end(), style));
            }
        }
    }

    fn row_style(&self, line: &str) -> RowStyle {
        neolog_core::plugin::row_style_for(self.severity(line))
    }
}

fn trim_range(found: regex::Match<'_>) -> Option<(usize, usize)> {
    let text = found.as_str();
    let start = found.start() + (text.len() - text.trim_start().len());
    let end = found.end() - (text.len() - text.trim_end().len());
    (start < end).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_reuse_source_bytes() {
        let plugin = PythonLogPlugin;
        let line = "2026-10-31 10:00:00,123 - camera - ERROR - frame dropped";
        let cols = plugin.columns(line).unwrap();
        assert_eq!(cols.level.unwrap().text, "ERROR");
        let module = cols.module.unwrap();
        assert_eq!(&module.text, "camera");
        assert_eq!(
            &line[module.range.unwrap().0..module.range.unwrap().1],
            "camera"
        );
        assert_eq!(cols.message.text, "frame dropped");
        assert_eq!(cols.message.range, Some((43, line.len())));
        assert!(plugin.columns("plain continuation").is_none());
    }
}
