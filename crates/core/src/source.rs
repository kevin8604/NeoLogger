use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;

use memmap2::Mmap;

use crate::mcap::{self, McapCatalog};

pub type DocId = u32;
pub type LineNo = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    Gb18030,
}

impl TextEncoding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Gb18030 => "GB18030",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Gb18030 => "gb18030",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "utf-8" | "utf8" => Some(Self::Utf8),
            "gb18030" | "gbk" | "cp936" => Some(Self::Gb18030),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LineRef {
    pub doc: DocId,
    pub line: LineNo,
}

impl LineRef {
    pub fn new(doc: DocId, line: usize) -> Self {
        Self {
            doc,
            line: line as LineNo,
        }
    }
}

enum Text {
    Mapped(Mmap),
    Owned(Box<str>),
}

impl Text {
    fn bytes(&self) -> &[u8] {
        match self {
            Text::Mapped(m) => m,
            Text::Owned(s) => s.as_bytes(),
        }
    }
}

pub struct LogDoc {
    pub id: DocId,
    pub path: PathBuf,
    text: Text,
    starts: Vec<usize>,
    times: Vec<i64>,
    /// True when the line parsed its own timestamp. Continuation lines are false
    /// and belong to the previous event.
    owned: Vec<bool>,
    pub plugin_id: String,
    pub day_base: i64,
    pub encoding: TextEncoding,
    pub missing: bool,
    /// When set, reloads read into memory and drop the file so another process
    /// can truncate or replace it. A live mapping blocks copytruncate on Windows.
    pub unmapped: bool,
    /// Present when the file was an MCAP bag. Payloads are not retained; the
    /// text is one short line per message plus any history projection.
    pub mcap: Option<McapCatalog>,
}

impl LogDoc {
    pub fn open(id: DocId, path: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with(id, path, None)
    }

    /// `forced` skips detection. UTF-8 that is not valid is lossily decoded only
    /// when the user explicitly picks UTF-8.
    pub fn open_with(
        id: DocId,
        path: impl Into<PathBuf>,
        forced: Option<TextEncoding>,
    ) -> io::Result<Self> {
        let path = path.into();
        let mut file = File::open(&path)?;
        if forced.is_none() {
            let mut magic = [0u8; 8];
            let read = file.read(&mut magic)?;
            file.seek(SeekFrom::Start(0))?;
            if read == 8 && mcap::is_mcap(&magic) {
                let bytes = std::fs::read(&path)?;
                return Ok(doc_from_mcap(id, path, &bytes));
            }
        }
        if file.metadata()?.len() == 0 {
            return Ok(Self::from_text(
                id,
                path,
                Text::Owned(Box::from("")),
                forced.unwrap_or(TextEncoding::Utf8),
                false,
            ));
        }
        // SAFETY: the mapping is read-only. It is kept only when the bytes are
        // already valid UTF-8; otherwise they are decoded into an owned string.
        let map = unsafe { Mmap::map(&file)? };
        if keep_mapped(&map, forced) {
            return Ok(Self::from_text(
                id,
                path,
                Text::Mapped(map),
                TextEncoding::Utf8,
                false,
            ));
        }
        let (text, encoding) = decode_bytes(&map, forced);
        Ok(Self::from_text(id, path, text, encoding, false))
    }

    /// Placeholder kept in the session when the path cannot be opened.
    pub fn missing(id: DocId, path: impl Into<PathBuf>) -> Self {
        Self::from_text(
            id,
            path.into(),
            Text::Owned(Box::from("")),
            TextEncoding::Utf8,
            true,
        )
    }

    pub fn from_string(id: DocId, name: impl Into<PathBuf>, content: impl Into<String>) -> Self {
        Self::from_text(
            id,
            name.into(),
            Text::Owned(content.into().into_boxed_str()),
            TextEncoding::Utf8,
            false,
        )
    }

    fn from_text(
        id: DocId,
        path: PathBuf,
        text: Text,
        encoding: TextEncoding,
        missing: bool,
    ) -> Self {
        let starts = line_starts(text.bytes());
        Self {
            id,
            path,
            text,
            starts,
            times: Vec::new(),
            owned: Vec::new(),
            plugin_id: "plain".to_string(),
            day_base: crate::time::local_day_base(),
            encoding,
            missing,
            unmapped: false,
            mcap: None,
        }
    }

    pub fn mcap(&self) -> Option<&McapCatalog> {
        self.mcap.as_ref()
    }

    /// Drops generated history lines so message lines stay at their old indexes.
    pub fn retain_message_prefix(&mut self) {
        let Some(count) = self.mcap.as_ref().map(|catalog| catalog.message_lines) else {
            return;
        };
        if count >= self.len() {
            return;
        }
        let lines: Vec<String> = (0..count)
            .map(|idx| self.line_text(idx).to_string())
            .collect();
        let mut text = lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        self.text = Text::Owned(text.into_boxed_str());
        self.starts = line_starts(self.text.bytes());
    }

    pub fn append_owned(&mut self, extra: &str) {
        if extra.is_empty() {
            return;
        }
        let mut text = match &self.text {
            Text::Owned(text) => text.to_string(),
            Text::Mapped(map) => String::from_utf8_lossy(map).into_owned(),
        };
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(extra);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        self.text = Text::Owned(text.into_boxed_str());
        self.starts = line_starts(self.text.bytes());
        self.unmapped = true;
    }

    /// Reads the whole file into memory and closes it. Used while following so
    /// the log can be rotated out from under the viewer.
    pub fn open_owned(
        id: DocId,
        path: impl Into<PathBuf>,
        forced: Option<TextEncoding>,
    ) -> io::Result<Self> {
        let path = path.into();
        let bytes = std::fs::read(&path)?;
        if forced.is_none() && mcap::is_mcap(&bytes) {
            return Ok(doc_from_mcap(id, path, &bytes));
        }
        let (text, encoding) = if bytes.is_empty() {
            (
                Text::Owned(Box::from("")),
                forced.unwrap_or(TextEncoding::Utf8),
            )
        } else {
            decode_bytes(&bytes, forced)
        };
        let mut doc = Self::from_text(id, path, text, encoding, false);
        doc.unmapped = true;
        Ok(doc)
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    pub fn line_text(&self, line: usize) -> &str {
        let bytes = self.text.bytes();
        let start = self.starts[line];
        let mut end = self
            .starts
            .get(line + 1)
            .copied()
            .unwrap_or_else(|| bytes.len());
        while end > start && matches!(bytes[end - 1], b'\n' | b'\r') {
            end -= 1;
        }
        // SAFETY: the buffer was validated or lossily transcoded as UTF-8, and both
        // ends of the range sit on newline boundaries.
        unsafe { std::str::from_utf8_unchecked(&bytes[start..end]) }
    }

    pub fn sample_lines(&self, max: usize) -> Vec<&str> {
        (0..self.len().min(max))
            .map(|i| self.line_text(i))
            .collect()
    }

    pub fn short_name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }

    pub fn set_times(&mut self, times: Vec<i64>) {
        debug_assert_eq!(times.len(), self.len());
        self.times = times;
    }

    pub fn set_owned(&mut self, owned: Vec<bool>) {
        debug_assert!(owned.is_empty() || owned.len() == self.len());
        self.owned = owned;
    }

    /// Inclusive start and exclusive end of the event containing `line`.
    /// A line with its own timestamp starts an event; following lines without
    /// one stay in it. Lines before the first timestamp belong to that first event.
    /// A file with no timestamps keeps one line per event.
    pub fn event_bounds(&self, line: usize) -> (usize, usize) {
        let len = self.len();
        if len == 0 || line >= len || self.owned.len() != len {
            let start = line.min(len);
            return (start, (start + 1).min(len.max(start)));
        }
        let Some(first_owned) = self.owned.iter().position(|flag| *flag) else {
            return (line, line + 1);
        };
        let anchor = if line <= first_owned {
            first_owned
        } else if self.owned[line] {
            line
        } else {
            let mut cursor = line;
            while cursor > first_owned && !self.owned[cursor] {
                cursor -= 1;
            }
            cursor
        };
        let start = if anchor == first_owned { 0 } else { anchor };
        let mut end = anchor + 1;
        while end < len && !self.owned[end] {
            end += 1;
        }
        (start, end)
    }

    pub fn time_at(&self, line: usize) -> Option<i64> {
        self.times
            .get(line)
            .copied()
            .filter(|t| *t != crate::time::UNKNOWN)
    }

    pub fn has_times(&self) -> bool {
        self.times.iter().any(|t| *t != crate::time::UNKNOWN)
    }
}

fn doc_from_mcap(id: DocId, path: PathBuf, bytes: &[u8]) -> LogDoc {
    let decoded = mcap::decode(bytes);
    let mut doc = LogDoc::from_text(
        id,
        path,
        Text::Owned(decoded.text.into_boxed_str()),
        TextEncoding::Utf8,
        false,
    );
    doc.mcap = Some(decoded.catalog);
    doc.unmapped = true;
    doc
}

fn keep_mapped(bytes: &[u8], forced: Option<TextEncoding>) -> bool {
    match forced {
        Some(TextEncoding::Gb18030) => false,
        Some(TextEncoding::Utf8) | None => {
            !bytes.starts_with(&[0xEF, 0xBB, 0xBF])
                && !bytes.starts_with(&[0xFF, 0xFE])
                && !bytes.starts_with(&[0xFE, 0xFF])
                && std::str::from_utf8(bytes).is_ok()
        }
    }
}

fn decode_bytes(bytes: &[u8], forced: Option<TextEncoding>) -> (Text, TextEncoding) {
    if bytes.is_empty() {
        return (
            Text::Owned(Box::from("")),
            forced.unwrap_or(TextEncoding::Utf8),
        );
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) && forced != Some(TextEncoding::Gb18030) {
        let rest = &bytes[3..];
        if forced != Some(TextEncoding::Utf8) && std::str::from_utf8(rest).is_err() {
            return decode_gb18030(rest);
        }
        return (
            Text::Owned(String::from_utf8_lossy(rest).into_owned().into_boxed_str()),
            TextEncoding::Utf8,
        );
    }
    if let Some(units) = decode_utf16_bom(bytes) {
        if forced != Some(TextEncoding::Gb18030) {
            return (
                Text::Owned(String::from_utf16_lossy(&units).into_boxed_str()),
                TextEncoding::Utf8,
            );
        }
    }
    match forced {
        Some(TextEncoding::Utf8) => (
            Text::Owned(String::from_utf8_lossy(bytes).into_owned().into_boxed_str()),
            TextEncoding::Utf8,
        ),
        Some(TextEncoding::Gb18030) => decode_gb18030(bytes),
        None if std::str::from_utf8(bytes).is_ok() => (
            Text::Owned(
                std::str::from_utf8(bytes)
                    .unwrap()
                    .to_string()
                    .into_boxed_str(),
            ),
            TextEncoding::Utf8,
        ),
        None => decode_gb18030(bytes),
    }
}

fn decode_gb18030(bytes: &[u8]) -> (Text, TextEncoding) {
    let encoding = encoding_rs::Encoding::for_label(b"gb18030").unwrap_or(encoding_rs::GBK);
    let (cow, _, _) = encoding.decode(bytes);
    (
        Text::Owned(cow.into_owned().into_boxed_str()),
        TextEncoding::Gb18030,
    )
}

fn decode_utf16_bom(bytes: &[u8]) -> Option<Vec<u16>> {
    let (rest, le) = if bytes.starts_with(&[0xFF, 0xFE]) {
        (&bytes[2..], true)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        (&bytes[2..], false)
    } else {
        return None;
    };
    let mut units = Vec::with_capacity(rest.len() / 2);
    let mut idx = 0;
    while idx + 1 < rest.len() {
        let unit = if le {
            u16::from_le_bytes([rest[idx], rest[idx + 1]])
        } else {
            u16::from_be_bytes([rest[idx], rest[idx + 1]])
        };
        units.push(unit);
        idx += 2;
    }
    Some(units)
}

fn line_starts(bytes: &[u8]) -> Vec<usize> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0];
    for i in memchr::memchr_iter(b'\n', bytes) {
        if i + 1 < bytes.len() {
            starts.push(i + 1);
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(content: &str) -> LogDoc {
        LogDoc::from_string(0, "test.log", content)
    }

    #[test]
    fn indexes_unix_and_crlf_lines() {
        let d = doc("alpha\r\nbeta\n\ngamma");
        assert_eq!(d.len(), 4);
        assert_eq!(d.line_text(0), "alpha");
        assert_eq!(d.line_text(1), "beta");
        assert_eq!(d.line_text(2), "");
        assert_eq!(d.line_text(3), "gamma");
    }

    #[test]
    fn trailing_newline_does_not_add_line() {
        let d = doc("a\nb\n");
        assert_eq!(d.len(), 2);
        assert_eq!(d.line_text(1), "b");
    }

    #[test]
    fn empty_file_has_no_lines() {
        let d = doc("");
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn line_ref_conversion() {
        let lr = LineRef::new(3, 42);
        assert_eq!(lr.doc, 3);
        assert_eq!(lr.line, 42);
    }

    #[test]
    fn detects_gb18030_and_keeps_valid_utf8() {
        let dir = std::env::temp_dir().join(format!("neolog-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gbk = dir.join("gbk.log");
        std::fs::write(&gbk, [0xD6, 0xD0, 0xCE, 0xC4, b'\n']).unwrap();
        let decoded = LogDoc::open(0, &gbk).unwrap();
        assert_eq!(decoded.encoding, TextEncoding::Gb18030);
        assert_eq!(decoded.line_text(0), "中文");

        let utf = dir.join("utf.log");
        std::fs::write(&utf, "中文\n").unwrap();
        let mapped = LogDoc::open(1, &utf).unwrap();
        assert_eq!(mapped.encoding, TextEncoding::Utf8);
        assert_eq!(mapped.line_text(0), "中文");

        let forced = LogDoc::open_with(2, &gbk, Some(TextEncoding::Utf8)).unwrap();
        assert_eq!(forced.encoding, TextEncoding::Utf8);
        assert_ne!(forced.line_text(0), "中文");

        let _ = std::fs::remove_file(&gbk);
        let _ = std::fs::remove_file(&utf);
    }

    #[test]
    fn event_bounds_share_one_start() {
        let mut sample = doc("head\n2026-10-31 10:00:00.000 ERROR boom\ntrace\nnext\n");
        sample.set_owned(vec![false, true, false, true]);
        assert_eq!(sample.event_bounds(0), (0, 3));
        assert_eq!(sample.event_bounds(1), (0, 3));
        assert_eq!(sample.event_bounds(2), (0, 3));
        assert_eq!(sample.event_bounds(3), (3, 4));
        let mut bare = doc("a\nb\n");
        bare.set_owned(vec![false, false]);
        assert_eq!(bare.event_bounds(0), (0, 1));
        assert_eq!(bare.event_bounds(1), (1, 2));
    }
}
