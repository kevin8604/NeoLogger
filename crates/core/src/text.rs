/// Returns the whitespace-delimited word containing `char_idx` (0-based char index).
/// Punctuation that commonly terminates log tokens is excluded from words.
pub fn word_at_char(text: &str, char_idx: usize) -> Option<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let (_, ch) = *chars.get(char_idx)?;
    if !is_word_char(ch) {
        return None;
    }
    let mut start_idx = char_idx;
    while start_idx > 0 && is_word_char(chars[start_idx - 1].1) {
        start_idx -= 1;
    }
    let mut end_idx = char_idx;
    while end_idx + 1 < chars.len() && is_word_char(chars[end_idx + 1].1) {
        end_idx += 1;
    }
    let start = chars[start_idx].0;
    let end = chars
        .get(end_idx + 1)
        .map(|(byte, _)| *byte)
        .unwrap_or(text.len());
    Some(&text[start..end])
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '/' | '@' | '#' | '\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_word_under_cursor() {
        let line = "[ERROR] node_camera: timeout after 3s";
        let idx = line.find("timeout").unwrap();
        assert_eq!(word_at_char(line, idx), Some("timeout"));
    }

    #[test]
    fn pulls_whole_token_on_partial_index() {
        let line = "path=/dev/ttyUSB0 failed";
        let idx = line.find("USB").unwrap();
        assert_eq!(word_at_char(line, idx), Some("/dev/ttyUSB0"));
    }

    #[test]
    fn punctuation_returns_none() {
        let line = "a, b";
        assert_eq!(word_at_char(line, 1), None);
    }

    #[test]
    fn chinese_word() {
        let line = "传感器 温度过高 触发保护";
        let byte_idx = line.find("过高").unwrap();
        let char_idx = line[..byte_idx].chars().count();
        assert_eq!(word_at_char(line, char_idx), Some("温度过高"));
    }
}
