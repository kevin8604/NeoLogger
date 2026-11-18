use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use crate::matcher::Needle;
use crate::style::{Rgb, RowStyle, StyledSpan, TextStyle};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Severity {
    pub fn parse(text: &str) -> Option<Self> {
        let s = text.trim();
        if s.eq_ignore_ascii_case("trace") {
            Some(Self::Trace)
        } else if s.eq_ignore_ascii_case("debug") {
            Some(Self::Debug)
        } else if s.eq_ignore_ascii_case("info") || s.eq_ignore_ascii_case("notice") {
            Some(Self::Info)
        } else if s.eq_ignore_ascii_case("warn") || s.eq_ignore_ascii_case("warning") {
            Some(Self::Warn)
        } else if s.eq_ignore_ascii_case("error") || s.eq_ignore_ascii_case("err") {
            Some(Self::Error)
        } else if s.eq_ignore_ascii_case("fatal")
            || s.eq_ignore_ascii_case("critical")
            || s.eq_ignore_ascii_case("crit")
        {
            Some(Self::Fatal)
        } else {
            None
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Fatal => "FATAL",
        }
    }

    pub fn next(self) -> Option<Self> {
        match self {
            Self::Trace => Some(Self::Debug),
            Self::Debug => Some(Self::Info),
            Self::Info => Some(Self::Warn),
            Self::Warn => Some(Self::Error),
            Self::Error => Some(Self::Fatal),
            Self::Fatal => None,
        }
    }

    pub fn color(self) -> Rgb {
        match self {
            Self::Trace => Rgb::new(150, 150, 150),
            Self::Debug => Rgb::new(178, 178, 178),
            Self::Info => Rgb::new(120, 190, 255),
            Self::Warn => Rgb::new(255, 196, 87),
            Self::Error => Rgb::new(255, 110, 110),
            Self::Fatal => Rgb::new(255, 80, 140),
        }
    }
}

/// Walks every word token and reports severity keywords. A token match is
/// equivalent to the old `(?i)\b(keyword)\b` regex because every keyword is a
/// complete word, and is roughly an order of magnitude cheaper on the hot path.
fn scan_severities(line: &str, mut visit: impl FnMut(usize, usize, Severity) -> bool) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !b.is_ascii() {
            i += line[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if !b.is_ascii_alphabetic() || (i > 0 && prev_is_word(line, i)) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii() {
                if c.is_ascii_alphanumeric() || c == b'_' {
                    i += 1;
                } else {
                    break;
                }
            } else {
                match line[i..].chars().next() {
                    Some(ch) if is_word_char(ch) => i += ch.len_utf8(),
                    _ => break,
                }
            }
        }
        if let Some(sev) = Severity::parse(&line[start..i]) {
            if visit(start, i, sev) {
                return true;
            }
        }
    }
    false
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn prev_is_word(line: &str, i: usize) -> bool {
    let b = line.as_bytes()[i - 1];
    if b.is_ascii() {
        return b.is_ascii_alphanumeric() || b == b'_';
    }
    let mut start = i - 1;
    while start > 0 && !line.is_char_boundary(start) {
        start -= 1;
    }
    line[start..i].chars().next_back().is_some_and(is_word_char)
}

pub fn detect_severity(line: &str) -> Option<Severity> {
    let mut found = None;
    scan_severities(line, |_, _, sev| {
        found = Some(sev);
        true
    });
    found
}

pub fn decorate_severity(line: &str, out: &mut Vec<StyledSpan>) {
    scan_severities(line, |start, end, sev| {
        let style = TextStyle {
            fg: Some(sev.color()),
            bold: matches!(sev, Severity::Error | Severity::Fatal),
            ..Default::default()
        };
        if let Some(span) = StyledSpan::new(start, end, style) {
            out.push(span);
        }
        false
    });
}

pub fn row_style_for(sev: Option<Severity>) -> RowStyle {
    match sev {
        Some(Severity::Fatal) => RowStyle {
            bg: Some(Rgb::new(94, 18, 32)),
            fg: Some(Rgb::new(255, 214, 224)),
            bold: true,
        },
        Some(Severity::Error) => RowStyle {
            bg: Some(Rgb::new(66, 22, 26)),
            fg: None,
            bold: true,
        },
        Some(Severity::Warn) => RowStyle {
            bg: Some(Rgb::new(56, 48, 16)),
            fg: None,
            bold: false,
        },
        Some(Severity::Debug) => RowStyle {
            bg: None,
            fg: Some(Rgb::new(160, 160, 160)),
            bold: false,
        },
        Some(Severity::Trace) => RowStyle {
            bg: None,
            fg: Some(Rgb::new(130, 130, 130)),
            bold: false,
        },
        _ => RowStyle::default(),
    }
}

/// One cell of the aligned column view. `range` points into the source line when
/// the text is a contiguous slice, which keeps rule styling, find highlighting
/// and selection working on the original bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnCell<'a> {
    pub text: Cow<'a, str>,
    pub range: Option<(usize, usize)>,
}

impl<'a> ColumnCell<'a> {
    pub fn sliced(line: &'a str, range: (usize, usize)) -> Self {
        Self {
            text: Cow::Borrowed(&line[range.0..range.1]),
            range: Some(range),
        }
    }

    pub fn owned(text: impl Into<String>) -> Self {
        Self {
            text: Cow::Owned(text.into()),
            range: None,
        }
    }
}

/// Split of a structured line for the column view. The timestamp column is
/// filled by the UI from the parsed line time, so plugins only describe level,
/// module and the remaining message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineColumns<'a> {
    pub level: Option<ColumnCell<'a>>,
    pub module: Option<ColumnCell<'a>>,
    pub message: ColumnCell<'a>,
}

/// Skips the punctuation and whitespace that separate prefix fields from the
/// message (`": "`, `" - "`, `"| "` …).
pub fn skip_separators(line: &str, mut idx: usize) -> usize {
    while idx < line.len() {
        let ch = line[idx..].chars().next().unwrap_or(' ');
        if ch.is_whitespace() || matches!(ch, ':' | '-' | '|' | ';' | ',' | ']' | ')') {
            idx += ch.len_utf8();
        } else {
            break;
        }
    }
    idx
}

pub trait LogPlugin: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;

    fn sniff(&self, _path: &Path, _sample: &[&str]) -> f32 {
        0.0
    }

    fn timestamp(&self, line: &str, day_base: i64) -> Option<i64> {
        crate::time::parse_common(line, day_base)
    }

    fn severity(&self, line: &str) -> Option<Severity> {
        detect_severity(line)
    }

    fn module<'a>(&self, _line: &'a str) -> Option<&'a str> {
        None
    }

    /// Splits a structured line into aligned column cells. Returning `None`
    /// means the line has no recognizable prefix (continuation lines), and the
    /// row renders as a single message cell.
    fn columns<'a>(&self, _line: &'a str) -> Option<LineColumns<'a>> {
        None
    }

    /// Structured field lookup. The default understands `key=value` tokens
    /// (logfmt style); structured plugins can override it.
    fn field(&self, line: &str, key: &str) -> Option<String> {
        kv_field(line, key)
    }

    /// Allocation-free field predicate used by the `field:` filter. Plugins
    /// that override [`LogPlugin::field`] should override this to keep the same
    /// semantics.
    fn field_contains(&self, line: &str, key: &str, needle: &Needle) -> bool {
        kv_field_ref(line, key).is_some_and(|value| needle.contains(value))
    }

    /// Exact field value. `field:key=value` uses this; `~=` keeps substring matching.
    fn field_eq(&self, line: &str, key: &str, value: &str) -> bool {
        self.field(line, key).is_some_and(|found| found == value)
    }

    fn decorate(&self, line: &str, out: &mut Vec<StyledSpan>) {
        decorate_severity(line, out);
    }

    fn row_style(&self, line: &str) -> RowStyle {
        row_style_for(self.severity(line))
    }
}

pub struct PlainPlugin;

impl LogPlugin for PlainPlugin {
    fn id(&self) -> &str {
        "plain"
    }

    fn name(&self) -> &str {
        "纯文本"
    }
}

/// Finds `key=value` in a logfmt-style line and returns the unquoted value.
/// Quoted values may contain spaces.
pub fn kv_field(line: &str, key: &str) -> Option<String> {
    kv_field_ref(line, key).map(str::to_string)
}

/// Borrowing variant of [`kv_field`] for allocation-free per-row filtering.
pub fn kv_field_ref<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    if key.is_empty() {
        return None;
    }
    let bytes = line.as_bytes();
    if memchr::memchr(b'=', bytes).is_none() {
        return None;
    }
    // Logfmt lines are usually plain ASCII without quotes, where tokens are
    // just runs between whitespace/commas: jump between `=` delimiters instead
    // of walking every character of every token.
    if line.is_ascii() && memchr::memchr2(b'"', b'\'', bytes).is_none() {
        return kv_field_plain(line, key);
    }
    // Quoted or non-ASCII lines keep the quote-aware tokenizer, but a line that
    // does not contain the key text at all cannot match either way.
    if crate::matcher::contains_ascii_ci(bytes, key.as_bytes()).is_none() {
        return None;
    }
    kv_field_tokens(line, key)
}

fn kv_field_plain<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let bytes = line.as_bytes();
    for eq in memchr::memchr_iter(b'=', bytes) {
        let mut start = eq;
        while start > 0 && !is_kv_separator(bytes[start - 1]) {
            start -= 1;
        }
        if line[start..eq].eq_ignore_ascii_case(key) {
            let mut end = eq + 1;
            while end < bytes.len() && !is_kv_separator(bytes[end]) {
                end += 1;
            }
            return Some(trim_kv_value(&line[eq + 1..end]));
        }
    }
    None
}

fn kv_field_tokens<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    for token in kv_tokens(line) {
        let Some((name, value)) = token.split_once('=') else {
            continue;
        };
        let name = name.trim_matches('"');
        if !name.eq_ignore_ascii_case(key) {
            continue;
        }
        return Some(trim_kv_value(value));
    }
    None
}

fn trim_kv_value(value: &str) -> &str {
    value
        .trim_matches(|c: char| matches!(c, ',' | ';' | ')' | ']' | '}'))
        .trim_matches('"')
        .trim_matches('\'')
}

/// ASCII whitespace exactly as `char::is_whitespace` classifies it (0x09–0x0D,
/// space), or the comma token separator.
fn is_kv_separator(b: u8) -> bool {
    matches!(b, b' ' | 0x09..=0x0D | b',')
}

struct KvTokens<'a> {
    line: &'a str,
    pos: usize,
    start: usize,
    quote: Option<char>,
}

impl<'a> Iterator for KvTokens<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let bytes = self.line.as_bytes();
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            let (ch, next) = if b.is_ascii() {
                (b as char, self.pos + 1)
            } else {
                let ch = self.line[self.pos..].chars().next()?;
                (ch, self.pos + ch.len_utf8())
            };
            match self.quote {
                Some(q) => {
                    if ch == q {
                        self.quote = None;
                    }
                }
                None if ch == '"' || ch == '\'' => self.quote = Some(ch),
                None if ch.is_whitespace() || ch == ',' => {
                    if self.start < self.pos {
                        let token = &self.line[self.start..self.pos];
                        self.start = next;
                        self.pos = next;
                        return Some(token);
                    }
                    self.start = next;
                }
                None => {}
            }
            self.pos = next;
        }
        if self.start < self.line.len() {
            let token = &self.line[self.start..];
            self.start = self.line.len();
            return Some(token);
        }
        None
    }
}

fn kv_tokens(line: &str) -> KvTokens<'_> {
    KvTokens {
        line,
        pos: 0,
        start: 0,
        quote: None,
    }
}

#[derive(Clone)]
pub struct PluginRegistry {
    plugins: Vec<Arc<dyn LogPlugin>>,
    fallback: Arc<dyn LogPlugin>,
}

impl PluginRegistry {
    pub fn new(plugins: Vec<Arc<dyn LogPlugin>>) -> Self {
        Self {
            plugins,
            fallback: Arc::new(PlainPlugin),
        }
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn LogPlugin>> {
        self.plugins.iter()
    }

    pub fn fallback(&self) -> Arc<dyn LogPlugin> {
        self.fallback.clone()
    }

    pub fn by_id(&self, id: &str) -> Arc<dyn LogPlugin> {
        self.plugins
            .iter()
            .find(|p| p.id() == id)
            .cloned()
            .unwrap_or_else(|| self.fallback())
    }

    /// Borrowing lookup for hot paths that cannot afford a refcount bump.
    pub fn get(&self, id: &str) -> &dyn LogPlugin {
        self.plugins
            .iter()
            .find(|p| p.id() == id)
            .map_or_else(|| self.fallback.as_ref(), |p| p.as_ref())
    }

    pub fn fallback_ref(&self) -> &dyn LogPlugin {
        self.fallback.as_ref()
    }

    pub fn detect(&self, path: &Path, sample: &[&str]) -> Arc<dyn LogPlugin> {
        let mut best: Option<(f32, &Arc<dyn LogPlugin>)> = None;
        for plugin in &self.plugins {
            let score = plugin.sniff(path, sample);
            if score > 0.0 && best.map_or(true, |(b, _)| score > b) {
                best = Some((score, plugin));
            }
        }
        best.map(|(_, p)| p.clone())
            .unwrap_or_else(|| self.fallback())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_detection_is_word_bounded() {
        assert_eq!(detect_severity("[ERROR] boom"), Some(Severity::Error));
        assert_eq!(detect_severity("terrifying"), None);
    }

    #[test]
    fn severity_scan_matches_whole_words_only() {
        assert_eq!(detect_severity("x y critical"), Some(Severity::Fatal));
        assert_eq!(detect_severity("errno=2"), None);
        assert_eq!(detect_severity("error2"), None);
        assert_eq!(detect_severity("错误error"), None);
        assert_eq!(detect_severity("（warning）"), Some(Severity::Warn));
        assert_eq!(detect_severity("notice"), Some(Severity::Info));
    }

    #[test]
    fn decorate_reports_every_keyword_span() {
        let mut spans = Vec::new();
        decorate_severity("warn then error", &mut spans);
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].start, spans[0].end), (0, 4));
        assert_eq!((spans[1].start, spans[1].end), (10, 15));
    }

    #[test]
    fn error_gets_row_background() {
        let style = row_style_for(Some(Severity::Error));
        assert!(style.bg.is_some());
        assert!(style.bold);
    }

    #[test]
    fn registry_falls_back_to_plain() {
        let reg = PluginRegistry::new(vec![]);
        assert_eq!(reg.by_id("nope").id(), "plain");
        assert_eq!(reg.detect(Path::new("a.log"), &["hello"]).id(), "plain");
    }

    #[test]
    fn kv_fields_are_case_insensitive_and_unquoted() {
        let line = r#"ts=1 level=ERROR module="lidar driver" msg=timeout,"#;
        assert_eq!(kv_field(line, "level"), Some("ERROR".to_string()));
        assert_eq!(kv_field(line, "MODULE"), Some("lidar driver".to_string()));
        assert_eq!(kv_field(line, "msg"), Some("timeout".to_string()));
        assert_eq!(kv_field(line, "missing"), None);
    }

    #[test]
    fn kv_plain_scan_matches_tokenizer() {
        let cases = [
            ("a=1 level=INFO x=y", "level"),
            ("a=1 level=INFO x=y", "A"),
            ("foo=bar=baz level=WARN", "level"),
            ("foo=level=INFO", "level"),
            ("level=INFO level=WARN", "level"),
            ("just text without fields", "level"),
            ("level = INFO", "level"),
            ("x,level=INFO,y", "level"),
            ("level=INFO\r", "level"),
            ("empty= level=INFO", "level"),
            ("\u{b}foo=1 level=INFO", "level"),
            ("cut=INFO) level=INFO", "level"),
        ];
        for (line, key) in cases {
            assert_eq!(
                kv_field_ref(line, key),
                kv_field_tokens(line, key),
                "line={line:?} key={key:?}"
            );
        }
    }

    #[test]
    fn kv_quoted_and_unicode_lines_keep_tokenizer_semantics() {
        assert_eq!(
            kv_field_ref(r#"msg="a level=INFO" level=WARN"#, "level"),
            Some("WARN")
        );
        assert_eq!(kv_field_ref("level\u{3000}=INFO", "level"), None);
        assert_eq!(kv_field_ref("no tokens here", "level"), None);
    }
}
