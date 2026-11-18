use std::collections::HashMap;

use eframe::egui::{self, vec2, RichText, Sense, Ui};
use neolog_core::time::{format_time_of_day, NS_PER_DAY};
use neolog_core::{format_ts, Session, Severity, ViewId};

use crate::theme;

pub enum Action {
    Jump { row: usize, note: String },
    Filter(String),
}

pub struct StatsCache {
    key: Option<(ViewId, u64)>,
    data: Option<ViewStats>,
}

impl Default for StatsCache {
    fn default() -> Self {
        Self {
            key: None,
            data: None,
        }
    }
}

struct ViewStats {
    total: usize,
    unknown_levels: usize,
    severities: [usize; 6],
    modules: Vec<(String, usize)>,
    buckets: Vec<(i64, usize)>,
    bucket_ns: i64,
    gaps: Vec<Gap>,
}

struct Gap {
    from: i64,
    to: i64,
    to_row: usize,
}

/// Severity counts for the current view. The full stats scan is cached, so the
/// status-bar pills and the stats window share one pass per view revision.
pub fn cached_levels(cache: &mut StatsCache, session: &Session, view: ViewId) -> [usize; 6] {
    cache.ensure(session, view);
    cache
        .data
        .as_ref()
        .map(|data| data.severities)
        .unwrap_or([0; 6])
}

impl StatsCache {
    fn ensure(&mut self, session: &Session, view: ViewId) {
        let key = (view, session.revision());
        if self.key != Some(key) {
            self.key = Some(key);
            self.data = Some(compute(session, view));
        }
    }
}

pub fn show(
    ui: &mut Ui,
    session: &Session,
    view: ViewId,
    cache: &mut StatsCache,
) -> Option<Action> {
    cache.ensure(session, view);
    let data = cache.data.as_ref()?;
    let mut action = None;

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("统计").strong().size(14.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            theme::chip(
                ui,
                format!("{} 行", data.total),
                theme::p().dim,
                theme::p().surface_high,
            );
        });
    });
    ui.separator();

    ui.label(
        RichText::new("级别（点击筛选）")
            .small()
            .color(theme::p().faint),
    );
    let order = [
        Severity::Trace,
        Severity::Debug,
        Severity::Info,
        Severity::Warn,
        Severity::Error,
        Severity::Fatal,
    ];
    for level in order {
        let count = data.severities[level as usize];
        if count == 0 {
            continue;
        }
        let label = RichText::new(format!("{:<5} {:>6}", level.label(), count))
            .monospace()
            .color(theme::ink(level.color()));
        if ui.selectable_label(false, label).clicked() {
            action = Some(Action::Filter(format!("level={}", level.label())));
        }
    }
    if data.unknown_levels > 0 {
        ui.label(
            RichText::new(format!("{:<5} {:>6}", "未知", data.unknown_levels))
                .monospace()
                .small()
                .color(theme::p().dim),
        );
    }

    if !data.modules.is_empty() {
        ui.separator();
        ui.label(
            RichText::new("模块 Top（点击筛选）")
                .small()
                .color(theme::p().dim),
        );
        for (name, count) in &data.modules {
            let label = RichText::new(format!("{name}  ({count})")).size(12.0);
            if ui.selectable_label(false, label).clicked() {
                action = Some(Action::Filter(format!("module:{}", quote_term(name))));
            }
        }
    }

    ui.separator();
    ui.label(RichText::new("时间分布").small().color(theme::p().dim));
    if data.buckets.is_empty() {
        ui.label(
            RichText::new("没有可用的时间戳")
                .small()
                .color(theme::p().dim),
        );
    } else {
        action = histogram(ui, data).or(action);
        ui.label(
            RichText::new(format!(
                "每 {:.1}s 一格 · 点击柱子按该时段派生子视图",
                data.bucket_ns as f64 / 1e9
            ))
            .small()
            .color(theme::p().dim),
        );
    }

    if !data.gaps.is_empty() {
        ui.separator();
        ui.label(
            RichText::new("时间间隙 Top（≥1s，点击跳转）")
                .small()
                .color(theme::p().dim),
        );
        for gap in &data.gaps {
            let seconds = (gap.to - gap.from) as f64 / 1e9;
            let label = format!(
                "{seconds:.2}s  {} → {}",
                time_only(gap.from),
                time_only(gap.to)
            );
            if ui
                .selectable_label(false, RichText::new(label).size(12.0))
                .on_hover_text(format!(
                    "{} → {}\n跳转到间隙后的第一行",
                    format_ts(gap.from),
                    format_ts(gap.to)
                ))
                .clicked()
            {
                action = Some(Action::Jump {
                    row: gap.to_row,
                    note: format!("已跳到时间间隙之后（中断 {seconds:.2}s）"),
                });
            }
        }
    }

    action
}

fn histogram(ui: &mut Ui, data: &ViewStats) -> Option<Action> {
    let height = 90.0;
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width().max(80.0), height), Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::Rounding::same(7.0), theme::p().surface_high);

    let max = data
        .buckets
        .iter()
        .map(|(_, count)| *count)
        .max()
        .unwrap_or(1)
        .max(1);
    let bar_w = rect.width() / data.buckets.len() as f32;
    for (idx, (_, count)) in data.buckets.iter().enumerate() {
        let ratio = (*count as f32 / max as f32).sqrt();
        let bar_h = ratio * (height - 6.0);
        let bar = egui::Rect::from_min_size(
            egui::pos2(
                rect.left() + idx as f32 * bar_w,
                rect.bottom() - 2.0 - bar_h,
            ),
            vec2((bar_w - 1.0).max(1.0), bar_h),
        );
        let color = if idx % 2 == 0 {
            theme::p().accent.gamma_multiply(0.75)
        } else {
            theme::p().accent.gamma_multiply(0.6)
        };
        painter.rect_filled(bar, egui::Rounding::same(2.0), color);
    }

    let hover_index = resp.hover_pos().and_then(|pos| {
        if !rect.contains(pos) {
            return None;
        }
        Some((((pos.x - rect.left()) / bar_w) as usize).min(data.buckets.len().saturating_sub(1)))
    });
    if let Some(idx) = hover_index {
        let start = data.buckets[idx].0;
        let end = start + data.bucket_ns;
        painter.rect_stroke(
            egui::Rect::from_x_y_ranges(
                rect.left() + idx as f32 * bar_w..=(rect.left() + (idx + 1) as f32 * bar_w),
                rect.top()..=rect.bottom(),
            ),
            3.0,
            egui::Stroke::new(1.0_f32, theme::p().accent_strong),
        );
        let label = format!(
            "{}–{} · {} 行",
            time_only(start),
            time_only(end),
            data.buckets[idx].1
        );
        painter.text(
            rect.left_top() + vec2(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(11.0),
            theme::p().text,
        );
    }

    if resp.clicked() {
        if let Some(idx) = hover_index {
            let start = data.buckets[idx].0;
            let end = start + data.bucket_ns;
            return Some(Action::Filter(format!(
                "time:{}..{}",
                format_time_of_day(start),
                format_time_of_day(end)
            )));
        }
    }
    None
}

fn time_only(nanos: i64) -> String {
    format_time_of_day(nanos.rem_euclid(NS_PER_DAY))
}

fn quote_term(text: &str) -> String {
    if text.chars().any(char::is_whitespace) {
        format!("\"{text}\"")
    } else {
        text.to_string()
    }
}

fn compute(session: &Session, view: ViewId) -> ViewStats {
    let rows = session.visible_len(view);
    let mut severities = [0usize; 6];
    let mut unknown_levels = 0usize;
    let mut module_counts: HashMap<&str, usize> = HashMap::new();
    let mut times: Vec<(i64, usize)> = Vec::new();

    for row in 0..rows {
        let Some(lr) = session.line_ref(view, row) else {
            continue;
        };
        let text = session.line_text(lr);
        let plugin = session.plugin_for(lr.doc);
        match plugin.severity(text) {
            Some(level) => severities[level as usize] += 1,
            None => unknown_levels += 1,
        }
        if let Some(name) = plugin.module(text) {
            *module_counts.entry(name).or_default() += 1;
        }
        if let Some(time) = session.line_time(lr) {
            if time > 0 {
                times.push((time, row));
            }
        }
    }

    let mut modules: Vec<(&str, usize)> = module_counts.into_iter().collect();
    modules.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    modules.truncate(10);
    let modules: Vec<(String, usize)> = modules
        .into_iter()
        .map(|(name, count)| (name.to_string(), count))
        .collect();

    let (buckets, bucket_ns) = histogram_data(&times);
    let gaps = gap_data(&times);

    ViewStats {
        total: rows,
        unknown_levels,
        severities,
        modules,
        buckets,
        bucket_ns,
        gaps,
    }
}

fn histogram_data(times: &[(i64, usize)]) -> (Vec<(i64, usize)>, i64) {
    if times.len() < 2 {
        return (Vec::new(), 1_000_000_000);
    }
    let start = times.iter().map(|(time, _)| *time).min().unwrap_or(0);
    let end = times.iter().map(|(time, _)| *time).max().unwrap_or(0);
    let span = (end - start).max(1);
    let mut bucket_ns = nice_bucket(span / 80);
    while span / bucket_ns + 1 > 240 {
        bucket_ns *= 2;
    }
    let count = ((span / bucket_ns) + 1) as usize;
    let mut buckets = vec![0usize; count];
    for (time, _) in times {
        let idx = (((time - start) / bucket_ns) as usize).min(count - 1);
        buckets[idx] += 1;
    }
    (
        buckets
            .into_iter()
            .enumerate()
            .map(|(idx, hits)| (start + idx as i64 * bucket_ns, hits))
            .collect(),
        bucket_ns,
    )
}

fn nice_bucket(min: i64) -> i64 {
    const CANDIDATES: [i64; 19] = [
        1_000_000,
        2_000_000,
        5_000_000,
        10_000_000,
        25_000_000,
        50_000_000,
        100_000_000,
        250_000_000,
        500_000_000,
        1_000_000_000,
        2_000_000_000,
        5_000_000_000,
        10_000_000_000,
        30_000_000_000,
        60_000_000_000,
        300_000_000_000,
        600_000_000_000,
        1_800_000_000_000,
        3_600_000_000_000,
    ];
    CANDIDATES
        .iter()
        .copied()
        .find(|candidate| *candidate >= min)
        .unwrap_or(3_600_000_000_000)
}

fn gap_data(times: &[(i64, usize)]) -> Vec<Gap> {
    const MIN_GAP: i64 = 1_000_000_000;
    let mut gaps = Vec::new();
    for pair in times.windows(2) {
        let (from, _) = pair[0];
        let (to, to_row) = pair[1];
        if to - from >= MIN_GAP {
            gaps.push(Gap { from, to, to_row });
        }
    }
    gaps.sort_by(|a, b| (b.to - b.from).cmp(&(a.to - a.from)));
    gaps.truncate(8);
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_cover_the_span() {
        let times: Vec<(i64, usize)> = (0..100)
            .map(|idx| {
                (
                    1_700_000_000_000_000_000 + idx * 10_000_000_000,
                    idx as usize,
                )
            })
            .collect();
        let (buckets, bucket) = histogram_data(&times);
        assert!(!buckets.is_empty());
        assert!(buckets.len() <= 240);
        assert_eq!(buckets.iter().map(|(_, hits)| hits).sum::<usize>(), 100);
        assert!(bucket >= 1_000_000_000);
    }

    #[test]
    fn gaps_are_sorted_by_duration() {
        let times = vec![
            (0, 0),
            (2_000_000_000, 1),
            (10_000_000_000, 2),
            (10_500_000_000, 3),
        ];
        let gaps = gap_data(&times);
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0].to_row, 2);
    }
}
