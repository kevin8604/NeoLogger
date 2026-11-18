use std::path::Path;

use neolog_core::plugin::{skip_separators, ColumnCell, LineColumns, LogPlugin};
use neolog_core::style::{RowStyle, StyledSpan};
use neolog_core::Severity;

/// Fallback for plain text logs: still colors quoted severity words and parses the
/// common `YYYY-MM-DD HH:MM:SS` / `HH:MM:SS` timestamp prefixes.
pub struct GenericPlugin;

impl LogPlugin for GenericPlugin {
    fn id(&self) -> &str {
        "generic"
    }

    fn name(&self) -> &str {
        "通用日志"
    }

    fn sniff(&self, _path: &Path, sample: &[&str]) -> f32 {
        let mut total = 0usize;
        let mut hits = 0usize;
        for line in sample.iter().take(50) {
            if line.trim().is_empty() {
                continue;
            }
            total += 1;
            if neolog_core::time::parse_common(line, 0).is_some() {
                hits += 1;
            }
        }
        if total == 0 {
            0.0
        } else if hits * 2 >= total {
            0.35
        } else {
            0.05
        }
    }

    fn severity(&self, line: &str) -> Option<Severity> {
        neolog_core::plugin::detect_severity(line)
    }

    fn columns<'a>(&self, line: &'a str) -> Option<LineColumns<'a>> {
        let end = neolog_core::time::timestamp_prefix_end(line)?;
        let start = skip_separators(line, end);
        Some(LineColumns {
            level: None,
            module: None,
            message: ColumnCell::sliced(line, (start, line.len())),
        })
    }

    fn decorate(&self, line: &str, out: &mut Vec<StyledSpan>) {
        neolog_core::plugin::decorate_severity(line, out);
    }

    fn row_style(&self, line: &str) -> RowStyle {
        neolog_core::plugin::row_style_for(self.severity(line))
    }
}
