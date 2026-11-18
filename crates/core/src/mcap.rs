//! MCAP (Foxglove / rosbag2) reader.
//!
//! The bag stays a line document: each message becomes one short line, and the
//! payload bytes are dropped after the line and the field index are built.
//! Chunks are decompressed one at a time. A broken chunk keeps everything
//! decoded before it.

use std::collections::BTreeMap;

const MAGIC: &[u8] = b"\x89MCAP0\r\n";
const BODY_LIMIT: usize = 4096;
const RAW_LIMIT: usize = 10_000;

#[derive(Clone, Debug)]
pub struct McapProp {
    pub path: String,
    pub numeric: bool,
    pub last: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub changes: u64,
    pub count: u64,
}

#[derive(Clone, Debug)]
pub struct McapTopic {
    pub name: String,
    pub schema: String,
    pub log_like: bool,
    pub count: u64,
    pub first_ns: i64,
    pub last_ns: i64,
    pub props: Vec<McapProp>,
}

impl McapTopic {
    pub fn hz(&self) -> f64 {
        if self.count < 2 || self.last_ns <= self.first_ns {
            return 0.0;
        }
        self.count as f64 / ((self.last_ns - self.first_ns) as f64 / 1_000_000_000.0)
    }
}

#[derive(Clone, Debug)]
pub struct McapNode {
    pub name: String,
    pub count: u64,
    pub first_ns: i64,
    pub last_ns: i64,
    pub topics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryKind {
    Topic(String),
    Node(String),
    Changes { topic: String, field: String },
    Samples { topic: String, field: String },
}

impl HistoryKind {
    pub fn query(&self) -> String {
        match self {
            HistoryKind::Topic(topic) => {
                format!("{} field:series=-", field_term("topic", topic))
            }
            HistoryKind::Node(name) => {
                format!("{} field:series=-", field_term("node", name))
            }
            HistoryKind::Changes { topic, field } => {
                field_term("series", &series_key(topic, field, false))
            }
            HistoryKind::Samples { topic, field } => {
                field_term("series", &series_key(topic, field, true))
            }
        }
    }

    pub fn title(&self) -> String {
        match self {
            HistoryKind::Topic(topic) => topic.clone(),
            HistoryKind::Node(name) => format!("节点 {name}"),
            HistoryKind::Changes { topic, field } | HistoryKind::Samples { topic, field } => {
                let mark = if matches!(self, HistoryKind::Samples { .. }) {
                    "原始"
                } else {
                    "历史"
                };
                format!("{topic} {field} {mark}")
            }
        }
    }

    pub fn series_key(&self) -> Option<String> {
        match self {
            HistoryKind::Changes { topic, field } => Some(series_key(topic, field, false)),
            HistoryKind::Samples { topic, field } => Some(series_key(topic, field, true)),
            HistoryKind::Topic(_) | HistoryKind::Node(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct McapCatalog {
    pub topics: Vec<McapTopic>,
    pub nodes: Vec<McapNode>,
    pub error: Option<String>,
    pub consumed: u64,
    pub message_lines: usize,
    /// Property histories already appended after `message_lines`.
    pub series: Vec<String>,
}

#[derive(Clone, Debug)]
struct SchemaRec {
    name: String,
    encoding: String,
}

#[derive(Clone, Debug)]
struct ChannelRec {
    schema_id: u16,
    topic: String,
    encoding: String,
    node: String,
}

struct Built {
    order: i64,
    seq: u64,
    log_ns: i64,
    stamp_ns: i64,
    level: String,
    topic: String,
    schema: String,
    log_like: bool,
    node: String,
    publish_ns: i64,
    fields: Vec<(String, String)>,
    body: String,
}

pub struct Decoded {
    pub text: String,
    pub catalog: McapCatalog,
}

pub fn is_mcap(bytes: &[u8]) -> bool {
    bytes.len() >= MAGIC.len() && &bytes[..MAGIC.len()] == MAGIC
}

pub fn decode(bytes: &[u8]) -> Decoded {
    if !is_mcap(bytes) {
        return Decoded {
            text: String::new(),
            catalog: McapCatalog {
                error: Some("不是 MCAP 文件".to_string()),
                ..McapCatalog::default()
            },
        };
    }
    let mut schemas: BTreeMap<u16, SchemaRec> = BTreeMap::new();
    let mut channels: BTreeMap<u16, ChannelRec> = BTreeMap::new();
    let mut built = Vec::new();
    let mut seq = 0u64;
    let mut error = None;
    let mut offset = MAGIC.len();
    while offset + 9 <= bytes.len() {
        let op = bytes[offset];
        let len = u64::from_le_bytes(bytes[offset + 1..offset + 9].try_into().unwrap()) as usize;
        let start = offset + 9;
        let Some(end) = start.checked_add(len) else {
            error = Some(format!("停在偏移 {offset}: 记录长度无效"));
            break;
        };
        if end > bytes.len() {
            error = Some(format!("停在偏移 {offset}: 记录不完整"));
            break;
        }
        let data = &bytes[start..end];
        match op {
            0x03 => {
                if let Some((id, rec)) = parse_schema(data) {
                    schemas.insert(id, rec);
                }
            }
            0x04 => {
                if let Some((id, rec)) = parse_channel(data) {
                    channels.insert(id, rec);
                }
            }
            0x05 => {
                seq += 1;
                if let Some(message) = parse_message(data, &schemas, &channels, seq) {
                    built.push(message);
                }
            }
            0x06 => match inflate_chunk(data) {
                Ok(raw) => {
                    if let Err(message) =
                        walk_chunk(&raw, &mut schemas, &mut channels, &mut built, &mut seq)
                    {
                        error = Some(message);
                    }
                }
                Err(message) => error = Some(message),
            },
            0x09 => {
                seq += 1;
                if let Some(message) = parse_attachment(data, seq) {
                    built.push(message);
                }
            }
            0x0C => {
                seq += 1;
                if let Some(message) = parse_metadata(data, seq) {
                    built.push(message);
                }
            }
            _ => {}
        }
        offset = end;
    }
    built.sort_by(|a, b| a.order.cmp(&b.order).then(a.seq.cmp(&b.seq)));
    let mut catalog = McapCatalog {
        consumed: offset as u64,
        error,
        ..McapCatalog::default()
    };
    let mut text = String::new();
    for message in &built {
        fold_catalog(&mut catalog, message);
        text.push_str(&format_message(message));
        text.push('\n');
    }
    if text.is_empty() {
        let reason = catalog
            .error
            .clone()
            .unwrap_or_else(|| "文件里没有消息".to_string());
        text.push_str(&format_parts(&LineParts {
            log_ns: 1,
            level: "ERROR",
            topic: "/".to_string(),
            schema: "mcap".to_string(),
            log_like: true,
            node: "-".to_string(),
            publish_ns: 0,
            stamp_ns: 0,
            fields: vec![("series".into(), "-".into())],
            body: reason,
        }));
        text.push('\n');
        catalog.topics.push(McapTopic {
            name: "/".into(),
            schema: "mcap".into(),
            log_like: true,
            count: 1,
            first_ns: 1,
            last_ns: 1,
            props: Vec::new(),
        });
    }
    catalog.message_lines = text.lines().count();
    Decoded { text, catalog }
}

/// A parsed display line. Offsets point into `line`.
#[derive(Clone, Copy)]
pub struct ParsedLine<'a> {
    pub log_ns: i64,
    pub level: &'a str,
    pub level_range: (usize, usize),
    pub topic: &'a str,
    pub topic_range: (usize, usize),
    pub schema: &'a str,
    pub log_flag: &'a str,
    pub node: &'a str,
    pub publish: &'a str,
    pub stamp: &'a str,
    pub publish_ns: i64,
    pub stamp_ns: i64,
    pub fields: &'a str,
    pub body: &'a str,
    pub body_range: (usize, usize),
}

pub fn parse_line(line: &str) -> Option<ParsedLine<'_>> {
    if !line.starts_with("MCAP\t") {
        return None;
    }
    let mut marks = [0usize; 10];
    let mut parts = [""; 10];
    let mut count = 0;
    let mut start = 0;
    for (idx, ch) in line.char_indices() {
        if ch == '\t' && count < 9 {
            parts[count] = &line[start..idx];
            marks[count] = start;
            count += 1;
            start = idx + 1;
        }
    }
    if count != 9 {
        return None;
    }
    parts[9] = &line[start..];
    marks[9] = start;
    let range = |idx: usize| {
        let from = marks[idx];
        (from, from + parts[idx].len())
    };
    Some(ParsedLine {
        log_ns: parts[1].parse().unwrap_or(0),
        level: parts[2],
        level_range: range(2),
        topic: parts[3],
        topic_range: range(3),
        schema: parts[4],
        log_flag: parts[5],
        node: parts[6],
        publish: parts[7],
        stamp: parts[8],
        publish_ns: parts[7].parse().unwrap_or(0),
        stamp_ns: parts[8].parse().unwrap_or(0),
        fields: parts[9]
            .split_once('\t')
            .map(|(fields, _)| fields)
            .unwrap_or(""),
        body: parts[9]
            .split_once('\t')
            .map(|(_, body)| body)
            .unwrap_or(parts[9]),
        body_range: {
            let body = parts[9]
                .split_once('\t')
                .map(|(_, body)| body)
                .unwrap_or(parts[9]);
            let from = line.len() - body.len();
            (from, line.len())
        },
    })
}

pub fn field_of<'a>(parsed: &ParsedLine<'a>, key: &str) -> Option<&'a str> {
    match key {
        "topic" => Some(parsed.topic),
        "schema" => Some(parsed.schema),
        "log" => Some(parsed.log_flag),
        "node" if parsed.node != "-" && !parsed.node.is_empty() => Some(parsed.node),
        "publish" => Some(parsed.publish),
        "stamp" => Some(parsed.stamp),
        _ => blob_get(parsed.fields, key),
    }
}

pub fn blob_get<'a>(fields: &'a str, key: &str) -> Option<&'a str> {
    for pair in fields.split(';') {
        let Some((name, value)) = pair.split_once(':') else {
            continue;
        };
        if name == key {
            return Some(value);
        }
    }
    None
}

/// Builds the change-only or raw history lines for one property.
pub fn project_history(messages: &[&str], topic: &str, field: &str, raw: bool) -> (String, bool) {
    let mut points = Vec::new();
    for line in messages {
        let Some(parsed) = parse_line(line) else {
            continue;
        };
        if parsed.topic != topic || blob_get(parsed.fields, "series") != Some("-") {
            continue;
        }
        let Some(value) = field_of(&parsed, field).filter(|value| !value.is_empty()) else {
            continue;
        };
        let stamp = if parsed.stamp_ns > 0 {
            parsed.stamp_ns
        } else {
            parsed.log_ns
        };
        points.push((stamp, value.to_string(), parsed.node));
    }
    let truncated = raw && points.len() > RAW_LIMIT;
    if truncated {
        points.truncate(RAW_LIMIT);
    }
    let mut text = String::new();
    if raw {
        for (stamp, value, node) in points {
            text.push_str(&history_line(stamp, topic, field, &node, &value, 1, true));
            text.push('\n');
        }
    } else {
        let mut last: Option<String> = None;
        let mut run_stamp = 0i64;
        let mut run_value = String::new();
        let mut run_node = String::new();
        let mut run_count = 0u64;
        let flush = |text: &mut String, stamp, value: &str, node: &str, count| {
            if count == 0 {
                return;
            }
            text.push_str(&history_line(
                stamp, topic, field, node, value, count, false,
            ));
            text.push('\n');
        };
        for (stamp, value, node) in points {
            if last.as_ref() == Some(&value) {
                run_count += 1;
            } else {
                flush(&mut text, run_stamp, &run_value, &run_node, run_count);
                run_stamp = stamp;
                run_node = node.to_string();
                run_value = value.clone();
                run_count = 1;
                last = Some(value);
            }
        }
        flush(&mut text, run_stamp, &run_value, &run_node, run_count);
    }
    (text, truncated)
}

struct LineParts {
    log_ns: i64,
    level: &'static str,
    topic: String,
    schema: String,
    log_like: bool,
    node: String,
    publish_ns: i64,
    stamp_ns: i64,
    fields: Vec<(String, String)>,
    body: String,
}

fn format_message(message: &Built) -> String {
    let mut fields = message.fields.clone();
    fields.push(("series".into(), "-".into()));
    format_parts(&LineParts {
        log_ns: message.log_ns,
        level: level_static(&message.level),
        topic: message.topic.clone(),
        schema: message.schema.clone(),
        log_like: message.log_like,
        node: message.node.clone(),
        publish_ns: message.publish_ns,
        stamp_ns: message.stamp_ns,
        fields,
        body: message.body.clone(),
    })
}

fn level_static(level: &str) -> &'static str {
    match level {
        "TRACE" => "TRACE",
        "DEBUG" => "DEBUG",
        "INFO" => "INFO",
        "WARN" => "WARN",
        "ERROR" => "ERROR",
        "FATAL" => "FATAL",
        _ => "INFO",
    }
}

fn format_parts(parts: &LineParts) -> String {
    let mut fields = String::new();
    for (key, value) in &parts.fields {
        if key.is_empty() || value.is_empty() {
            continue;
        }
        if !fields.is_empty() {
            fields.push(';');
        }
        fields.push_str(&sanitize_token(key));
        fields.push(':');
        fields.push_str(&sanitize_token(value));
    }
    let node = if parts.node.is_empty() {
        "-".to_string()
    } else {
        sanitize_token(&parts.node)
    };
    format!(
        "MCAP\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{fields}\t{}",
        parts.log_ns,
        parts.level,
        sanitize_cell(&parts.topic),
        sanitize_cell(&parts.schema),
        if parts.log_like { "1" } else { "0" },
        node,
        parts.publish_ns,
        parts.stamp_ns,
        sanitize_cell(&parts.body),
    )
}

fn history_line(
    stamp: i64,
    topic: &str,
    field: &str,
    node: &str,
    value: &str,
    count: u64,
    raw: bool,
) -> String {
    let body = if count > 1 {
        format!("{value} ×{count}")
    } else {
        value.to_string()
    };
    format_parts(&LineParts {
        log_ns: stamp,
        level: "INFO",
        topic: topic.to_string(),
        schema: field.to_string(),
        log_like: false,
        node: node.to_string(),
        publish_ns: stamp,
        stamp_ns: stamp,
        fields: vec![("series".into(), series_key(topic, field, raw))],
        body,
    })
}

fn series_key(topic: &str, field: &str, raw: bool) -> String {
    let topic = topic.replace(['#', ' ', '"'], "_");
    let field = field.replace(['#', ' ', '"'], "_");
    if raw {
        format!("{topic}#{field}#raw")
    } else {
        format!("{topic}#{field}")
    }
}

fn field_term(key: &str, value: &str) -> String {
    let term = format!("field:{key}={value}");
    if term.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        format!("\"{}\"", term.replace('"', ""))
    } else {
        term
    }
}

fn sanitize_cell(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(BODY_LIMIT));
    for ch in text.chars().take(BODY_LIMIT) {
        if matches!(ch, '\n' | '\r' | '\t') {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn sanitize_token(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars().take(256) {
        if matches!(ch, '\n' | '\r' | '\t' | ';' | ':') {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn fold_catalog(catalog: &mut McapCatalog, message: &Built) {
    let topic = if let Some(topic) = catalog
        .topics
        .iter_mut()
        .find(|topic| topic.name == message.topic)
    {
        topic
    } else {
        catalog.topics.push(McapTopic {
            name: message.topic.clone(),
            schema: message.schema.clone(),
            log_like: message.log_like,
            count: 0,
            first_ns: message.order,
            last_ns: message.order,
            props: Vec::new(),
        });
        catalog.topics.last_mut().unwrap()
    };
    if topic.count == 0 {
        topic.first_ns = message.order;
    }
    topic.count += 1;
    topic.last_ns = message.order;
    topic.log_like |= message.log_like;
    for (path, value) in &message.fields {
        if path == "series" {
            continue;
        }
        note_prop(&mut topic.props, path, value);
    }
    if message.node.is_empty() || message.node == "-" {
        return;
    }
    let node = if let Some(node) = catalog
        .nodes
        .iter_mut()
        .find(|node| node.name == message.node)
    {
        node
    } else {
        catalog.nodes.push(McapNode {
            name: message.node.clone(),
            count: 0,
            first_ns: message.order,
            last_ns: message.order,
            topics: Vec::new(),
        });
        catalog.nodes.last_mut().unwrap()
    };
    if node.count == 0 {
        node.first_ns = message.order;
    }
    node.count += 1;
    node.last_ns = message.order;
    if !node.topics.iter().any(|topic| topic == &message.topic) {
        node.topics.push(message.topic.clone());
    }
}

fn note_prop(props: &mut Vec<McapProp>, path: &str, value: &str) {
    let numeric = value.parse::<f64>().ok().filter(|n| n.is_finite());
    let prop = if let Some(prop) = props.iter_mut().find(|prop| prop.path == path) {
        prop
    } else {
        props.push(McapProp {
            path: path.to_string(),
            numeric: numeric.is_some(),
            last: value.to_string(),
            min: numeric,
            max: numeric,
            changes: 1,
            count: 1,
        });
        return;
    };
    if prop.last != value {
        prop.changes += 1;
    }
    prop.count += 1;
    prop.last = value.to_string();
    if let Some(number) = numeric {
        prop.numeric = true;
        prop.min = Some(prop.min.map_or(number, |min| min.min(number)));
        prop.max = Some(prop.max.map_or(number, |max| max.max(number)));
    }
}

fn walk_chunk(
    raw: &[u8],
    schemas: &mut BTreeMap<u16, SchemaRec>,
    channels: &mut BTreeMap<u16, ChannelRec>,
    built: &mut Vec<Built>,
    seq: &mut u64,
) -> Result<(), String> {
    let mut offset = 0;
    while offset + 9 <= raw.len() {
        let op = raw[offset];
        let len = u64::from_le_bytes(raw[offset + 1..offset + 9].try_into().unwrap()) as usize;
        let start = offset + 9;
        let Some(end) = start.checked_add(len) else {
            return Err("Chunk 内记录长度无效".to_string());
        };
        if end > raw.len() {
            return Err("Chunk 内记录不完整".to_string());
        }
        let data = &raw[start..end];
        match op {
            0x03 => {
                if let Some((id, rec)) = parse_schema(data) {
                    schemas.insert(id, rec);
                }
            }
            0x04 => {
                if let Some((id, rec)) = parse_channel(data) {
                    channels.insert(id, rec);
                }
            }
            0x05 => {
                *seq += 1;
                if let Some(message) = parse_message(data, schemas, channels, *seq) {
                    built.push(message);
                }
            }
            _ => {}
        }
        offset = end;
    }
    if offset < raw.len() && !raw[offset..].iter().all(|byte| *byte == 0) {
        return Err("Chunk 尾部记录不完整".to_string());
    }
    Ok(())
}

fn inflate_chunk(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 8 + 8 + 8 + 4 {
        return Err("Chunk 头不完整".to_string());
    }
    let mut rest = data;
    let _start = take_u64(&mut rest).unwrap();
    let _end = take_u64(&mut rest).unwrap();
    let size = take_u64(&mut rest).unwrap() as usize;
    let _crc = take_u32(&mut rest).unwrap();
    let compression = take_string(&mut rest).ok_or("Chunk 压缩字段无效")?;
    let records = take_bytes_u64(&mut rest).ok_or("Chunk 数据无效")?;
    inflate(&compression, records, size)
}

fn inflate(compression: &str, data: &[u8], size: usize) -> Result<Vec<u8>, String> {
    match compression {
        "" => Ok(data.to_vec()),
        "lz4" => lz4_flex::block::decompress(data, size).map_err(|error| format!("lz4: {error}")),
        "zstd" => {
            zstd::bulk::decompress(data, size.max(1)).map_err(|error| format!("zstd: {error}"))
        }
        other => Err(format!("不支持的压缩 {other}")),
    }
}

fn parse_schema(data: &[u8]) -> Option<(u16, SchemaRec)> {
    let mut rest = data;
    let id = take_u16(&mut rest)?;
    let name = take_string(&mut rest)?;
    let encoding = take_string(&mut rest)?;
    Some((id, SchemaRec { name, encoding }))
}

fn parse_channel(data: &[u8]) -> Option<(u16, ChannelRec)> {
    let mut rest = data;
    let id = take_u16(&mut rest)?;
    let schema_id = take_u16(&mut rest)?;
    let topic = take_string(&mut rest)?;
    let encoding = take_string(&mut rest)?;
    let metadata = take_map(&mut rest).unwrap_or_default();
    let node = metadata
        .iter()
        .find(|(key, _)| matches!(key.as_str(), "callerid" | "node" | "name"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    Some((
        id,
        ChannelRec {
            schema_id,
            topic,
            encoding,
            node,
        },
    ))
}

fn parse_message(
    data: &[u8],
    schemas: &BTreeMap<u16, SchemaRec>,
    channels: &BTreeMap<u16, ChannelRec>,
    seq: u64,
) -> Option<Built> {
    let mut rest = data;
    let channel_id = take_u16(&mut rest)?;
    let _sequence = take_u32(&mut rest)?;
    let log_time = take_u64(&mut rest)?;
    let publish_time = take_u64(&mut rest)?;
    let payload = rest;
    let channel = channels.get(&channel_id)?;
    let schema = schemas.get(&channel.schema_id);
    let schema_name = schema.map(|s| s.name.as_str()).unwrap_or("");
    let schema_encoding = schema.map(|s| s.encoding.as_str()).unwrap_or("");
    let mut decoded = decode_payload(schema_name, schema_encoding, &channel.encoding, payload);
    if decoded.node.is_empty() && !channel.node.is_empty() {
        decoded.node = channel.node.clone();
    }
    let log_ns = u64_ns(log_time);
    let stamp_ns = if decoded.stamp_ns > 0 {
        decoded.stamp_ns
    } else {
        0
    };
    let order = if stamp_ns > 0 { stamp_ns } else { log_ns };
    Some(Built {
        order,
        seq,
        log_ns,
        stamp_ns,
        level: decoded.level,
        topic: channel.topic.clone(),
        schema: if schema_name.is_empty() {
            channel.encoding.clone()
        } else {
            format!("{}/{}", channel.encoding, schema_name)
        },
        log_like: decoded.log_like,
        node: decoded.node,
        publish_ns: u64_ns(publish_time),
        fields: decoded.fields,
        body: decoded.body,
    })
}

struct Payload {
    level: String,
    node: String,
    stamp_ns: i64,
    log_like: bool,
    fields: Vec<(String, String)>,
    body: String,
}

fn decode_payload(
    schema: &str,
    schema_encoding: &str,
    message_encoding: &str,
    payload: &[u8],
) -> Payload {
    let schema_l = schema.to_ascii_lowercase();
    let is_log_schema = schema_l.ends_with("/log")
        || schema_l.ends_with(".log")
        || schema_l == "log"
        || (schema_l.contains("log")
            && (schema_l.contains("rcl")
                || schema_l.contains("rosgraph")
                || schema_l.contains("foxglove")));
    if message_encoding.eq_ignore_ascii_case("json")
        || schema_encoding.eq_ignore_ascii_case("jsonschema")
        || payload.first() == Some(&b'{')
    {
        if let Ok(text) = std::str::from_utf8(payload) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                return payload_from_json(&value, is_log_schema || schema_l.contains("log"));
            }
        }
    }
    if is_log_schema && (message_encoding.eq_ignore_ascii_case("cdr") || payload.len() > 4) {
        if let Some(parsed) = decode_cdr_log(payload) {
            return parsed;
        }
    }
    if schema_l.contains("image") {
        if let Some((width, height)) = decode_cdr_image_size(payload) {
            return Payload {
                level: "INFO".into(),
                node: String::new(),
                stamp_ns: 0,
                log_like: false,
                fields: vec![
                    ("width".into(), width.to_string()),
                    ("height".into(), height.to_string()),
                ],
                body: format!("{width}x{height} {}B", payload.len()),
            };
        }
    }
    Payload {
        level: "INFO".into(),
        node: String::new(),
        stamp_ns: 0,
        log_like: false,
        fields: Vec::new(),
        body: format!("binary {}B", payload.len()),
    }
}

fn payload_from_json(value: &serde_json::Value, schema_log: bool) -> Payload {
    let mut fields = Vec::new();
    walk_json(value, "", &mut fields, 0);
    let level = json_level(value).unwrap_or_else(|| "INFO".to_string());
    let node = json_string(value, &["name", "node", "callerid"]).unwrap_or_default();
    let stamp_ns = json_stamp(value);
    let msg = json_string(value, &["msg", "message", "text"]).unwrap_or_default();
    let log_like = schema_log || (!msg.is_empty() && json_level(value).is_some());
    let body = if !msg.is_empty() {
        msg
    } else if log_like {
        serde_json::to_string(value).unwrap_or_default()
    } else {
        format!(
            "json {}B",
            serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
        )
    };
    Payload {
        level,
        node,
        stamp_ns,
        log_like,
        fields,
        body,
    }
}

fn walk_json(
    value: &serde_json::Value,
    prefix: &str,
    out: &mut Vec<(String, String)>,
    depth: usize,
) {
    if depth > 4 || out.len() >= 24 {
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                walk_json(child, &path, out, depth + 1);
            }
        }
        serde_json::Value::Array(items) => {
            if !prefix.is_empty() {
                out.push((format!("{prefix}.len"), items.len().to_string()));
            }
        }
        serde_json::Value::String(text) => {
            if !prefix.is_empty() && !text.is_empty() {
                out.push((prefix.to_string(), text.clone()));
            }
        }
        serde_json::Value::Number(number) => {
            if !prefix.is_empty() {
                out.push((prefix.to_string(), number.to_string()));
            }
        }
        serde_json::Value::Bool(flag) => {
            if !prefix.is_empty() {
                out.push((prefix.to_string(), flag.to_string()));
            }
        }
        serde_json::Value::Null => {}
    }
}

fn json_level(value: &serde_json::Value) -> Option<String> {
    let raw = value.get("level").or_else(|| value.get("severity"))?;
    match raw {
        serde_json::Value::String(text) => {
            crate::plugin::Severity::parse(text).map(|s| s.label().to_string())
        }
        serde_json::Value::Number(number) => number.as_u64().and_then(level_from_code),
        _ => None,
    }
}

fn level_from_code(code: u64) -> Option<String> {
    let label = match code {
        1 | 10 => "DEBUG",
        2 | 20 => "INFO",
        3 | 30 => "WARN",
        4 | 40 => "ERROR",
        5 | 50 => "FATAL",
        _ => return None,
    };
    Some(label.to_string())
}

fn json_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(serde_json::Value::String(text)) = value.get(*key) {
            if !text.is_empty() {
                return Some(text.clone());
            }
        }
    }
    None
}

fn json_stamp(value: &serde_json::Value) -> i64 {
    let stamp = value
        .get("header")
        .and_then(|header| header.get("stamp"))
        .or_else(|| value.get("timestamp"))
        .or_else(|| value.get("stamp"));
    let Some(stamp) = stamp else {
        return 0;
    };
    if let Some(number) = stamp.as_i64() {
        return if number > 10_000_000_000 {
            number
        } else {
            number.saturating_mul(1_000_000_000)
        };
    }
    if let Some(number) = stamp.as_f64() {
        if number > 10_000_000_000.0 {
            return number as i64;
        }
        return (number * 1_000_000_000.0) as i64;
    }
    let sec = stamp
        .get("sec")
        .or_else(|| stamp.get("secs"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let nsec = stamp
        .get("nanosec")
        .or_else(|| stamp.get("nsec"))
        .or_else(|| stamp.get("nsecs"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if sec == 0 && nsec == 0 {
        0
    } else {
        sec.saturating_mul(1_000_000_000).saturating_add(nsec)
    }
}

fn decode_cdr_log(payload: &[u8]) -> Option<Payload> {
    let mut cdr = Cdr {
        data: payload,
        pos: 0,
    };
    cdr.header()?;
    let sec = cdr.i32()?;
    let nsec = cdr.u32()?;
    let level = cdr.u8()?;
    let name = cdr.string()?;
    let msg = cdr.string()?;
    let label = match level {
        10 => "DEBUG",
        20 => "INFO",
        30 => "WARN",
        40 => "ERROR",
        50 => "FATAL",
        _ => return None,
    };
    let stamp_ns = (sec as i64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as i64);
    Some(Payload {
        level: label.into(),
        node: name.clone(),
        stamp_ns,
        log_like: true,
        fields: vec![("name".into(), name)],
        body: msg,
    })
}

fn decode_cdr_image_size(payload: &[u8]) -> Option<(u32, u32)> {
    let mut cdr = Cdr {
        data: payload,
        pos: 0,
    };
    cdr.header()?;
    let _sec = cdr.i32()?;
    let _nsec = cdr.u32()?;
    let _frame = cdr.string()?;
    let height = cdr.u32()?;
    let width = cdr.u32()?;
    if width == 0 || height == 0 || width > 100_000 || height > 100_000 {
        return None;
    }
    Some((width, height))
}

struct Cdr<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cdr<'a> {
    fn header(&mut self) -> Option<()> {
        if self.data.len() < 4 {
            return None;
        }
        self.pos = 4;
        Some(())
    }

    fn align(&mut self, n: usize) {
        let mis = self.pos % n;
        if mis != 0 {
            self.pos += n - mis;
        }
    }

    fn u8(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(byte)
    }

    fn u32(&mut self) -> Option<u32> {
        self.align(4);
        let bytes = self.data.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        if len == 0 || self.pos + len > self.data.len() {
            return None;
        }
        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(len);
        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }
}

fn parse_attachment(data: &[u8], seq: u64) -> Option<Built> {
    let mut rest = data;
    let log_time = take_u64(&mut rest)?;
    let _create = take_u64(&mut rest)?;
    let name = take_string(&mut rest)?;
    let media = take_string(&mut rest)?;
    let bytes = take_bytes_u64(&mut rest)?;
    Some(Built {
        order: u64_ns(log_time),
        seq,
        log_ns: u64_ns(log_time),
        stamp_ns: 0,
        level: "INFO".into(),
        topic: name,
        schema: "attachment".into(),
        log_like: true,
        node: String::new(),
        publish_ns: 0,
        fields: vec![("media".into(), media.clone())],
        body: format!("{media} {}B", bytes.len()),
    })
}

fn parse_metadata(data: &[u8], seq: u64) -> Option<Built> {
    let mut rest = data;
    let name = take_string(&mut rest)?;
    let map = take_map(&mut rest).unwrap_or_default();
    let body = map
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    Some(Built {
        order: 1,
        seq,
        log_ns: 1,
        stamp_ns: 0,
        level: "INFO".into(),
        topic: format!("/metadata/{name}"),
        schema: "metadata".into(),
        log_like: true,
        node: String::new(),
        publish_ns: 0,
        fields: map,
        body,
    })
}

fn u64_ns(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn take_u16(data: &mut &[u8]) -> Option<u16> {
    let bytes = data.get(..2)?;
    *data = &data[2..];
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

fn take_u32(data: &mut &[u8]) -> Option<u32> {
    let bytes = data.get(..4)?;
    *data = &data[4..];
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn take_u64(data: &mut &[u8]) -> Option<u64> {
    let bytes = data.get(..8)?;
    *data = &data[8..];
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

fn take_string(data: &mut &[u8]) -> Option<String> {
    let len = take_u32(data)? as usize;
    if data.len() < len {
        return None;
    }
    let text = String::from_utf8_lossy(&data[..len]).into_owned();
    *data = &data[len..];
    Some(text)
}

fn take_bytes_u64<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = take_u64(data)? as usize;
    if data.len() < len {
        return None;
    }
    let bytes = &data[..len];
    *data = &data[len..];
    Some(bytes)
}

fn take_map(data: &mut &[u8]) -> Option<Vec<(String, String)>> {
    let len = take_u32(data)? as usize;
    if data.len() < len {
        return None;
    }
    let mut body = &data[..len];
    *data = &data[len..];
    let mut pairs = Vec::new();
    while !body.is_empty() {
        let key = take_string(&mut body)?;
        let value = take_string(&mut body)?;
        pairs.push((key, value));
    }
    Some(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_record(buf: &mut Vec<u8>, op: u8, payload: &[u8]) {
        buf.push(op);
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(payload);
    }

    fn push_string(buf: &mut Vec<u8>, text: &str) {
        buf.extend_from_slice(&(text.len() as u32).to_le_bytes());
        buf.extend_from_slice(text.as_bytes());
    }

    fn sample_bag() -> Vec<u8> {
        let mut buf = Vec::from(MAGIC);
        let mut schema = Vec::new();
        schema.extend_from_slice(&1u16.to_le_bytes());
        push_string(&mut schema, "foxglove.Log");
        push_string(&mut schema, "jsonschema");
        schema.extend_from_slice(&0u32.to_le_bytes());
        push_record(&mut buf, 0x03, &schema);

        let mut channel = Vec::new();
        channel.extend_from_slice(&1u16.to_le_bytes());
        channel.extend_from_slice(&1u16.to_le_bytes());
        push_string(&mut channel, "/rosout");
        push_string(&mut channel, "json");
        channel.extend_from_slice(&0u32.to_le_bytes());
        push_record(&mut buf, 0x04, &channel);

        let mut image_schema = Vec::new();
        image_schema.extend_from_slice(&2u16.to_le_bytes());
        push_string(&mut image_schema, "sensor_msgs/msg/Image");
        push_string(&mut image_schema, "");
        image_schema.extend_from_slice(&0u32.to_le_bytes());
        push_record(&mut buf, 0x03, &image_schema);

        let mut image_channel = Vec::new();
        image_channel.extend_from_slice(&2u16.to_le_bytes());
        image_channel.extend_from_slice(&2u16.to_le_bytes());
        push_string(&mut image_channel, "/camera/image");
        push_string(&mut image_channel, "cdr");
        image_channel.extend_from_slice(&0u32.to_le_bytes());
        push_record(&mut buf, 0x04, &image_channel);

        for (ns, temp, level) in [
            (1_000_000_000u64, 1, "ERROR"),
            (2_000_000_000u64, 1, "INFO"),
            (3_000_000_000u64, 2, "INFO"),
        ] {
            let payload = format!(
                "{{\"level\":\"{level}\",\"name\":\"lidar\",\"msg\":\"temp\",\"temp\":{temp},\"header\":{{\"stamp\":{{\"sec\":{},\"nanosec\":0}}}}}}",
                ns / 1_000_000_000
            );
            let mut message = Vec::new();
            message.extend_from_slice(&1u16.to_le_bytes());
            message.extend_from_slice(&1u32.to_le_bytes());
            message.extend_from_slice(&ns.to_le_bytes());
            message.extend_from_slice(&ns.to_le_bytes());
            message.extend_from_slice(payload.as_bytes());
            push_record(&mut buf, 0x05, &message);
        }

        let mut image = Vec::new();
        image.extend_from_slice(&2u16.to_le_bytes());
        image.extend_from_slice(&1u32.to_le_bytes());
        image.extend_from_slice(&1_500_000_000u64.to_le_bytes());
        image.extend_from_slice(&1_500_000_000u64.to_le_bytes());
        image.extend_from_slice(&[0, 0, 0, 0]);
        push_record(&mut buf, 0x05, &image);
        buf
    }

    #[test]
    fn decodes_messages_without_keeping_binary_payloads() {
        let decoded = decode(&sample_bag());
        assert!(
            decoded.catalog.error.is_none(),
            "{:?}",
            decoded.catalog.error
        );
        let lines: Vec<_> = decoded.text.lines().collect();
        assert_eq!(lines.len(), 4);
        let error = parse_line(lines[0]).unwrap();
        assert_eq!(error.topic, "/rosout");
        assert_eq!(error.level, "ERROR");
        assert_eq!(error.log_flag, "1");
        assert_eq!(field_of(&error, "temp"), Some("1"));
        assert_eq!(error.node, "lidar");
        let image = parse_line(lines[1]).unwrap();
        assert_eq!(image.topic, "/camera/image");
        assert_eq!(image.log_flag, "0");
        assert!(image.body.contains("binary"));
        let ros = decoded
            .catalog
            .topics
            .iter()
            .find(|t| t.name == "/rosout")
            .unwrap();
        assert!(ros.log_like);
        assert_eq!(ros.count, 3);
        let temp = ros.props.iter().find(|p| p.path == "temp").unwrap();
        assert_eq!(temp.min, Some(1.0));
        assert_eq!(temp.max, Some(2.0));
        assert_eq!(temp.changes, 2);
        let node = decoded
            .catalog
            .nodes
            .iter()
            .find(|n| n.name == "lidar")
            .unwrap();
        assert_eq!(node.count, 3);
    }

    #[test]
    fn broken_tail_keeps_earlier_messages() {
        let mut bytes = sample_bag();
        bytes.push(0x05);
        bytes.extend_from_slice(&1000u64.to_le_bytes());
        let decoded = decode(&bytes);
        assert!(decoded.catalog.error.unwrap().contains("不完整"));
        assert_eq!(decoded.text.lines().count(), 4);
    }

    #[test]
    fn history_collapses_repeated_values() {
        let decoded = decode(&sample_bag());
        let lines: Vec<_> = decoded.text.lines().collect();
        let (text, truncated) = project_history(&lines, "/rosout", "temp", false);
        assert!(!truncated);
        let rows: Vec<_> = text.lines().collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains('×') || rows[0].contains("temp"));
        assert!(parse_line(rows[0]).unwrap().body.starts_with('1'));
        assert!(parse_line(rows[1]).unwrap().body.starts_with('2'));
    }

    #[test]
    fn lz4_chunk_roundtrip() {
        let mut inner = Vec::new();
        let mut schema = Vec::new();
        schema.extend_from_slice(&1u16.to_le_bytes());
        push_string(&mut schema, "foxglove.Log");
        push_string(&mut schema, "jsonschema");
        schema.extend_from_slice(&0u32.to_le_bytes());
        push_record(&mut inner, 0x03, &schema);
        let mut channel = Vec::new();
        channel.extend_from_slice(&1u16.to_le_bytes());
        channel.extend_from_slice(&1u16.to_le_bytes());
        push_string(&mut channel, "/rosout");
        push_string(&mut channel, "json");
        channel.extend_from_slice(&0u32.to_le_bytes());
        push_record(&mut inner, 0x04, &channel);
        let payload = br#"{"level":"WARN","name":"cam","msg":"dark"}"#;
        let mut message = Vec::new();
        message.extend_from_slice(&1u16.to_le_bytes());
        message.extend_from_slice(&1u32.to_le_bytes());
        message.extend_from_slice(&5_000_000_000u64.to_le_bytes());
        message.extend_from_slice(&5_000_000_000u64.to_le_bytes());
        message.extend_from_slice(payload);
        push_record(&mut inner, 0x05, &message);

        let compressed = lz4_flex::block::compress(&inner);
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&0u64.to_le_bytes());
        chunk.extend_from_slice(&0u64.to_le_bytes());
        chunk.extend_from_slice(&(inner.len() as u64).to_le_bytes());
        chunk.extend_from_slice(&0u32.to_le_bytes());
        push_string(&mut chunk, "lz4");
        chunk.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
        chunk.extend_from_slice(&compressed);

        let mut buf = Vec::from(MAGIC);
        push_record(&mut buf, 0x06, &chunk);
        let decoded = decode(&buf);
        assert!(
            decoded.catalog.error.is_none(),
            "{:?}",
            decoded.catalog.error
        );
        let line = parse_line(decoded.text.lines().next().unwrap()).unwrap();
        assert_eq!(line.level, "WARN");
        assert_eq!(line.topic, "/rosout");
        assert_eq!(line.body, "dark");
    }
}
