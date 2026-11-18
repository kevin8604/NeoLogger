use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::matcher::Needle;
use crate::source::DocId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self(r, g, b)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextStyle {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
}

impl TextStyle {
    pub fn is_plain(&self) -> bool {
        self.fg.is_none() && self.bg.is_none() && !self.bold
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSpan {
    pub start: usize,
    pub end: usize,
    pub style: TextStyle,
}

impl StyledSpan {
    pub fn new(start: usize, end: usize, style: TextStyle) -> Option<Self> {
        (start < end).then_some(Self { start, end, style })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowStyle {
    pub bg: Option<Rgb>,
    pub fg: Option<Rgb>,
    pub bold: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleScope {
    Global,
    Doc(DocId),
    Plugin(String),
}

#[derive(Debug)]
pub enum Matcher {
    Plain(Needle),
    Regex(Regex),
}

impl Matcher {
    pub fn is_match(&self, text: &str) -> bool {
        match self {
            Matcher::Plain(n) => n.contains(text),
            Matcher::Regex(re) => re.is_match(text),
        }
    }

    pub fn find_all(&self, text: &str, out: &mut Vec<(usize, usize)>) {
        match self {
            Matcher::Plain(n) => n.find_all(text, out),
            Matcher::Regex(re) => out.extend(re.find_iter(text).map(|m| (m.start(), m.end()))),
        }
    }
}

#[derive(Debug)]
pub struct HighlightRule {
    pub id: u64,
    pub label: String,
    pub scope: RuleScope,
    pub matcher: Matcher,
    pub style: TextStyle,
    pub row_bg: Option<Rgb>,
}

impl HighlightRule {
    pub fn plain(
        label: impl Into<String>,
        scope: RuleScope,
        needle: impl Into<String>,
        case_sensitive: bool,
        style: TextStyle,
        row_bg: Option<Rgb>,
    ) -> Self {
        let needle = needle.into();
        Self {
            id: 0,
            label: label.into(),
            scope,
            matcher: Matcher::Plain(Needle::new(needle, case_sensitive)),
            style,
            row_bg,
        }
    }

    pub fn regex(
        label: impl Into<String>,
        scope: RuleScope,
        pattern: &str,
        style: TextStyle,
        row_bg: Option<Rgb>,
    ) -> Result<Self, regex::Error> {
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()?;
        Ok(Self {
            id: 0,
            label: label.into(),
            scope,
            matcher: Matcher::Regex(re),
            style,
            row_bg,
        })
    }

    pub fn applies_to(&self, doc: DocId, plugin_id: &str) -> bool {
        match &self.scope {
            RuleScope::Global => true,
            RuleScope::Doc(d) => *d == doc,
            RuleScope::Plugin(p) => p == plugin_id,
        }
    }

    pub fn is_match(&self, text: &str) -> bool {
        self.matcher.is_match(text)
    }

    pub fn collect_spans(&self, text: &str, out: &mut Vec<StyledSpan>) {
        if self.style.is_plain() {
            return;
        }
        let mut ranges = Vec::new();
        self.matcher.find_all(text, &mut ranges);
        for (start, end) in ranges {
            if let Some(span) = StyledSpan::new(start, end, self.style) {
                out.push(span);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_rule_matches_and_spans() {
        let rule = HighlightRule::plain(
            "error",
            RuleScope::Global,
            "error",
            false,
            TextStyle {
                fg: Some(Rgb::new(255, 0, 0)),
                ..Default::default()
            },
            Some(Rgb::new(30, 0, 0)),
        );
        assert!(rule.is_match("an ERROR occurred"));
        let mut spans = Vec::new();
        rule.collect_spans("ERROR and error", &mut spans);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].start, 10);
    }

    #[test]
    fn scope_filters_rules() {
        let rule = HighlightRule::plain(
            "x",
            RuleScope::Doc(1),
            "x",
            true,
            TextStyle::default(),
            None,
        );
        assert!(rule.applies_to(1, "generic"));
        assert!(!rule.applies_to(2, "generic"));
    }
}
