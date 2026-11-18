use crate::plugin::LogPlugin;

/// An id found on one line. Keys are listed from most specific to least.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorrelationId {
    pub key: String,
    pub value: String,
}

const KEYS: &[&str] = &[
    "trace_id",
    "traceid",
    "trace-id",
    "trace.id",
    "request_id",
    "requestid",
    "request-id",
    "request.id",
    "req_id",
    "correlation_id",
    "correlationid",
    "correlation-id",
    "span_id",
    "spanid",
    "x-request-id",
    "x-trace-id",
    "x-correlation-id",
    "otel.trace_id",
];

/// Ids this line can be joined on. Duplicate values collapse to the first key.
pub fn correlation_ids(plugin: &dyn LogPlugin, line: &str) -> Vec<CorrelationId> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for key in KEYS {
        let Some(value) = read_id(plugin, line, key) else {
            continue;
        };
        if !usable(&value)
            || seen
                .iter()
                .any(|prev: &String| prev.eq_ignore_ascii_case(&value))
        {
            continue;
        }
        seen.push(value.clone());
        out.push(CorrelationId {
            key: (*key).to_string(),
            value,
        });
        if out.len() == 4 {
            break;
        }
    }
    out
}

/// True when the line carries `value` as a correlation field, or as its own token.
pub fn line_has_id(plugin: &dyn LogPlugin, line: &str, value: &str) -> bool {
    let value = value.trim();
    if value.len() < 4 {
        return false;
    }
    for key in KEYS {
        if read_id(plugin, line, key).is_some_and(|found| found.eq_ignore_ascii_case(value)) {
            return true;
        }
    }
    value.len() >= 8 && contains_token(line, value)
}

fn read_id(plugin: &dyn LogPlugin, line: &str, key: &str) -> Option<String> {
    if let Some(value) = plugin.field(line, key) {
        let value = value.trim().trim_matches('"').trim().to_string();
        if usable(&value) {
            return Some(value);
        }
    }
    loose_field(line, key).map(str::to_string)
}

fn usable(value: &str) -> bool {
    let value = value.trim();
    (4..=128).contains(&value.len())
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "null" | "none" | "nil" | "undefined" | "-" | "n/a"
        )
}

fn loose_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let bytes = line.as_bytes();
    let key_bytes = key.as_bytes();
    if key_bytes.is_empty() || bytes.len() < key_bytes.len() + 1 {
        return None;
    }
    let mut i = 0;
    while i + key_bytes.len() < bytes.len() {
        if bytes[i..i + key_bytes.len()].eq_ignore_ascii_case(key_bytes) {
            let before_ok = i == 0 || !is_id_byte(bytes[i - 1]);
            if before_ok {
                let mut j = i + key_bytes.len();
                while j < bytes.len() && bytes[j] == b' ' {
                    j += 1;
                }
                if j < bytes.len() && (bytes[j] == b':' || bytes[j] == b'=') {
                    j += 1;
                    while j < bytes.len() && bytes[j] == b' ' {
                        j += 1;
                    }
                    let start = j;
                    while j < bytes.len() && is_id_byte(bytes[j]) {
                        j += 1;
                    }
                    if j - start >= 4 {
                        return Some(&line[start..j]);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn contains_token(hay: &str, needle: &str) -> bool {
    let hay = hay.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            let before_ok = i == 0 || !is_id_byte(hay[i - 1]);
            let after = i + needle.len();
            let after_ok = after == hay.len() || !is_id_byte(hay[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PlainPlugin;

    #[test]
    fn reads_trace_and_request_ids() {
        let line = "INFO trace_id=abc-123 request_id: req-9988 done";
        let ids = correlation_ids(&PlainPlugin, line);
        assert_eq!(ids[0].key, "trace_id");
        assert_eq!(ids[0].value, "abc-123");
        assert!(ids.iter().any(|id| id.value == "req-9988"));
        assert!(line_has_id(&PlainPlugin, line, "abc-123"));
        assert!(line_has_id(&PlainPlugin, line, "req-9988"));
        assert!(!line_has_id(&PlainPlugin, "trace_id=abc-1234", "abc-123"));
    }

    #[test]
    fn token_match_needs_a_boundary() {
        assert!(line_has_id(
            &PlainPlugin,
            "handled 0123456789abcdef",
            "0123456789abcdef"
        ));
        assert!(!line_has_id(
            &PlainPlugin,
            "handled x0123456789abcdef",
            "0123456789abcdef"
        ));
    }
}
