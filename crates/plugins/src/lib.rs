mod declarative;
mod generic;
mod jsonl;
mod mcap;
mod python;
mod ros2;

pub use declarative::{load_dir, DeclarativePlugin, LoadReport};
pub use generic::GenericPlugin;
pub use jsonl::JsonLinesPlugin;
pub use mcap::McapPlugin;
pub use python::PythonLogPlugin;
pub use ros2::Ros2Plugin;

use std::sync::Arc;

use neolog_core::plugin::LogPlugin;

pub fn builtin_plugins() -> Vec<Arc<dyn LogPlugin>> {
    vec![
        Arc::new(McapPlugin),
        Arc::new(Ros2Plugin),
        Arc::new(PythonLogPlugin),
        Arc::new(JsonLinesPlugin),
        Arc::new(GenericPlugin),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use neolog_core::{PluginRegistry, Session};

    fn registry() -> PluginRegistry {
        PluginRegistry::new(builtin_plugins())
    }

    const ROS: &str = "\
[INFO] [1730380800.000000001] [camera]: frame captured
[ERROR] [1730380802.500000000] [lidar]: timeout waiting for data
";

    #[test]
    fn detects_ros2_and_parses_fields() {
        let mut s = Session::new(registry());
        let doc = s.open_text("ros.log", ROS);
        let d = s.doc(doc).unwrap();
        assert_eq!(d.plugin_id, "ros2");
        assert_eq!(d.time_at(0), Some(1_730_380_800_000_000_001));
        let plugin = s.plugin_for(doc);
        assert_eq!(
            plugin.severity("[ERROR] [1.0] [lidar]: x"),
            Some(neolog_core::Severity::Error)
        );
        assert_eq!(plugin.module("[ERROR] [1.0] [lidar]: x"), Some("lidar"));
        let mut spans = Vec::new();
        let line = "[ERROR] [1.0] [lidar]: timeout";
        plugin.decorate(line, &mut spans);
        assert!(spans.iter().any(|sp| sp.start == 1 && sp.end == 6));
        let node_at = line.find("lidar").unwrap();
        assert!(spans.iter().any(|sp| sp.start == node_at));
        let timeout_at = line.find("timeout").unwrap();
        assert!(spans.iter().any(|sp| sp.start == timeout_at));

        let columns = plugin.columns(line).unwrap();
        assert_eq!(columns.level.unwrap().text, "ERROR");
        let module = columns.module.unwrap();
        assert_eq!(module.range, Some((node_at, node_at + 5)));
        assert_eq!(columns.message.range, Some((timeout_at, line.len())));
    }

    #[test]
    fn detects_python_logging() {
        let mut s = Session::new(registry());
        let doc = s.open_text(
            "py.log",
            "2026-10-31 10:00:00,123 - camera - ERROR - sensor lost\n\
             2026-10-31 10:00:01,000 - main - INFO - retry\n",
        );
        let d = s.doc(doc).unwrap();
        assert_eq!(d.plugin_id, "python");
        assert_eq!(s.plugin_for(doc).module(d.line_text(0)), Some("camera"));
        assert_eq!(
            s.plugin_for(doc).severity(d.line_text(0)),
            Some(neolog_core::Severity::Error)
        );
    }

    #[test]
    fn generic_catches_plain_logs() {
        let mut s = Session::new(registry());
        let doc = s.open_text(
            "app.log",
            "2026-10-31 10:00:00.000 INFO boot\nWARN battery low\n",
        );
        assert_eq!(s.doc(doc).unwrap().plugin_id, "generic");
    }

    #[test]
    fn merged_ros_and_python_logs_are_time_ordered() {
        let mut s = Session::new(registry());
        let ros = s.open_text("ros.log", ROS);
        let py = s.open_text(
            "py.log",
            "2026-10-31 10:00:00,500 - main - INFO - started\n",
        );
        let view = s.create_merged_view(&[ros, py], "").unwrap();
        let first = s.line_ref(view, 0).unwrap();
        let last = s.line_ref(view, 2).unwrap();
        assert_eq!(first.doc, ros);
        assert_eq!(last.doc, py);
    }

    #[test]
    fn mcap_history_is_a_child_of_the_bag() {
        let dir = std::env::temp_dir().join(format!("neolog-mcap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bag.mcap");
        std::fs::write(&path, sample_mcap()).unwrap();
        let mut session = Session::new(registry());
        let doc = session.open_path(&path).unwrap();
        assert_eq!(session.doc(doc).unwrap().plugin_id, "mcap");
        let root = session.create_doc_view(doc).unwrap();
        let log = session.view(root).unwrap().children[0];
        assert_eq!(session.view(log).unwrap().title, "日志");
        assert!(session.visible_len(log) >= 1);
        assert!(session.visible_len(root) > session.visible_len(log));
        let topic = session
            .open_mcap_history(root, neolog_core::HistoryKind::Topic("/rosout".into()))
            .unwrap();
        let again = session
            .open_mcap_history(root, neolog_core::HistoryKind::Topic("/rosout".into()))
            .unwrap();
        assert_eq!(topic, again);
        assert!(session.visible_len(topic) >= 1);
        let changes = session
            .open_mcap_history(
                root,
                neolog_core::HistoryKind::Changes {
                    topic: "/rosout".into(),
                    field: "temp".into(),
                },
            )
            .unwrap();
        assert_eq!(session.visible_len(changes), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_json_lines_and_fields() {
        let mut s = Session::new(registry());
        let doc = s.open_text(
            "app.jsonl",
            "{\"time\":1730380800000,\"level\":\"ERROR\",\"module\":\"lidar\",\"msg\":\"timeout\"}\n\
             {\"time\":1730380801000,\"level\":\"INFO\",\"module\":\"camera\",\"msg\":\"ok\"}\n",
        );
        let d = s.doc(doc).unwrap();
        assert_eq!(d.plugin_id, "jsonl");
        assert_eq!(d.time_at(0), Some(1_730_380_800_000_000_000));
        let plugin = s.plugin_for(doc);
        assert_eq!(
            plugin.field(d.line_text(0), "module"),
            Some("lidar".to_string())
        );
        assert_eq!(
            plugin.severity(d.line_text(0)),
            Some(neolog_core::Severity::Error)
        );
    }

    fn push_string(buf: &mut Vec<u8>, text: &str) {
        buf.extend_from_slice(&(text.len() as u32).to_le_bytes());
        buf.extend_from_slice(text.as_bytes());
    }

    fn push_record(buf: &mut Vec<u8>, op: u8, payload: &[u8]) {
        buf.push(op);
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(payload);
    }

    fn sample_mcap() -> Vec<u8> {
        let mut buf = vec![0x89, b'M', b'C', b'A', b'P', b'0', b'\r', b'\n'];
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
        for (sec, temp) in [(1u64, 1), (2u64, 1), (3u64, 2)] {
            let ns = sec * 1_000_000_000;
            let payload = format!(
                "{{\"level\":\"INFO\",\"name\":\"lidar\",\"msg\":\"temp\",\"temp\":{temp}}}"
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
        image.extend_from_slice(&[9, 9, 9, 9]);
        push_record(&mut buf, 0x05, &image);
        buf
    }
}
