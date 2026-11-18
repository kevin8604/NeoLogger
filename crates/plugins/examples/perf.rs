use std::time::{Duration, Instant};

use neolog_core::matcher::{Needle, SearchMatcher, SearchOptions};
use neolog_core::{PluginRegistry, Session, Severity, ViewId};
use neolog_plugins::builtin_plugins;

const LINES: usize = 1_000_000;

fn time<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = f();
    report(label, start.elapsed());
    value
}

fn report(label: &str, elapsed: Duration) {
    println!("{label:<34} {:>9.1} ms", elapsed.as_secs_f64() * 1000.0);
}

fn build_log(fields: bool) -> String {
    let modules = [
        "mission_planner",
        "camera_driver",
        "lidar_driver",
        "controller",
        "battery_monitor",
    ];
    let mut text = String::with_capacity(LINES * 96);
    for i in 0..LINES {
        let level = match i % 20 {
            0 => "ERROR",
            1..=3 => "WARN",
            _ => "INFO",
        };
        let secs = 1_793_440_800 + i / 1000;
        let ms = i % 1000;
        text.push_str(&format!(
            "[{level}] [{secs}.{ms:09}] [{}]: packet {i} from 192.168.1.20 latency {}ms",
            modules[i % modules.len()],
            i % 500
        ));
        if fields {
            text.push_str(&format!(" level={level} frame={i}"));
        }
        text.push('\n');
    }
    text
}

fn main() {
    let text = time("generate 1M-line text", || build_log(false));
    let registry = PluginRegistry::new(builtin_plugins());
    let mut session = Session::new(registry);

    let doc = time("open_text (index + times)", || {
        session.open_text("bench_ros2.log", &text)
    });
    let base = session.create_doc_view(doc).unwrap();
    println!(
        "  docs={} lines={} plugin={}",
        session.docs().len(),
        session.visible_len(base),
        session.plugin_id_of(doc)
    );

    for query in [
        "level>=warn",
        "level=error",
        "module:lidar",
        "field:level=INFO",
        "timeout",
        r#""re:latency 49[0-9]ms""#,
    ] {
        let child = time(&format!("filter {query:?}"), || {
            session.fork_view(base, query).unwrap()
        });
        println!("  rows={}", session.visible_len(child));
    }

    // The ROS log above has no logfmt tokens, so `field:` only exercises the
    // absent-key reject path. Measure a log source that actually carries
    // `key=value` fields as well.
    drop(text);
    let logfmt = time("generate 1M-line logfmt text", || build_log(true));
    let field_doc = time("open_text (logfmt)", || {
        session.open_text("bench_logfmt.log", &logfmt)
    });
    let field_base = session.create_doc_view(field_doc).unwrap();
    for query in ["field:level=INFO", "field:frame=999999"] {
        let child = time(&format!("filter {query:?} (logfmt)"), || {
            session.fork_view(field_base, query).unwrap()
        });
        println!("  rows={}", session.visible_len(child));
    }
    drop(logfmt);

    let matcher = SearchMatcher::build("192.168.1.20", SearchOptions::default())
        .unwrap()
        .unwrap();
    let mut hits = 0usize;
    let mut buf = Vec::new();
    time("find scan (plain)", || {
        for row in 0..session.visible_len(base) {
            let lr = session.line_ref(base, row).unwrap();
            buf.clear();
            matcher.find_all(session.line_text(lr), &mut buf);
            hits += buf.len();
        }
    });
    println!("  hits={hits}");

    let regex_matcher = SearchMatcher::build(
        "latency 49[0-9]ms",
        SearchOptions {
            regex: true,
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    let mut hits = 0usize;
    time("find scan (regex)", || {
        for row in 0..session.visible_len(base) {
            let lr = session.line_ref(base, row).unwrap();
            buf.clear();
            regex_matcher.find_all(session.line_text(lr), &mut buf);
            hits += buf.len();
        }
    });
    println!("  hits={hits}");

    per_row_cost(&session, base);
    stats_scan(&session, base);
    regex_ceiling(&session, base);
}

/// Compares a per-row regex scan with and without the literal prefilter that
/// `Filter::Regex` now applies.
fn regex_ceiling(session: &Session, view: ViewId) {
    let re = regex::Regex::new(r"(?i)latency 49[0-9]ms").unwrap();
    let needle = Needle::new("latency 49", false);
    let rows = session.visible_len(view);

    let hits = time("regex loop (no prefilter)", || {
        let mut hits = 0;
        for row in 0..rows {
            let Some(lr) = session.line_ref(view, row) else {
                continue;
            };
            if re.is_match(session.line_text(lr)) {
                hits += 1;
            }
        }
        hits
    });
    println!("  hits={hits}");

    let hits = time("regex loop (prefilter)", || {
        let mut hits = 0;
        for row in 0..rows {
            let Some(lr) = session.line_ref(view, row) else {
                continue;
            };
            let text = session.line_text(lr);
            if needle.contains(text) && re.is_match(text) {
                hits += 1;
            }
        }
        hits
    });
    println!("  hits={hits}");
}

/// Full-file pass used by the stats panel: severity bucket + module counts.
fn stats_scan(session: &Session, view: ViewId) {
    let start = Instant::now();
    let rows = session.visible_len(view);
    let mut severities = [0usize; 6];
    let mut modules: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for row in 0..rows {
        let Some(lr) = session.line_ref(view, row) else {
            continue;
        };
        let text = session.line_text(lr);
        let plugin = session.plugin_for(lr.doc);
        if let Some(level) = plugin.severity(text) {
            severities[level as usize] += 1;
        }
        if let Some(module) = plugin.module(text) {
            *modules.entry(module).or_default() += 1;
        }
    }
    report("stats scan (severity+module)", start.elapsed());
    println!(
        "  rows={rows} modules={} warn+={}",
        modules.len(),
        severities[Severity::Warn as usize]
            + severities[Severity::Error as usize]
            + severities[Severity::Fatal as usize]
    );
}

/// Rough per-frame cost of one visible row: severity, module, decorate, and the
/// column split used by the column view.
fn per_row_cost(session: &Session, view: ViewId) {
    let rows: Vec<usize> = (0..session.visible_len(view).min(50)).collect();
    let rounds = 200;
    let start = Instant::now();
    let mut sink = 0usize;
    for _ in 0..rounds {
        for row in &rows {
            let lr = session.line_ref(view, *row).unwrap();
            let text = session.line_text(lr);
            let plugin = session.plugin_for(lr.doc);
            sink += plugin.severity(text).is_some() as usize;
            sink += plugin.module(text).is_some() as usize;
            let mut spans = Vec::new();
            plugin.decorate(text, &mut spans);
            sink += spans.len();
            sink += plugin.columns(text).is_some() as usize;
        }
    }
    report(&format!("50 rows x {rounds} parse-only"), start.elapsed());
    println!(
        "  ≈ {:.1} µs per 50-row frame (parse only), sink={sink}",
        start.elapsed().as_secs_f64() * 1e6 / rounds as f64
    );
}
