use regex::{Regex, RegexBuilder};

use crate::matcher::{regex_prefilter, Needle, RegexPrefilter};
use crate::plugin::{LogPlugin, Severity};

pub struct MatchCtx<'a> {
    pub text: &'a str,
    pub plugin: &'a dyn LogPlugin,
    pub time: i64,
    pub bookmarked: bool,
}

#[derive(Debug, Clone)]
pub enum Filter {
    Contains(Needle),
    Regex {
        re: Regex,
        prefilter: Option<RegexPrefilter>,
    },
    LevelAtLeast(Severity),
    Level(Severity),
    Module(Needle),
    Field {
        key: String,
        needle: Needle,
        exact: bool,
    },
    /// Time-of-day or absolute window. `wrap` is only meaningful for time-of-day.
    TimeRange {
        start: i64,
        end: i64,
        absolute: bool,
        wrap: bool,
    },
    Bookmarked,
    /// Same trace / request / correlation id, matched across files.
    Id(Needle),
    Not(Box<Filter>),
}

impl Filter {
    /// Whether evaluating this filter reads the bookmark set. Bookmark lookups
    /// are skipped entirely for the common level/text filters.
    pub fn uses_bookmarks(&self) -> bool {
        match self {
            Filter::Bookmarked => true,
            Filter::Not(inner) => inner.uses_bookmarks(),
            _ => false,
        }
    }

    pub fn matches(&self, cx: &MatchCtx) -> bool {
        match self {
            Filter::Contains(n) => n.contains(cx.text),
            Filter::Regex { re, prefilter } => {
                prefilter.as_ref().map_or(true, |p| p.accepts(cx.text)) && re.is_match(cx.text)
            }
            Filter::LevelAtLeast(sev) => cx.plugin.severity(cx.text).is_some_and(|s| s >= *sev),
            Filter::Level(sev) => cx.plugin.severity(cx.text) == Some(*sev),
            Filter::Module(m) => cx
                .plugin
                .module(cx.text)
                .is_some_and(|module| m.contains(module)),
            Filter::Field { key, needle, exact } => {
                if *exact {
                    cx.plugin.field_eq(cx.text, key, needle.raw())
                } else {
                    cx.plugin.field_contains(cx.text, key, needle)
                }
            }
            Filter::TimeRange {
                start,
                end,
                absolute,
                wrap,
            } => {
                cx.time > 0
                    && if *absolute {
                        *start <= cx.time && cx.time < *end
                    } else {
                        let tod = cx.time.rem_euclid(crate::time::NS_PER_DAY);
                        if *wrap {
                            tod >= *start || tod < *end
                        } else {
                            *start <= tod && tod < *end
                        }
                    }
            }
            Filter::Bookmarked => cx.bookmarked,
            Filter::Id(needle) => crate::correlate::line_has_id(cx.plugin, cx.text, needle.raw()),
            Filter::Not(inner) => !inner.matches(cx),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Filter::Contains(n) => n.raw().to_string(),
            Filter::Regex { re, .. } => format!("/{}/", re.as_str()),
            Filter::LevelAtLeast(s) => format!("level>={}", s.label()),
            Filter::Level(s) => format!("level={}", s.label()),
            Filter::Module(m) => format!("module:{}", m.raw()),
            Filter::Field { key, needle, exact } => {
                if *exact {
                    format!("field:{key}={}", needle.raw())
                } else {
                    format!("field:{key}~={}", needle.raw())
                }
            }
            Filter::TimeRange {
                start,
                end,
                absolute,
                wrap,
            } => {
                if *absolute {
                    format!("time:{start}..{end}")
                } else if *wrap {
                    format!(
                        "time:{}..{} (跨午夜)",
                        crate::time::format_time_of_day(*start),
                        crate::time::format_time_of_day(*end)
                    )
                } else {
                    format!(
                        "time:{}..{}",
                        crate::time::format_time_of_day(*start),
                        crate::time::format_time_of_day(*end)
                    )
                }
            }
            Filter::Bookmarked => "bm:".to_string(),
            Filter::Id(needle) => format!("id:{}", needle.raw()),
            Filter::Not(f) => format!("!{}", f.describe()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Expr {
    True,
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Atom(Filter),
}

impl Expr {
    pub fn matches(&self, cx: &MatchCtx) -> bool {
        match self {
            Expr::True => true,
            Expr::And(items) => items.iter().all(|item| item.matches(cx)),
            Expr::Or(items) => items.iter().any(|item| item.matches(cx)),
            Expr::Not(inner) => !inner.matches(cx),
            Expr::Atom(filter) => filter.matches(cx),
        }
    }

    pub fn uses_bookmarks(&self) -> bool {
        match self {
            Expr::True => false,
            Expr::And(items) | Expr::Or(items) => items.iter().any(Expr::uses_bookmarks),
            Expr::Not(inner) => inner.uses_bookmarks(),
            Expr::Atom(filter) => filter.uses_bookmarks(),
        }
    }
}

/// A parsed filter. `around` is extra parent rows kept before and after each hit.
/// Hits also keep the rest of their multi-line event; that expansion lives in the session.
#[derive(Debug, Clone)]
pub struct Query {
    pub expr: Expr,
    pub around: usize,
}

impl Query {
    pub fn all() -> Self {
        Self {
            expr: Expr::True,
            around: 0,
        }
    }

    pub fn is_all(&self) -> bool {
        matches!(self.expr, Expr::True)
    }

    pub fn matches(&self, cx: &MatchCtx) -> bool {
        self.expr.matches(cx)
    }

    pub fn uses_bookmarks(&self) -> bool {
        self.expr.uses_bookmarks()
    }

    pub fn only_atom(&self) -> Option<&Filter> {
        match &self.expr {
            Expr::Atom(filter) => Some(filter),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum Tok {
    Term(String),
    Or,
    And,
    Not,
    LParen,
    RParen,
}

pub fn parse_query(query: &str) -> Result<Query, String> {
    let tokens = tokenize(query)?;
    let mut around = 0usize;
    let mut filtered = Vec::new();
    for token in tokens {
        if let Tok::Term(term) = &token {
            if let Some(rest) = term
                .strip_prefix("around:")
                .or_else(|| term.strip_prefix("context:"))
                .or_else(|| term.strip_prefix("ctx:"))
            {
                around = rest
                    .parse::<usize>()
                    .map_err(|_| format!("上下文行数无效: {rest}"))?;
                continue;
            }
        }
        filtered.push(token);
    }
    if filtered.is_empty() {
        return Ok(Query {
            expr: Expr::True,
            around,
        });
    }
    let mut parser = Parser {
        tokens: filtered,
        pos: 0,
    };
    let expr = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err("过滤表达式有多余内容".to_string());
    }
    Ok(Query { expr, around })
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut items = vec![self.parse_and()?];
        while self.eat_or() {
            items.push(self.parse_and()?);
        }
        if items.len() == 1 {
            Ok(items.pop().unwrap())
        } else {
            Ok(Expr::Or(items))
        }
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut items = vec![self.parse_unary()?];
        while self.peek_continues_and() {
            if matches!(self.tokens.get(self.pos), Some(Tok::And)) {
                self.pos += 1;
            }
            items.push(self.parse_unary()?);
        }
        if items.len() == 1 {
            Ok(items.pop().unwrap())
        } else {
            Ok(Expr::And(items))
        }
    }

    fn peek_continues_and(&self) -> bool {
        matches!(
            self.tokens.get(self.pos),
            Some(Tok::Term(_) | Tok::Not | Tok::LParen | Tok::And)
        )
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        if matches!(self.tokens.get(self.pos), Some(Tok::Not)) {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        if let Some(Tok::Term(term)) = self.tokens.get(self.pos) {
            if let Some(rest) = term.strip_prefix('!') {
                if !rest.is_empty() {
                    self.pos += 1;
                    return Ok(Expr::Not(Box::new(Expr::Atom(parse_positive(rest)?))));
                }
            }
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        if matches!(self.tokens.get(self.pos), Some(Tok::LParen)) {
            self.pos += 1;
            let expr = self.parse_or()?;
            if !matches!(self.tokens.get(self.pos), Some(Tok::RParen)) {
                return Err("括号未闭合".to_string());
            }
            self.pos += 1;
            return Ok(expr);
        }
        if matches!(self.tokens.get(self.pos), Some(Tok::Term(_))) {
            let Tok::Term(term) = self.tokens[self.pos].take_term() else {
                unreachable!();
            };
            self.pos += 1;
            return Ok(Expr::Atom(parse_positive(&term)?));
        }
        match self.tokens.get(self.pos) {
            Some(Tok::RParen) => Err("多余的右括号".to_string()),
            Some(Tok::Or | Tok::And) => Err("缺少过滤条件".to_string()),
            Some(Tok::Not) => Err("! 后面缺少条件".to_string()),
            _ => Err("缺少过滤条件".to_string()),
        }
    }

    fn eat_or(&mut self) -> bool {
        if matches!(self.tokens.get(self.pos), Some(Tok::Or)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}

impl Tok {
    fn take_term(&mut self) -> Tok {
        std::mem::replace(self, Tok::And)
    }
}

fn parse_positive(term: &str) -> Result<Filter, String> {
    if let Some(pattern) = term
        .strip_prefix("re-s:")
        .or_else(|| term.strip_prefix("re:"))
    {
        let case_insensitive = !term.starts_with("re-s:");
        let re = RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| format!("正则错误: {e}"))?;
        return Ok(Filter::Regex {
            re,
            prefilter: regex_prefilter(pattern),
        });
    }
    if let Some(rest) = term.strip_prefix("id:") {
        if rest.is_empty() {
            return Err("id: 后面要有关联值".to_string());
        }
        return Ok(Filter::Id(Needle::new(rest, false)));
    }
    if let Some(rest) = term.strip_prefix("module:") {
        return Ok(Filter::Module(Needle::new(rest, false)));
    }
    if let Some(rest) = term.strip_prefix("field:") {
        let (key, value) = rest
            .split_once('=')
            .ok_or_else(|| "字段过滤格式: field:key=value 或 field:key~=value".to_string())?;
        let (key, exact) = if let Some(key) = key.strip_suffix('~') {
            (key, false)
        } else {
            (key, true)
        };
        let value = value.strip_prefix('=').unwrap_or(value);
        if key.is_empty() || value.is_empty() {
            return Err("字段过滤的 key 和 value 都不能为空".to_string());
        }
        return Ok(Filter::Field {
            key: key.to_string(),
            needle: Needle::new(value, false),
            exact,
        });
    }
    if let Some(rest) = term.strip_prefix("time:") {
        return parse_time_filter(rest);
    }
    let lower = term.to_ascii_lowercase();
    if lower == "bm:" || lower == "bookmark" || lower == "bookmarked" {
        return Ok(Filter::Bookmarked);
    }
    if let Some(rest) = lower.strip_prefix("level") {
        for op in [">=", "<=", ">", "<", "="] {
            if let Some(value) = rest.strip_prefix(op) {
                let sev = Severity::parse(value).ok_or_else(|| format!("未知日志级别: {value}"))?;
                return Ok(match op {
                    ">=" => Filter::LevelAtLeast(sev),
                    "=" => Filter::Level(sev),
                    ">" => Filter::LevelAtLeast(
                        sev.next().ok_or_else(|| "FATAL 之上没有级别".to_string())?,
                    ),
                    "<" => Filter::Not(Box::new(Filter::LevelAtLeast(sev))),
                    "<=" => Filter::Not(Box::new(Filter::Level(sev))),
                    _ => unreachable!(),
                });
            }
        }
    }
    Ok(Filter::Contains(Needle::new(term, false)))
}

fn parse_time_filter(rest: &str) -> Result<Filter, String> {
    let (from, to) = rest
        .split_once("..")
        .ok_or_else(|| "时间范围格式: time:起..止".to_string())?;
    if from.contains('-') || to.contains('-') {
        let start =
            crate::time::parse_instant(from).ok_or_else(|| format!("无法解析时间: {from}"))?;
        let end = crate::time::parse_instant(to).ok_or_else(|| format!("无法解析时间: {to}"))?;
        if start >= end {
            return Err("绝对时间范围的起点必须早于终点".to_string());
        }
        return Ok(Filter::TimeRange {
            start,
            end,
            absolute: true,
            wrap: false,
        });
    }
    let start = crate::time::parse_time_of_day(from)
        .ok_or_else(|| format!("无法解析时间: {from}（期望 HH:MM:SS[.fff]）"))?;
    let end = crate::time::parse_time_of_day(to)
        .ok_or_else(|| format!("无法解析时间: {to}（期望 HH:MM:SS[.fff]）"))?;
    if start == end {
        return Err("时间范围不能为空".to_string());
    }
    Ok(Filter::TimeRange {
        start,
        end,
        absolute: false,
        wrap: start > end,
    })
}

fn tokenize(query: &str) -> Result<Vec<Tok>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = query.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => quoted = !quoted,
            '(' if !quoted && current.is_empty() => tokens.push(Tok::LParen),
            ')' if !quoted && current.is_empty() => tokens.push(Tok::RParen),
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    push_term(&mut tokens, &mut current);
                }
            }
            c => current.push(c),
        }
    }
    if quoted {
        return Err("引号未闭合".to_string());
    }
    if !current.is_empty() {
        push_term(&mut tokens, &mut current);
    }
    Ok(tokens)
}

fn push_term(tokens: &mut Vec<Tok>, current: &mut String) {
    let term = std::mem::take(current);
    if term.eq_ignore_ascii_case("OR") {
        tokens.push(Tok::Or);
        return;
    }
    if term.eq_ignore_ascii_case("AND") {
        tokens.push(Tok::And);
        return;
    }
    let mut rest = term.as_str();
    if rest == "!" {
        tokens.push(Tok::Not);
        return;
    }
    if let Some(stripped) = rest.strip_prefix('!') {
        tokens.push(Tok::Not);
        rest = stripped;
    }
    while let Some(stripped) = rest.strip_prefix('(') {
        tokens.push(Tok::LParen);
        rest = stripped;
    }
    let regex = rest.starts_with("re:") || rest.starts_with("re-s:");
    let mut trailing = 0;
    if !regex {
        let bytes = rest.as_bytes();
        while trailing < rest.len() && bytes[rest.len() - 1 - trailing] == b')' {
            trailing += 1;
        }
    }
    let core = &rest[..rest.len() - trailing];
    if !core.is_empty() {
        tokens.push(Tok::Term(core.to_string()));
    }
    for _ in 0..trailing {
        tokens.push(Tok::RParen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PlainPlugin;

    fn ctx<'a>(text: &'a str, plugin: &'a dyn LogPlugin, bookmarked: bool) -> MatchCtx<'a> {
        MatchCtx {
            text,
            plugin,
            time: 0,
            bookmarked,
        }
    }

    #[test]
    fn plain_terms_are_and_ed() {
        let plugin = PlainPlugin;
        let query = parse_query("error timeout").unwrap();
        assert!(query.matches(&ctx("ERROR: connection timeout", &plugin, false)));
        assert!(!query.matches(&ctx("ERROR: connection reset", &plugin, false)));
    }

    #[test]
    fn quoted_terms_keep_spaces() {
        let plugin = PlainPlugin;
        let query = parse_query(r#"level>=warn "connection reset""#).unwrap();
        assert!(query.matches(&ctx("WARN connection reset now", &plugin, false)));
        assert!(!query.matches(&ctx("WARN connection", &plugin, false)));
    }

    #[test]
    fn level_filters() {
        let plugin = PlainPlugin;
        let filters = parse_query("level>=warn").unwrap();
        assert!(filters.matches(&ctx("x ERROR y", &plugin, false)));
        assert!(!filters.matches(&ctx("x INFO y", &plugin, false)));

        let only = parse_query("level=warn").unwrap();
        assert!(only.matches(&ctx("WARN", &plugin, false)));
        assert!(!only.matches(&ctx("ERROR", &plugin, false)));

        let below = parse_query("!level>=warn").unwrap();
        assert!(below.matches(&ctx("INFO", &plugin, false)));
        assert!(!below.matches(&ctx("ERROR", &plugin, false)));
    }

    #[test]
    fn or_and_parentheses() {
        let plugin = PlainPlugin;
        let query = parse_query("error OR timeout").unwrap();
        assert!(query.matches(&ctx("ERROR ready", &plugin, false)));
        assert!(query.matches(&ctx("timeout", &plugin, false)));
        assert!(!query.matches(&ctx("info", &plugin, false)));

        let grouped = parse_query("(error OR timeout) camera").unwrap();
        assert!(grouped.matches(&ctx("ERROR camera", &plugin, false)));
        assert!(!grouped.matches(&ctx("ERROR lidar", &plugin, false)));

        let negated = parse_query("!(error OR timeout)").unwrap();
        assert!(negated.matches(&ctx("info ready", &plugin, false)));
        assert!(!negated.matches(&ctx("ERROR", &plugin, false)));
        assert_eq!(parse_query("timeout around:2").unwrap().around, 2);
    }

    #[test]
    fn bookmark_filter() {
        let plugin = PlainPlugin;
        let filters = parse_query("bm:").unwrap();
        assert!(filters.matches(&ctx("whatever", &plugin, true)));
        assert!(!filters.matches(&ctx("whatever", &plugin, false)));
    }

    #[test]
    fn regex_filter_is_case_insensitive_unless_asked() {
        let plugin = PlainPlugin;
        let filters = parse_query("re:time.?out").unwrap();
        assert!(filters.matches(&ctx("Timeout", &plugin, false)));
        let sensitive = parse_query("re-s:Timeout").unwrap();
        assert!(sensitive.matches(&ctx("Timeout", &plugin, false)));
        assert!(!sensitive.matches(&ctx("timeout", &plugin, false)));
    }

    #[test]
    fn regex_prefilter_extracts_required_literal() {
        let filters = parse_query(r#""re:latency 49[0-9]ms""#).unwrap();
        let Filter::Regex { prefilter, .. } = filters.only_atom().unwrap() else {
            panic!("expected regex filter");
        };
        let prefilter = prefilter.as_ref().expect("literal prefilter");
        assert_eq!(prefilter.literal(), "latency 49");
        assert!(prefilter.accepts("latency 495ms"));
        assert!(!prefilter.accepts("latency 485ms"));

        let alternation = parse_query("re:error|warning").unwrap();
        let Filter::Regex { prefilter, .. } = alternation.only_atom().unwrap() else {
            panic!("expected regex filter");
        };
        assert!(prefilter.is_none());
    }

    #[test]
    fn regex_prefilter_keeps_engine_semantics() {
        let plugin = PlainPlugin;
        let cases = [
            (r#""re:latency 49[0-9]ms""#, "x latency 495ms", true),
            (r#""re:latency 49[0-9]ms""#, "x latency 485ms", false),
            ("re:TIMEOUT", "a timeout", true),
            ("re:(?:abc)?defg", "defg", true),
            ("re:(?:abc)?defg", "xyz", false),
            ("re:error|warning", "warning", true),
            ("re:time.?out", "timeout", true),
            (r#""re:(?-i:ABC) def""#, "ABC def", true),
            (r#""re:(?-i:ABC) def""#, "abc def", false),
        ];
        for (query, text, expected) in cases {
            let filters = parse_query(query).unwrap();
            assert_eq!(
                filters.matches(&ctx(text, &plugin, false)),
                expected,
                "{query} on {text:?}"
            );
        }
    }

    #[test]
    fn regex_prefilter_skips_unicode_lines() {
        let plugin = PlainPlugin;
        let filters = parse_query("re:status").unwrap();
        // U+017F folds to 's', so the regex engine matches while the byte-level
        // prefilter would not; non-ASCII lines therefore bypass the prefilter.
        assert!(filters.matches(&ctx("ſtatus", &plugin, false)));
    }

    #[test]
    fn bad_regex_is_reported() {
        assert!(parse_query("re:[").is_err());
    }

    #[test]
    fn field_filter_uses_plugin_fields() {
        let plugin = PlainPlugin;
        let filters = parse_query("field:module=lidar").unwrap();
        assert!(filters.matches(&ctx("level=INFO module=lidar msg=ready", &plugin, false)));
        assert!(!filters.matches(&ctx(
            "level=INFO module=lidar_extra msg=ready",
            &plugin,
            false
        )));
        assert!(!filters.matches(&ctx("level=INFO module=camera msg=ready", &plugin, false)));
        let contains = parse_query("field:module~=lid").unwrap();
        assert!(contains.matches(&ctx("level=INFO module=lidar msg=ready", &plugin, false)));
        assert!(parse_query("field:broken").is_err());
    }

    #[test]
    fn id_filter_matches_the_same_correlation_value() {
        let plugin = PlainPlugin;
        let query = parse_query("id:abc-123").unwrap();
        assert!(query.matches(&ctx("INFO trace_id=abc-123 ready", &plugin, false)));
        assert!(!query.matches(&ctx("INFO trace_id=abc-1234 ready", &plugin, false)));
        assert!(!query.matches(&ctx("INFO other", &plugin, false)));
    }

    #[test]
    fn time_range_matches_time_of_day_and_absolute() {
        let plugin = PlainPlugin;
        let filters = parse_query("time:10:00:00..10:05:00").unwrap();
        let day = 1_793_404_800_000_000_000;
        assert!(filters.matches(&MatchCtx {
            text: "x",
            plugin: &plugin,
            time: day + 10 * 3_600_000_000_000,
            bookmarked: false,
        }));
        assert!(!filters.matches(&MatchCtx {
            text: "x",
            plugin: &plugin,
            time: day + 10 * 3_600_000_000_000 + 5 * 60_000_000_000,
            bookmarked: false,
        }));
        let wrap = parse_query("time:23:00:00..01:00:00").unwrap();
        assert!(wrap.matches(&MatchCtx {
            text: "x",
            plugin: &plugin,
            time: day + 23 * 3_600_000_000_000,
            bookmarked: false,
        }));
        assert!(wrap.matches(&MatchCtx {
            text: "x",
            plugin: &plugin,
            time: day + 30 * 60_000_000_000,
            bookmarked: false,
        }));
        assert!(!wrap.matches(&MatchCtx {
            text: "x",
            plugin: &plugin,
            time: day + 12 * 3_600_000_000_000,
            bookmarked: false,
        }));
        let absolute = parse_query("time:2026-10-31T08:00:00..2026-10-31T09:30:00").unwrap();
        let start = crate::time::parse_instant("2026-10-31T08:30:00").unwrap();
        let outside = crate::time::parse_instant("2026-10-31T10:00:00").unwrap();
        assert!(absolute.matches(&MatchCtx {
            text: "x",
            plugin: &plugin,
            time: start,
            bookmarked: false,
        }));
        assert!(!absolute.matches(&MatchCtx {
            text: "x",
            plugin: &plugin,
            time: outside,
            bookmarked: false,
        }));
    }
}
