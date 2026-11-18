use regex::{Regex, RegexBuilder};
use regex_syntax::hir::{Hir, HirKind};

#[derive(Debug, Clone)]
pub struct Needle {
    raw: String,
    lower: String,
    case_sensitive: bool,
}

impl Needle {
    pub fn new(raw: impl Into<String>, case_sensitive: bool) -> Self {
        let raw = raw.into();
        let lower = raw.to_lowercase();
        Self {
            raw,
            lower,
            case_sensitive,
        }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    pub fn contains(&self, hay: &str) -> bool {
        if self.case_sensitive {
            return hay.contains(&self.raw);
        }
        if self.raw.is_ascii() {
            contains_ascii_ci(hay.as_bytes(), self.raw.as_bytes()).is_some()
        } else {
            hay.to_lowercase().contains(&self.lower)
        }
    }

    pub fn find_all(&self, hay: &str, out: &mut Vec<(usize, usize)>) {
        if self.raw.is_empty() {
            return;
        }
        if self.case_sensitive {
            out.extend(hay.match_indices(&self.raw).map(|(i, m)| (i, i + m.len())));
            return;
        }
        if self.raw.is_ascii() {
            let hb = hay.as_bytes();
            let nb = self.raw.as_bytes();
            let mut from = 0;
            while let Some(offset) = contains_ascii_ci(&hb[from..], nb) {
                let start = from + offset;
                out.push((start, start + nb.len()));
                from = start + nb.len();
            }
        } else {
            let lowered = hay.to_lowercase();
            let mut from = 0;
            while let Some(offset) = lowered[from..].find(&self.lower) {
                let start = from + offset;
                let end = start + self.lower.len();
                if hay.is_char_boundary(start) && hay.is_char_boundary(end) {
                    out.push((start, end));
                }
                from = end.max(start + 1);
            }
        }
    }

    /// Allocation-free variant of `find_all` for predicate searches. Stops at the
    /// first match accepted by `visit`.
    pub fn any_match(&self, hay: &str, mut visit: impl FnMut(usize, usize) -> bool) -> bool {
        if self.raw.is_empty() {
            return false;
        }
        if self.case_sensitive {
            return hay
                .match_indices(&self.raw)
                .any(|(i, m)| visit(i, i + m.len()));
        }
        if self.raw.is_ascii() {
            let hb = hay.as_bytes();
            let nb = self.raw.as_bytes();
            let mut from = 0;
            while let Some(offset) = contains_ascii_ci(&hb[from..], nb) {
                let start = from + offset;
                let end = start + nb.len();
                if visit(start, end) {
                    return true;
                }
                from = end;
            }
            false
        } else {
            let lowered = hay.to_lowercase();
            let mut from = 0;
            while let Some(offset) = lowered[from..].find(&self.lower) {
                let start = from + offset;
                let end = start + self.lower.len();
                if hay.is_char_boundary(start) && hay.is_char_boundary(end) && visit(start, end) {
                    return true;
                }
                from = end.max(start + 1);
            }
            false
        }
    }
}

/// A literal that every regex match must contain, used to reject most lines
/// with a cheap `memchr` scan before running the engine.
#[derive(Debug, Clone)]
pub struct RegexPrefilter {
    needle: Needle,
    /// Unicode case folding can make non-ASCII text match an ASCII pattern, so
    /// the byte-level scan is only applied to ASCII lines when the literal has
    /// letters.
    ascii_only: bool,
}

impl RegexPrefilter {
    pub fn literal(&self) -> &str {
        self.needle.raw()
    }

    pub fn accepts(&self, text: &str) -> bool {
        if self.ascii_only && !text.is_ascii() {
            return true;
        }
        self.needle.contains(text)
    }
}

/// Extracts the longest literal that every match of `pattern` must contain.
/// Bails out for alternations and optional repetitions, where no single literal
/// is guaranteed. The needle is always case-insensitive: that is a superset of
/// both insensitive and inline case-sensitive patterns, so it cannot reject a
/// real match.
pub fn regex_prefilter(pattern: &str) -> Option<RegexPrefilter> {
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let literal = longest_required_literal(&hir)?;
    if literal.len() < 3 || !literal.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        return None;
    }
    let ascii_only = literal.iter().any(|b| b.is_ascii_alphabetic());
    let needle = Needle::new(String::from_utf8(literal).ok()?, false);
    Some(RegexPrefilter { needle, ascii_only })
}

fn longest_required_literal(hir: &Hir) -> Option<Vec<u8>> {
    match hir.kind() {
        HirKind::Literal(literal) => Some(literal.0.to_vec()),
        HirKind::Repetition(repetition) if repetition.min == 0 => None,
        HirKind::Repetition(repetition) => longest_required_literal(&repetition.sub),
        HirKind::Capture(capture) => longest_required_literal(&capture.sub),
        HirKind::Concat(parts) => parts
            .iter()
            .filter_map(longest_required_literal)
            .max_by_key(|literal| literal.len()),
        HirKind::Alternation(_) => None,
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// Shared matcher for the find bar and global search: highlighting, match lists
/// and result snippets all use the same compiled query.
#[derive(Debug, Clone)]
pub enum SearchMatcher {
    Plain {
        needle: Needle,
        whole_word: bool,
        key: u64,
    },
    Regex {
        re: Regex,
        whole_word: bool,
        prefilter: Option<RegexPrefilter>,
        key: u64,
    },
}

impl SearchMatcher {
    /// `Ok(None)` for an empty query, `Err` for an invalid regex.
    pub fn build(query: &str, opts: SearchOptions) -> Result<Option<Self>, String> {
        if query.is_empty() {
            return Ok(None);
        }
        let key = matcher_key(query, opts);
        if opts.regex {
            let re = RegexBuilder::new(query)
                .case_insensitive(!opts.case_sensitive)
                .build()
                .map_err(|e| format!("正则错误: {e}"))?;
            Ok(Some(Self::Regex {
                prefilter: regex_prefilter(query),
                re,
                whole_word: opts.whole_word,
                key,
            }))
        } else {
            Ok(Some(Self::Plain {
                needle: Needle::new(query, opts.case_sensitive),
                whole_word: opts.whole_word,
                key,
            }))
        }
    }

    /// Identity of the query and options, for UI caches that keep the spans a
    /// matcher produced. Equal keys imply equal match results.
    pub fn key(&self) -> u64 {
        match self {
            Self::Plain { key, .. } | Self::Regex { key, .. } => *key,
        }
    }

    pub fn is_match(&self, text: &str) -> bool {
        match self {
            Self::Plain {
                needle, whole_word, ..
            } => {
                if *whole_word {
                    needle.any_match(text, |start, end| word_boundary(text, start, end))
                } else {
                    needle.contains(text)
                }
            }
            Self::Regex {
                re,
                whole_word,
                prefilter,
                ..
            } => {
                if prefilter.as_ref().is_some_and(|p| !p.accepts(text)) {
                    return false;
                }
                if *whole_word {
                    re.find_iter(text)
                        .any(|m| m.start() < m.end() && word_boundary(text, m.start(), m.end()))
                } else {
                    re.is_match(text)
                }
            }
        }
    }

    pub fn find_all(&self, text: &str, out: &mut Vec<(usize, usize)>) {
        match self {
            Self::Plain {
                needle, whole_word, ..
            } => {
                if *whole_word {
                    let from = out.len();
                    needle.find_all(text, out);
                    let mut write = from;
                    for read in from..out.len() {
                        if word_boundary(text, out[read].0, out[read].1) {
                            out[write] = out[read];
                            write += 1;
                        }
                    }
                    out.truncate(write);
                } else {
                    needle.find_all(text, out);
                }
            }
            Self::Regex {
                re,
                whole_word,
                prefilter,
                ..
            } => {
                if prefilter.as_ref().is_some_and(|p| !p.accepts(text)) {
                    return;
                }
                for m in re.find_iter(text) {
                    if m.start() < m.end()
                        && (!*whole_word || word_boundary(text, m.start(), m.end()))
                    {
                        out.push((m.start(), m.end()));
                    }
                }
            }
        }
    }
}

fn matcher_key(query: &str, opts: SearchOptions) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    query.hash(&mut hasher);
    opts.hash(&mut hasher);
    hasher.finish()
}

fn word_boundary(text: &str, start: usize, end: usize) -> bool {
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
    let before = text[..start].chars().next_back().is_some_and(is_word);
    let after = text[end..].chars().next().is_some_and(is_word);
    !before && !after
}

/// Finds `needle` in `hay` comparing ASCII letters case-insensitively. Used as a
/// cheap necessary-condition check before heavier parsing.
pub(crate) fn contains_ascii_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    if hay.len() < needle.len() {
        return None;
    }
    let lower = first.to_ascii_lowercase();
    let upper = first.to_ascii_uppercase();
    let last_start = hay.len() - needle.len();
    let mut from = 0;
    while from <= last_start {
        let offset = memchr::memchr2(lower, upper, &hay[from..=last_start])?;
        let start = from + offset;
        if hay[start + 1..start + needle.len()].eq_ignore_ascii_case(rest) {
            return Some(start);
        }
        from = start + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_case_insensitive_search() {
        let n = Needle::new("ERROR", false);
        assert!(n.contains("an error happened"));
        let mut hits = Vec::new();
        n.find_all("Error and ERROR", &mut hits);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], (0, 5));
    }

    #[test]
    fn case_sensitive_search() {
        let n = Needle::new("ERROR", true);
        assert!(!n.contains("error"));
        assert!(n.contains("ERROR"));
    }

    #[test]
    fn chinese_substring() {
        let n = Needle::new("超时", false);
        assert!(n.contains("连接超时了"));
    }

    #[test]
    fn missing_needle_is_none() {
        let n = Needle::new("zzz", false);
        assert!(!n.contains("abc"));
    }

    #[test]
    fn whole_word_plain_matches_token_boundaries() {
        let matcher = SearchMatcher::build(
            "cat",
            SearchOptions {
                whole_word: true,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert!(matcher.is_match("a cat."));
        assert!(matcher.is_match("x.cat"));
        assert!(!matcher.is_match("scatter"));
        assert!(!matcher.is_match("cat_1"));
        let mut hits = Vec::new();
        matcher.find_all("cat scatter cat", &mut hits);
        assert_eq!(hits, vec![(0, 3), (12, 15)]);
    }

    #[test]
    fn regex_respects_case_and_reports_errors() {
        let sensitive = SearchMatcher::build(
            "ERROR",
            SearchOptions {
                regex: true,
                case_sensitive: true,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert!(!sensitive.is_match("error and Error"));
        let insensitive = SearchMatcher::build(
            "ERROR",
            SearchOptions {
                regex: true,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert!(insensitive.is_match("error"));
        assert!(SearchMatcher::build(
            "(",
            SearchOptions {
                regex: true,
                ..Default::default()
            }
        )
        .is_err());
        assert!(SearchMatcher::build("", SearchOptions::default())
            .unwrap()
            .is_none());
    }

    #[test]
    fn whole_word_regex_needs_boundaries() {
        let matcher = SearchMatcher::build(
            r"ERR(or)?",
            SearchOptions {
                regex: true,
                whole_word: true,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert!(matcher.is_match("ERROR: x"));
        assert!(!matcher.is_match("TERROR"));
    }

    #[test]
    fn regex_search_prefilter_keeps_matches() {
        let matcher = SearchMatcher::build(
            r"latency 49[0-9]ms",
            SearchOptions {
                regex: true,
                case_sensitive: true,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert!(matcher.is_match("x latency 495ms"));
        assert!(!matcher.is_match("x latency 485ms"));
        let mut hits = Vec::new();
        matcher.find_all("latency 495ms and 485ms", &mut hits);
        assert_eq!(hits, vec![(0, 13)]);

        // Inline flags and Unicode folding must survive the ASCII prefilter.
        let inline = SearchMatcher::build(
            "(?i)ERROR",
            SearchOptions {
                regex: true,
                case_sensitive: true,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert!(inline.is_match("error"));
        let folding = SearchMatcher::build(
            "status",
            SearchOptions {
                regex: true,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert!(folding.is_match("ſtatus"));
    }

    #[test]
    fn matcher_key_tracks_query_and_options() {
        let base = SearchOptions::default();
        let a = SearchMatcher::build("error", base).unwrap().unwrap();
        let b = SearchMatcher::build("error", base).unwrap().unwrap();
        assert_eq!(a.key(), b.key());
        assert_ne!(
            a.key(),
            SearchMatcher::build("warning", base)
                .unwrap()
                .unwrap()
                .key()
        );
        assert_ne!(
            a.key(),
            SearchMatcher::build(
                "error",
                SearchOptions {
                    whole_word: true,
                    ..base
                }
            )
            .unwrap()
            .unwrap()
            .key()
        );
        // Same text compiled as a regex is a different matcher.
        assert_ne!(
            a.key(),
            SearchMatcher::build(
                "error",
                SearchOptions {
                    regex: true,
                    ..base
                }
            )
            .unwrap()
            .unwrap()
            .key()
        );
    }
}
