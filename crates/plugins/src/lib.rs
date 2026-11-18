mod declarative;
mod generic;
mod jsonl;
mod python;
mod ros2;

pub use declarative::{load_dir, DeclarativePlugin, LoadReport};
pub use generic::GenericPlugin;
pub use jsonl::JsonLinesPlugin;
pub use python::PythonLogPlugin;
pub use ros2::Ros2Plugin;

use std::sync::Arc;

use neolog_core::plugin::LogPlugin;

pub fn builtin_plugins() -> Vec<Arc<dyn LogPlugin>> {
    vec![
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
}
