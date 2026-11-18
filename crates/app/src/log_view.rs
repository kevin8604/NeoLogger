use chrono::{DateTime, Utc};
use eframe::egui::text::LayoutJob;
use eframe::egui::{
    self, pos2, vec2, Align2, Color32, FontFamily, FontId, Galley, Rect, Sense, Stroke, TextFormat,
};
use neolog_core::matcher::SearchMatcher;
use neolog_core::plugin::ColumnCell;
use neolog_core::style::{RowStyle, StyledSpan, TextStyle};
use neolog_core::text::word_at_char;
use neolog_core::{LineRef, Session, Severity, ViewId};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;

pub const FONT_SIZE: f32 = 13.5;

#[derive(Clone, Copy, Debug)]
pub enum Scroll {
    Offset(f32),
    Row(usize),
}

pub struct Options<'a> {
    pub selected: Option<LineRef>,
    pub scroll: Option<Scroll>,
    pub selection: Option<Selection>,
    pub wrap: bool,
    pub fold: bool,
    pub columns: bool,
    pub font_size: f32,
    pub find: Option<&'a SearchMatcher>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor_row: usize,
    pub anchor_byte: usize,
    pub focus_row: usize,
    pub focus_byte: usize,
}

impl Selection {
    pub fn new(row: usize, byte: usize) -> Self {
        Self {
            anchor_row: row,
            anchor_byte: byte,
            focus_row: row,
            focus_byte: byte,
        }
    }

    pub fn bounds(&self) -> ((usize, usize), (usize, usize)) {
        let anchor = (self.anchor_row, self.anchor_byte);
        let focus = (self.focus_row, self.focus_byte);
        if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor_row == self.focus_row && self.anchor_byte == self.focus_byte
    }

    /// Selected byte range inside `row`, if the row is part of the selection.
    pub fn row_range(&self, row: usize) -> Option<(usize, usize)> {
        let ((start_row, start_byte), (end_row, end_byte)) = self.bounds();
        if row < start_row || row > end_row {
            return None;
        }
        let from = if row == start_row { start_byte } else { 0 };
        let to = if row == end_row { end_byte } else { usize::MAX };
        (from < to).then_some((from, to))
    }
}

/// Whole-line selection between two rows. `usize::MAX` marks "to end of line",
/// so the anchor can sit below the focus when the drag goes upwards.
fn line_selection(anchor_row: usize, row: usize) -> Selection {
    let (anchor_byte, focus_byte) = if row >= anchor_row {
        (0, usize::MAX)
    } else {
        (usize::MAX, 0)
    };
    Selection {
        anchor_row,
        anchor_byte,
        focus_row: row,
        focus_byte,
    }
}

/// Whether the current drag gesture grabbed text or whole lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragMode {
    Text,
    Lines,
}

fn drag_key(ui: &egui::Ui) -> egui::Id {
    ui.id().with("log-drag-mode")
}

fn begin_drag(ui: &egui::Ui, mode: DragMode) {
    ui.data_mut(|data| data.insert_temp(drag_key(ui), mode));
}

/// Cached drag mode survives across rows and frames; cleared on release.
fn active_drag(ui: &egui::Ui) -> Option<DragMode> {
    if ui.input(|input| input.pointer.any_released()) {
        ui.data_mut(|data| data.remove::<DragMode>(drag_key(ui)));
        return None;
    }
    if !ui.input(|input| input.pointer.primary_down()) {
        return None;
    }
    ui.data(|data| data.get_temp::<DragMode>(drag_key(ui)))
}

/// Drags are owned by the row where they started, so each row checks the global
/// pointer and extends the selection once the pointer enters its rect.
fn extend_drag(
    ui: &egui::Ui,
    rect: Rect,
    idx: usize,
    out: &mut Response,
    byte_at: impl Fn(egui::Pos2) -> usize,
) {
    let Some(mode) = active_drag(ui) else {
        return;
    };
    let Some(selection) = out.selection else {
        return;
    };
    let Some(pointer) = ui.input(|input| input.pointer.interact_pos()) else {
        return;
    };
    if pointer.y < rect.top() || pointer.y >= rect.bottom() {
        return;
    }
    out.selection = Some(match mode {
        DragMode::Lines => line_selection(selection.anchor_row, idx),
        DragMode::Text => Selection {
            focus_row: idx,
            focus_byte: byte_at(pointer),
            ..selection
        },
    });
}

/// Scrolls while a selection drag hovers the viewport edges, so ranges longer
/// than the screen can be selected without releasing the button.
fn auto_scroll_drag(ui: &egui::Ui, out: &Response) {
    if out.selection.is_none() || !ui.input(|input| input.pointer.primary_down()) {
        return;
    }
    if ui
        .data(|data| data.get_temp::<DragMode>(drag_key(ui)))
        .is_none()
    {
        return;
    }
    let Some(pointer) = ui.input(|input| input.pointer.interact_pos()) else {
        return;
    };
    const EDGE: f32 = 24.0;
    const SPEED: f32 = 14.0;
    let viewport = ui.clip_rect();
    let dy = if pointer.y < viewport.top() + EDGE {
        ((viewport.top() + EDGE - pointer.y) / EDGE).min(1.0) * SPEED
    } else if pointer.y > viewport.bottom() - EDGE {
        -((pointer.y - (viewport.bottom() - EDGE)) / EDGE).min(1.0) * SPEED
    } else {
        0.0
    };
    if dy != 0.0 {
        ui.scroll_with_delta(vec2(0.0, dy));
    }
}

#[derive(Default)]
pub struct Response {
    pub selected: Option<LineRef>,
    pub toggle_bookmark: Option<LineRef>,
    pub highlight: Option<(LineRef, String)>,
    pub selection: Option<Selection>,
    /// Start row of a folded group that was clicked.
    pub toggle_fold: Option<usize>,
    /// Action picked from a row's context menu.
    pub action: Option<RowAction>,
    pub scroll_offset: f32,
    pub viewport_height: f32,
}

/// Row commands offered by the log context menu. The view only describes the
/// intent; the app owns clipboard, filters and highlight rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowAction {
    CopyLine(LineRef),
    CopyText(String),
    ToggleBookmark(LineRef),
    Highlight {
        line: LineRef,
        word: String,
        row_bg: bool,
    },
    FilterWord(String),
    FilterLevel(Severity),
    FilterModule(String),
    Correlate {
        key: String,
        value: String,
    },
    GlobalSearch(String),
    ToggleFold(usize),
}

/// Prefix layout for soft wrap: visual line counts per logical row. Rebuilt only
/// when the view, session revision or wrap width changes.
#[derive(Default)]
pub struct WrapCache {
    key: Option<(ViewId, u64, usize)>,
    counts: Vec<u32>,
    prefix: Vec<u64>,
}

impl WrapCache {
    fn ensure(&mut self, session: &Session, view: ViewId, cols: usize) {
        let key = (view, session.revision(), cols);
        let rows = session.visible_len(view);
        if self.key == Some(key) && self.prefix.len() == rows + 1 {
            return;
        }
        self.counts.clear();
        self.counts.reserve(rows);
        self.prefix.clear();
        self.prefix.reserve(rows + 1);
        self.prefix.push(0);
        for idx in 0..rows {
            let count = session
                .line_ref(view, idx)
                .map(|lr| wrap_count(session.line_text(lr), cols))
                .unwrap_or(1);
            self.counts.push(count as u32);
            let total = self.prefix.last().copied().unwrap_or(0) + count as u64;
            self.prefix.push(total);
        }
        self.key = Some(key);
    }

    fn visual_offset(&self, row: usize, row_h: f32) -> f32 {
        self.prefix.get(row).copied().unwrap_or(0) as f32 * row_h
    }

    fn total_visual(&self) -> u64 {
        self.prefix.last().copied().unwrap_or(0)
    }

    fn row_at_visual(&self, visual: u64) -> usize {
        self.prefix
            .partition_point(|&start| start <= visual)
            .saturating_sub(1)
    }

    fn count(&self, row: usize) -> usize {
        self.counts.get(row).copied().unwrap_or(1) as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FoldBadge {
    pub group: usize,
    pub len: usize,
    pub expanded: bool,
}

#[derive(Clone, Copy)]
struct FoldGroup {
    start: usize,
    len: usize,
}

#[derive(Clone, Copy)]
struct FoldEntry {
    row: usize,
    group: usize,
    len: usize,
    expanded: bool,
}

/// Consecutive rows whose text normalizes to the same template are folded into
/// one row with a `×N` badge. Only rebuilt when the view or its rows change.
#[derive(Default)]
pub struct FoldCache {
    key: Option<(ViewId, u64)>,
    groups: Vec<FoldGroup>,
    entries: Vec<FoldEntry>,
    expanded: BTreeSet<usize>,
}

impl FoldCache {
    fn ensure(&mut self, session: &Session, view: ViewId) {
        let key = (view, session.revision());
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        self.expanded.clear();
        self.groups.clear();
        self.entries.clear();
        let rows = session.visible_len(view);
        if rows == 0 {
            return;
        }
        let mut start = 0usize;
        let mut prev = String::new();
        let mut cur = String::new();
        for row in 0..rows {
            let text = session
                .line_ref(view, row)
                .map(|lr| session.line_text(lr))
                .unwrap_or("");
            if row == 0 {
                template_into(text, &mut prev);
                continue;
            }
            cur.clear();
            template_into(text, &mut cur);
            if cur != prev {
                self.groups.push(FoldGroup {
                    start,
                    len: row - start,
                });
                start = row;
                std::mem::swap(&mut prev, &mut cur);
            }
        }
        self.groups.push(FoldGroup {
            start,
            len: rows - start,
        });
        self.rebuild_entries();
    }

    fn rebuild_entries(&mut self) {
        self.entries.clear();
        for group in &self.groups {
            if group.len <= 1 || !self.expanded.contains(&group.start) {
                self.entries.push(FoldEntry {
                    row: group.start,
                    group: group.start,
                    len: group.len,
                    expanded: false,
                });
            } else {
                for row in group.start..group.start + group.len {
                    self.entries.push(FoldEntry {
                        row,
                        group: group.start,
                        len: group.len,
                        expanded: true,
                    });
                }
            }
        }
    }

    pub fn toggle(&mut self, group: usize) {
        if !self.expanded.remove(&group) {
            self.expanded.insert(group);
        }
        self.rebuild_entries();
    }

    fn entry_of_row(&self, row: usize) -> usize {
        self.entries
            .partition_point(|entry| entry.row <= row)
            .saturating_sub(1)
    }
}

const COLUMN_HEADER_H: f32 = 22.0;
const COLUMN_GAP: f32 = 12.0;
const COLUMN_LEVEL_CHARS: usize = 5;
const COLUMN_MODULE_MIN: usize = 6;
const COLUMN_MODULE_MAX: usize = 18;

/// Width of the module column and the date format, derived from the view once
/// per revision. Plugins split rows; the cache only measures their modules.
#[derive(Default)]
pub struct ColumnsCache {
    key: Option<(ViewId, u64)>,
    level_chars: usize,
    module_chars: usize,
    span_days: bool,
}

impl ColumnsCache {
    fn ensure(&mut self, session: &Session, view: ViewId) {
        let key = (view, session.revision());
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        let rows = session.visible_len(view);
        let mut level_chars = COLUMN_LEVEL_CHARS;
        let mut module_chars = COLUMN_MODULE_MIN;
        let step = (rows / 1500).max(1);
        let mut row = 0usize;
        while row < rows {
            if let Some(lr) = session.line_ref(view, row) {
                let text = session.line_text(lr);
                if let Some(columns) = session.plugin_for(lr.doc).columns(text) {
                    if let Some(level) = columns.level {
                        level_chars = level_chars.max(level.text.width());
                    }
                    if let Some(module) = columns.module {
                        module_chars = module_chars.max(module.text.width());
                    }
                }
            }
            row += step;
        }
        self.level_chars = level_chars.clamp(COLUMN_LEVEL_CHARS, 9);
        self.module_chars = module_chars.clamp(COLUMN_MODULE_MIN, COLUMN_MODULE_MAX);
        self.span_days = session.time_range(view).is_some_and(|(start, end)| {
            start.div_euclid(neolog_core::time::NS_PER_DAY)
                != end.div_euclid(neolog_core::time::NS_PER_DAY)
        });
    }
}

/// Style, highlight spans and the finished galley for rows that were on screen
/// recently. Keyed by physical line, so tab switches reuse entries; the whole
/// cache is dropped when the session, font size or find query changes.
#[derive(Default)]
pub struct RowCache {
    key: Option<(u64, u32, Option<u64>)>,
    frame: u64,
    rows: HashMap<LineRef, RowPaint>,
    galleys: HashMap<LineRef, CachedGalley>,
}

/// Frames an unused entry survives; a fast scroll touches a sliding window of
/// rows, so a few frames of slack keep it from re-decorating the same lines.
const ROW_CACHE_AGE: u64 = 8;
const ROW_CACHE_MAX_ROWS: usize = 2048;
const ROW_CACHE_MAX_GALLEYS: usize = 1024;

struct RowPaint {
    style: RowStyle,
    base_color: Color32,
    spans: Vec<StyledSpan>,
    frame: u64,
}

struct CachedGalley {
    galley: Arc<Galley>,
    frame: u64,
}

impl RowCache {
    fn begin(&mut self, session: &Session, font_size: f32, find: Option<&SearchMatcher>) {
        self.frame = self.frame.wrapping_add(1);
        let key = (
            session.revision(),
            font_size.to_bits(),
            find.map(SearchMatcher::key),
        );
        if self.key != Some(key) {
            self.key = Some(key);
            self.rows.clear();
            self.galleys.clear();
        }
        let frame = self.frame;
        if self.rows.len() > ROW_CACHE_MAX_ROWS {
            self.rows
                .retain(|_, paint| paint.frame + ROW_CACHE_AGE >= frame);
        }
        if self.galleys.len() > ROW_CACHE_MAX_GALLEYS {
            self.galleys
                .retain(|_, cached| cached.frame + ROW_CACHE_AGE >= frame);
        }
    }

    fn paint(&mut self, session: &Session, lr: LineRef, find: Option<&SearchMatcher>) -> &RowPaint {
        let frame = self.frame;
        let paint = self
            .rows
            .entry(lr)
            .or_insert_with(|| build_row_paint(session, lr, find));
        paint.frame = frame;
        paint
    }

    /// Style, base color and a cached full-line galley for scrolling modes.
    fn row(
        &mut self,
        ui: &egui::Ui,
        session: &Session,
        lr: LineRef,
        find: Option<&SearchMatcher>,
        font_size: f32,
    ) -> (RowStyle, Color32, Arc<Galley>) {
        let frame = self.frame;
        let paint = self
            .rows
            .entry(lr)
            .or_insert_with(|| build_row_paint(session, lr, find));
        paint.frame = frame;
        if let Some(cached) = self.galleys.get_mut(&lr) {
            cached.frame = frame;
            return (paint.style, paint.base_color, cached.galley.clone());
        }
        let text = session.line_text(lr);
        let job = styled_job(
            text,
            paint.base_color,
            paint.style.bold,
            &paint.spans,
            font_size,
        );
        let galley = ui.fonts(|fonts| fonts.layout_job(job));
        self.galleys.insert(
            lr,
            CachedGalley {
                galley: galley.clone(),
                frame,
            },
        );
        (paint.style, paint.base_color, galley)
    }
}

fn build_row_paint(session: &Session, lr: LineRef, find: Option<&SearchMatcher>) -> RowPaint {
    let text = session.line_text(lr);
    let plugin = session.plugin_for(lr.doc);
    let mut style = plugin.row_style(text);
    let mut spans = Vec::new();
    plugin.decorate(text, &mut spans);
    for rule in session.rules_for(lr) {
        if rule.is_match(text) {
            rule.collect_spans(text, &mut spans);
            if let Some(bg) = rule.row_bg {
                style.bg = Some(bg);
            }
        }
    }
    if let Some(needle) = find {
        append_find_spans(text, needle, &mut spans);
    }
    for span in &mut spans {
        span.start = floor_boundary(text, span.start);
        span.end = floor_boundary(text, span.end);
    }
    let base_color = style.fg.map(theme::ink).unwrap_or(theme::p().text);
    RowPaint {
        style,
        base_color,
        spans,
        frame: 0,
    }
}

struct ColumnLayout {
    time_x: f32,
    time_right: f32,
    level_x: f32,
    level_right: f32,
    module_x: f32,
    module_right: f32,
    message_x: f32,
    message_right: f32,
}

impl ColumnLayout {
    fn new(
        rect: Rect,
        gutter_w: f32,
        char_w: f32,
        level_chars: usize,
        module_chars: usize,
        span_days: bool,
    ) -> Self {
        let time_chars = if span_days { 18 } else { 12 };
        let time_x = rect.left() + gutter_w + 6.0;
        let time_right = time_x + time_chars as f32 * char_w;
        let level_x = time_right + COLUMN_GAP;
        let level_right = level_x + level_chars as f32 * char_w;
        let module_x = level_right + COLUMN_GAP;
        let module_right = module_x + module_chars as f32 * char_w;
        let message_x = module_right + COLUMN_GAP;
        Self {
            time_x,
            time_right,
            level_x,
            level_right,
            module_x,
            module_right,
            message_x,
            message_right: rect.right() - 6.0,
        }
    }
}

struct CellHit<'a> {
    rect: Rect,
    text_pos: egui::Pos2,
    text: &'a str,
    range: (usize, usize),
    galley: Arc<Galley>,
}

fn template_into(text: &str, out: &mut String) {
    out.clear();
    let mut digit = false;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            if !digit {
                out.push('#');
            }
            digit = true;
        } else {
            digit = false;
            out.push(ch.to_ascii_lowercase());
        }
    }
}

pub fn row_height(ctx: &egui::Context, font_size: f32) -> f32 {
    ctx.fonts(|fonts| fonts.row_height(&FontId::monospace(font_size))) + 2.0
}

pub fn show(
    ui: &mut egui::Ui,
    session: &Session,
    view: ViewId,
    opts: Options<'_>,
    wrap_cache: &mut WrapCache,
    fold_cache: &mut FoldCache,
    columns_cache: &mut ColumnsCache,
    row_cache: &mut RowCache,
) -> Response {
    let total = session.visible_len(view);
    let font_size = opts.font_size.clamp(8.0, 32.0);
    let font = FontId::monospace(font_size);
    let char_w = ui.fonts(|fonts| fonts.glyph_width(&font, 'M'));
    let row_h = row_height(ui.ctx(), font_size);
    let digits = total.max(1).to_string().len().max(3);
    let gutter_w = 6.0 + 16.0 + char_w * digits as f32 + 10.0;
    let find = opts.find;
    row_cache.begin(session, font_size, find);

    let mut out = Response {
        selected: opts.selected,
        selection: opts.selection,
        ..Default::default()
    };

    ui.spacing_mut().item_spacing.y = 0.0;
    if opts.fold {
        fold_cache.ensure(session, view);
        let offset = match opts.scroll {
            Some(Scroll::Offset(offset)) => Some(offset),
            Some(Scroll::Row(row)) => Some(fold_cache.entry_of_row(row) as f32 * row_h),
            None => None,
        };
        let mut scroll = egui::ScrollArea::both().auto_shrink([false, false]);
        if let Some(offset) = offset {
            scroll = scroll.vertical_scroll_offset(offset);
        }
        let entries = &fold_cache.entries;
        let output = scroll.show_rows(ui, row_h, entries.len(), |ui, range| {
            for idx in range {
                let entry = entries[idx];
                let badge = (entry.len > 1 && entry.row == entry.group).then_some(FoldBadge {
                    group: entry.group,
                    len: entry.len,
                    expanded: entry.expanded,
                });
                draw_row(
                    ui, session, view, entry.row, idx, row_h, gutter_w, font_size, find, badge,
                    row_cache, &mut out,
                );
            }
            auto_scroll_drag(ui, &out);
        });
        out.scroll_offset = output.state.offset.y;
        out.viewport_height = output.inner_rect.height();
    } else if opts.wrap {
        let wrap_width = (ui.available_width() - gutter_w - 24.0).max(char_w * 8.0);
        let cols = ((wrap_width / char_w).floor() as usize).max(8);
        wrap_cache.ensure(session, view, cols);
        let offset = match opts.scroll {
            Some(Scroll::Offset(offset)) => Some(offset),
            Some(Scroll::Row(row)) => Some(wrap_cache.visual_offset(row, row_h)),
            None => None,
        };
        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(offset) = offset {
            scroll = scroll.vertical_scroll_offset(offset);
        }
        let output = scroll.show_viewport(ui, |ui, viewport| {
            draw_wrapped(
                ui, session, view, row_h, gutter_w, cols, viewport, wrap_cache, row_cache,
                font_size, find, &mut out,
            );
            auto_scroll_drag(ui, &out);
        });
        out.scroll_offset = output.state.offset.y;
        out.viewport_height = output.inner_rect.height();
    } else if opts.columns {
        columns_cache.ensure(session, view);
        draw_columns_header(ui, gutter_w, char_w, columns_cache);
        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(offset) = opts.scroll {
            let offset = match offset {
                Scroll::Offset(offset) => offset,
                Scroll::Row(row) => row as f32 * row_h,
            };
            scroll = scroll.vertical_scroll_offset(offset);
        }
        let output = scroll.show_rows(ui, row_h, total, |ui, range| {
            for idx in range {
                draw_columns_row(
                    ui,
                    session,
                    view,
                    idx,
                    row_h,
                    gutter_w,
                    char_w,
                    columns_cache.level_chars,
                    columns_cache.module_chars,
                    columns_cache.span_days,
                    font_size,
                    find,
                    row_cache,
                    &mut out,
                );
            }
            auto_scroll_drag(ui, &out);
        });
        out.scroll_offset = output.state.offset.y;
        out.viewport_height = output.inner_rect.height();
    } else {
        let mut scroll = egui::ScrollArea::both().auto_shrink([false, false]);
        if let Some(offset) = opts.scroll {
            let offset = match offset {
                Scroll::Offset(offset) => offset,
                Scroll::Row(row) => row as f32 * row_h,
            };
            scroll = scroll.vertical_scroll_offset(offset);
        }
        let output = scroll.show_rows(ui, row_h, total, |ui, range| {
            for idx in range {
                draw_row(
                    ui, session, view, idx, idx, row_h, gutter_w, font_size, find, None, row_cache,
                    &mut out,
                );
            }
            auto_scroll_drag(ui, &out);
        });
        out.scroll_offset = output.state.offset.y;
        out.viewport_height = output.inner_rect.height();
    }
    out
}

fn menu_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> bool {
    ui.add_enabled(enabled, egui::Button::new(label)).clicked()
}

fn short_label(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[allow(clippy::too_many_arguments)]
fn row_context_menu(
    ui: &mut egui::Ui,
    resp: &egui::Response,
    session: &Session,
    lr: LineRef,
    idx: usize,
    text: &str,
    pointer_word: Option<String>,
    fold: Option<FoldBadge>,
    selection: Option<Selection>,
    out: &mut Response,
) {
    let seed = resp.id.with("row-menu-word");
    if resp.secondary_clicked() {
        out.selected = Some(lr);
        ui.data_mut(|data| data.insert_temp(seed, pointer_word));
    }
    let word: Option<String> = ui
        .data(|data| data.get_temp::<Option<String>>(seed))
        .flatten();
    resp.context_menu(|ui| {
        ui.set_min_width(190.0);
        let selected_text = selection.and_then(|sel| {
            if sel.is_empty() {
                return None;
            }
            let (from, to) = sel.row_range(idx)?;
            let from = floor_boundary(text, from.min(text.len()));
            let to = floor_boundary(text, to.min(text.len()));
            (from < to).then(|| text[from..to].to_string())
        });
        let plugin = session.plugin_for(lr.doc);
        let severity = plugin.severity(text);
        let module = plugin
            .module(text)
            .map(str::trim)
            .filter(|module| !module.is_empty())
            .map(str::to_string);
        let marked = session.is_bookmarked(lr);

        if menu_button(ui, "复制整行", true) {
            out.action = Some(RowAction::CopyLine(lr));
            ui.close_menu();
        }
        if let Some(selected) = selected_text {
            if menu_button(ui, "复制选中内容", true) {
                out.action = Some(RowAction::CopyText(selected));
                ui.close_menu();
            }
        }
        if let Some(word) = &word {
            if menu_button(ui, &format!("复制 \"{}\"", short_label(word, 20)), true) {
                out.action = Some(RowAction::CopyText(word.clone()));
                ui.close_menu();
            }
        }

        ui.separator();
        let bookmark = if marked {
            "移除书签"
        } else {
            "添加书签"
        };
        if menu_button(ui, bookmark, true) {
            out.action = Some(RowAction::ToggleBookmark(lr));
            ui.close_menu();
        }
        if let Some(word) = &word {
            if menu_button(ui, "高亮这个词…", true) {
                out.action = Some(RowAction::Highlight {
                    line: lr,
                    word: word.clone(),
                    row_bg: false,
                });
                ui.close_menu();
            }
        }

        ui.separator();
        if let Some(word) = &word {
            if menu_button(ui, &format!("筛选包含 \"{}\"", short_label(word, 18)), true) {
                out.action = Some(RowAction::FilterWord(word.clone()));
                ui.close_menu();
            }
        }
        if let Some(severity) = severity {
            if menu_button(ui, &format!("只显示 {} 级", severity.label()), true) {
                out.action = Some(RowAction::FilterLevel(severity));
                ui.close_menu();
            }
        }
        if let Some(module) = &module {
            if menu_button(ui, &format!("只显示模块 {}", short_label(module, 16)), true) {
                out.action = Some(RowAction::FilterModule(module.clone()));
                ui.close_menu();
            }
        }
        let ids = neolog_core::correlation_ids(plugin, text);
        if !ids.is_empty() {
            ui.separator();
            for id in ids {
                let label = format!("串起 {}={}", id.key, short_label(&id.value, 18));
                if menu_button(ui, &label, true) {
                    out.action = Some(RowAction::Correlate {
                        key: id.key,
                        value: id.value,
                    });
                    ui.close_menu();
                }
            }
        }
        if let Some(word) = &word {
            if menu_button(ui, "在全部文件中搜索该词", true) {
                out.action = Some(RowAction::GlobalSearch(word.clone()));
                ui.close_menu();
            }
        }

        if let Some(fold) = fold.filter(|fold| fold.len > 1) {
            ui.separator();
            let label = if fold.expanded {
                format!("收起这 {} 行", fold.len)
            } else {
                format!("展开这 {} 行", fold.len)
            };
            if menu_button(ui, &label, true) {
                out.action = Some(RowAction::ToggleFold(fold.group));
                ui.close_menu();
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    ui: &mut egui::Ui,
    session: &Session,
    view: ViewId,
    idx: usize,
    visible_idx: usize,
    row_h: f32,
    gutter_w: f32,
    font_size: f32,
    find: Option<&SearchMatcher>,
    fold: Option<FoldBadge>,
    row_cache: &mut RowCache,
    out: &mut Response,
) {
    let Some(lr) = session.line_ref(view, idx) else {
        return;
    };
    let text = session.line_text(lr);
    let (row_style, base_color, galley) = row_cache.row(ui, session, lr, find, font_size);

    let width = (gutter_w + galley.size().x + 24.0).max(ui.available_width());
    let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click_and_drag());

    let stripe = if visible_idx % 2 == 0 {
        theme::p().row_even
    } else {
        theme::p().row_odd
    };
    let bg = row_style.bg.map(theme::color).unwrap_or(stripe);
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, bg);

    if out.selected == Some(lr) {
        painter.rect_filled(rect, 0.0, theme::p().selected_row);
    } else if resp.hovered() {
        painter.rect_filled(rect, 0.0, theme::p().hover);
    }

    painter.rect_filled(
        egui::Rect::from_min_size(rect.left_top(), vec2(3.0, row_h)),
        egui::Rounding {
            nw: 0.0,
            sw: 0.0,
            ne: 1.5,
            se: 1.5,
        },
        theme::doc_color(lr.doc),
    );

    let bookmark_center = pos2(rect.left() + 15.0, rect.center().y);
    if session.is_bookmarked(lr) {
        painter.circle_stroke(
            bookmark_center,
            6.5,
            Stroke::new(1.0_f32, theme::p().bookmark.gamma_multiply(0.35)),
        );
        painter.circle_filled(bookmark_center, 3.5, theme::p().bookmark);
    } else if resp.hovered() {
        painter.circle_stroke(
            bookmark_center,
            4.0,
            Stroke::new(1.0_f32, theme::p().border),
        );
    }

    painter.line_segment(
        [
            pos2(rect.left() + gutter_w - 4.0, rect.top()),
            pos2(rect.left() + gutter_w - 4.0, rect.bottom()),
        ],
        Stroke::new(1.0_f32, theme::p().border_soft),
    );

    painter.text(
        pos2(rect.left() + gutter_w - 8.0, rect.center().y),
        Align2::RIGHT_CENTER,
        (lr.line as usize + 1).to_string(),
        FontId::monospace(font_size - 1.0),
        if out.selected == Some(lr) {
            theme::p().accent_strong
        } else {
            theme::p().faint
        },
    );

    let text_pos = pos2(rect.left() + gutter_w, rect.top() + 1.0);

    let font = FontId::monospace(font_size);
    if let Some(sel) = out.selection {
        if let Some((from, to)) = sel.row_range(idx) {
            let from = floor_boundary(text, from.min(text.len()));
            let to = floor_boundary(text, to.min(text.len()));
            let whole_line = from == 0 && to >= text.len();
            if whole_line || from < to {
                let x0 = if whole_line {
                    0.0
                } else {
                    text_width(ui, &text[..from], &font, base_color)
                };
                let x1 = if whole_line {
                    (rect.right() - text_pos.x - 8.0).max(x0 + 6.0)
                } else {
                    text_width(ui, &text[..to], &font, base_color)
                };
                painter.rect_filled(
                    Rect::from_min_max(
                        pos2(text_pos.x + x0, rect.top() + 1.0),
                        pos2(text_pos.x + x1, rect.bottom() - 1.0),
                    ),
                    0.0,
                    theme::p().selection,
                );
            }
        }
    }
    painter.galley(text_pos, galley.clone(), base_color);

    let mut badge_clicked = false;
    if let Some(fold) = fold {
        let label = if fold.expanded {
            format!("- {}", fold.len)
        } else {
            format!("×{}", fold.len)
        };
        let badge_font = FontId::monospace(font_size - 1.0);
        let badge_galley = ui.fonts(|fonts| {
            fonts.layout_no_wrap(label, badge_font, Color32::from_rgb(244, 222, 150))
        });
        let badge_pos = pos2(
            text_pos.x + galley.size().x + 12.0,
            rect.top() + (row_h - badge_galley.size().y) * 0.5,
        );
        let badge_rect = Rect::from_min_size(
            badge_pos - vec2(5.0, 2.0),
            badge_galley.size() + vec2(10.0, 4.0),
        );
        painter.rect_filled(
            badge_rect,
            3.0,
            Color32::from_rgba_unmultiplied(96, 78, 24, 220),
        );
        painter.galley(badge_pos, badge_galley, Color32::from_rgb(244, 222, 150));
        let hint = if fold.expanded {
            format!("收起这 {} 行", fold.len)
        } else {
            format!("展开这 {} 行（只折叠相邻且仅数字不同的行）", fold.len)
        };
        let badge_resp = ui
            .interact(
                badge_rect,
                ui.id().with(("fold-badge", fold.group)),
                Sense::click(),
            )
            .on_hover_text(hint);
        if badge_resp.clicked() {
            out.toggle_fold = Some(fold.group);
            badge_clicked = true;
        }
    }
    if badge_clicked {
        return;
    }

    if resp.clicked() || resp.double_clicked() {
        out.selected = Some(lr);
    }
    let gutter_cut = rect.left() + gutter_w;
    let byte_at = |pointer: egui::Pos2| -> usize {
        if pointer.x <= text_pos.x {
            return 0;
        }
        let local = vec2(pointer.x - text_pos.x, pointer.y - text_pos.y);
        let char_idx = galley.cursor_from_pos(local).ccursor.index;
        byte_of_char(text, char_idx)
    };
    if let Some(pointer) = resp.interact_pointer_pos() {
        let in_text = pointer.x > gutter_cut;
        if resp.clicked() && pointer.x < rect.left() + 6.0 + 16.0 {
            out.toggle_bookmark = Some(lr);
        }
        if resp.double_clicked() && in_text {
            let local = vec2(pointer.x - text_pos.x, pointer.y - text_pos.y);
            let char_idx = galley.cursor_from_pos(local).ccursor.index;
            if let Some(word) = word_at_char(text, char_idx) {
                out.highlight = Some((lr, word.to_string()));
            }
        }
        if resp.drag_started() {
            let origin = ui
                .input(|input| input.pointer.press_origin())
                .unwrap_or(pointer);
            if origin.x <= gutter_cut {
                begin_drag(ui, DragMode::Lines);
                out.selection = Some(line_selection(idx, idx));
            } else if in_text {
                begin_drag(ui, DragMode::Text);
                out.selection = Some(Selection::new(idx, byte_at(origin)));
            }
        }
        if resp.triple_clicked() && in_text {
            out.selection = Some(line_selection(idx, idx));
        } else if resp.clicked() && !resp.double_clicked() && !resp.triple_clicked() {
            let shift = ui.input(|input| input.modifiers.shift);
            if shift {
                let anchor = out
                    .selection
                    .map(|sel| (sel.anchor_row, sel.anchor_byte))
                    .unwrap_or((idx, byte_at(pointer)));
                out.selection = Some(Selection {
                    anchor_row: anchor.0,
                    anchor_byte: anchor.1,
                    focus_row: idx,
                    focus_byte: byte_at(pointer),
                });
            } else if pointer.x > rect.left() + 6.0 + 16.0 && pointer.x <= gutter_cut {
                out.selection = Some(line_selection(idx, idx));
            } else if in_text {
                out.selection = None;
            }
        }
    }
    extend_drag(ui, rect, idx, out, byte_at);
    let menu_word = resp
        .interact_pointer_pos()
        .and_then(|pointer| {
            (pointer.x > rect.left() + gutter_w).then(|| {
                let local = vec2(pointer.x - text_pos.x, pointer.y - text_pos.y);
                let char_idx = galley.cursor_from_pos(local).ccursor.index;
                word_at_char(text, char_idx).map(str::to_string)
            })
        })
        .flatten();
    row_context_menu(
        ui,
        &resp,
        session,
        lr,
        idx,
        text,
        menu_word,
        fold,
        out.selection,
        out,
    );
}

fn draw_columns_header(ui: &mut egui::Ui, gutter_w: f32, char_w: f32, cache: &ColumnsCache) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, COLUMN_HEADER_H), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, theme::p().surface);
    painter.line_segment(
        [
            pos2(rect.left(), rect.bottom() - 0.5),
            pos2(rect.right(), rect.bottom() - 0.5),
        ],
        Stroke::new(1.0_f32, theme::p().border_soft),
    );
    let layout = ColumnLayout::new(
        rect,
        gutter_w,
        char_w,
        cache.level_chars,
        cache.module_chars,
        cache.span_days,
    );
    let font = FontId::proportional(10.5);
    for (x, label) in [
        (layout.time_x, "时间"),
        (layout.level_x, "级别"),
        (layout.module_x, "模块"),
        (layout.message_x, "消息"),
    ] {
        painter.text(
            pos2(x, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font.clone(),
            theme::p().faint,
        );
    }
}

fn time_text(session: &Session, lr: LineRef, span_days: bool) -> Option<String> {
    let nanos = session.line_time(lr)?;
    if nanos == 0 {
        return None;
    }
    let pattern = if span_days {
        "%m-%d %H:%M:%S%.3f"
    } else {
        "%H:%M:%S%.3f"
    };
    Some(
        DateTime::<Utc>::from_timestamp_nanos(nanos)
            .format(pattern)
            .to_string(),
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_columns_row(
    ui: &mut egui::Ui,
    session: &Session,
    view: ViewId,
    idx: usize,
    row_h: f32,
    gutter_w: f32,
    char_w: f32,
    level_chars: usize,
    module_chars: usize,
    span_days: bool,
    font_size: f32,
    find: Option<&SearchMatcher>,
    row_cache: &mut RowCache,
    out: &mut Response,
) {
    let Some(lr) = session.line_ref(view, idx) else {
        return;
    };
    let text = session.line_text(lr);
    let paint = row_cache.paint(session, lr, find);
    let row_style = paint.style;
    let base_color = paint.base_color;
    let spans = paint.spans.as_slice();

    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click_and_drag());

    let stripe = if idx % 2 == 0 {
        theme::p().row_even
    } else {
        theme::p().row_odd
    };
    let bg = row_style.bg.map(theme::color).unwrap_or(stripe);
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, bg);

    if out.selected == Some(lr) {
        painter.rect_filled(rect, 0.0, theme::p().selected_row);
    } else if resp.hovered() {
        painter.rect_filled(rect, 0.0, theme::p().hover);
    }

    painter.rect_filled(
        egui::Rect::from_min_size(rect.left_top(), vec2(3.0, row_h)),
        egui::Rounding {
            nw: 0.0,
            sw: 0.0,
            ne: 1.5,
            se: 1.5,
        },
        theme::doc_color(lr.doc),
    );

    let bookmark_center = pos2(rect.left() + 15.0, rect.center().y);
    if session.is_bookmarked(lr) {
        painter.circle_stroke(
            bookmark_center,
            6.5,
            Stroke::new(1.0_f32, theme::p().bookmark.gamma_multiply(0.35)),
        );
        painter.circle_filled(bookmark_center, 3.5, theme::p().bookmark);
    } else if resp.hovered() {
        painter.circle_stroke(
            bookmark_center,
            4.0,
            Stroke::new(1.0_f32, theme::p().border),
        );
    }

    painter.line_segment(
        [
            pos2(rect.left() + gutter_w - 4.0, rect.top()),
            pos2(rect.left() + gutter_w - 4.0, rect.bottom()),
        ],
        Stroke::new(1.0_f32, theme::p().border_soft),
    );

    painter.text(
        pos2(rect.left() + gutter_w - 8.0, rect.center().y),
        Align2::RIGHT_CENTER,
        (lr.line as usize + 1).to_string(),
        FontId::monospace(font_size - 1.0),
        if out.selected == Some(lr) {
            theme::p().accent_strong
        } else {
            theme::p().faint
        },
    );

    let layout = ColumnLayout::new(rect, gutter_w, char_w, level_chars, module_chars, span_days);

    if let Some(clock) = time_text(session, lr, span_days) {
        let clip = Rect::from_min_max(
            pos2(layout.time_x, rect.top()),
            pos2(layout.time_right, rect.bottom()),
        );
        ui.painter().with_clip_rect(clip).text(
            pos2(layout.time_x, rect.center().y),
            Align2::LEFT_CENTER,
            clock,
            FontId::monospace(font_size - 1.0),
            theme::p().dim,
        );
    }

    let mut hits: Vec<CellHit<'_>> = Vec::new();
    let columns = session.plugin_for(lr.doc).columns(text);
    let fallback;
    if let Some(columns) = columns.as_ref() {
        if let Some(level) = &columns.level {
            let color = Severity::parse(level.text.trim())
                .map(|severity| theme::ink(severity.color()))
                .unwrap_or(theme::p().dim);
            draw_cell(
                ui,
                level,
                layout.level_x,
                layout.level_right,
                rect,
                font_size,
                color,
                true,
                spans,
                out.selection,
                idx,
                &mut hits,
            );
        }
        if let Some(module) = &columns.module {
            draw_cell(
                ui,
                module,
                layout.module_x,
                layout.module_right,
                rect,
                font_size,
                theme::p().dim,
                false,
                spans,
                out.selection,
                idx,
                &mut hits,
            );
        }
        draw_cell(
            ui,
            &columns.message,
            layout.message_x,
            layout.message_right,
            rect,
            font_size,
            base_color,
            row_style.bold,
            spans,
            out.selection,
            idx,
            &mut hits,
        );
    } else {
        fallback = ColumnCell {
            text: text.into(),
            range: Some((0, text.len())),
        };
        draw_cell(
            ui,
            &fallback,
            layout.message_x,
            layout.message_right,
            rect,
            font_size,
            base_color,
            row_style.bold,
            spans,
            out.selection,
            idx,
            &mut hits,
        );
    }

    if resp.clicked() || resp.double_clicked() {
        out.selected = Some(lr);
    }
    let gutter_cut = rect.left() + gutter_w;
    let byte_at = |pointer: egui::Pos2| -> usize {
        hits.iter()
            .find(|hit| hit.rect.contains(pointer))
            .map(|hit| {
                let local = vec2(pointer.x - hit.text_pos.x, pointer.y - hit.text_pos.y);
                let char_idx = hit.galley.cursor_from_pos(local).ccursor.index;
                let (start, end) = hit.range;
                start + byte_of_char(hit.text, char_idx).min(end - start)
            })
            .unwrap_or_else(|| {
                if pointer.x <= layout.message_x {
                    0
                } else {
                    text.len()
                }
            })
    };
    let mut menu_word = None;
    if let Some(pointer) = resp.interact_pointer_pos() {
        if resp.clicked() && pointer.x < rect.left() + 6.0 + 16.0 {
            out.toggle_bookmark = Some(lr);
        }
        if let Some(hit) = hits.iter().find(|hit| hit.rect.contains(pointer)) {
            let local = vec2(pointer.x - hit.text_pos.x, pointer.y - hit.text_pos.y);
            let char_idx = hit.galley.cursor_from_pos(local).ccursor.index;
            let global_char = text[..hit.range.0].chars().count() + char_idx;
            if resp.double_clicked() {
                if let Some(word) = word_at_char(text, global_char) {
                    out.highlight = Some((lr, word.to_string()));
                }
            }
            if resp.secondary_clicked() {
                menu_word = word_at_char(text, global_char).map(str::to_string);
            }
        }
        if resp.drag_started() {
            let origin = ui
                .input(|input| input.pointer.press_origin())
                .unwrap_or(pointer);
            if origin.x <= gutter_cut {
                begin_drag(ui, DragMode::Lines);
                out.selection = Some(line_selection(idx, idx));
            } else {
                begin_drag(ui, DragMode::Text);
                out.selection = Some(Selection::new(idx, byte_at(origin)));
            }
        }
        if resp.triple_clicked() && pointer.x > gutter_cut {
            out.selection = Some(line_selection(idx, idx));
        } else if resp.clicked() && !resp.double_clicked() && !resp.triple_clicked() {
            let shift = ui.input(|input| input.modifiers.shift);
            if shift {
                let anchor = out
                    .selection
                    .map(|sel| (sel.anchor_row, sel.anchor_byte))
                    .unwrap_or((idx, byte_at(pointer)));
                out.selection = Some(Selection {
                    anchor_row: anchor.0,
                    anchor_byte: anchor.1,
                    focus_row: idx,
                    focus_byte: byte_at(pointer),
                });
            } else if pointer.x > rect.left() + 6.0 + 16.0 && pointer.x <= gutter_cut {
                out.selection = Some(line_selection(idx, idx));
            } else if pointer.x > gutter_cut {
                out.selection = None;
            }
        }
    }
    extend_drag(ui, rect, idx, out, byte_at);
    row_context_menu(
        ui,
        &resp,
        session,
        lr,
        idx,
        text,
        menu_word,
        None,
        out.selection,
        out,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_cell<'a, 'b>(
    ui: &egui::Ui,
    cell: &'b ColumnCell<'a>,
    x: f32,
    right: f32,
    rect: Rect,
    font_size: f32,
    base_color: Color32,
    bold: bool,
    spans: &[StyledSpan],
    selection: Option<Selection>,
    row: usize,
    hits: &mut Vec<CellHit<'b>>,
) {
    if x >= right {
        return;
    }
    let clip = Rect::from_min_max(pos2(x, rect.top() + 1.0), pos2(right, rect.bottom() - 1.0));
    if clip.width() <= 0.0 {
        return;
    }
    let painter = ui.painter().with_clip_rect(clip);
    let font = FontId::monospace(font_size);
    let text = cell.text.as_ref();

    if let (Some((start, end)), Some(sel)) = (cell.range, selection) {
        if let Some((from, to)) = sel.row_range(row) {
            let from = floor_boundary(text, from.clamp(start, end) - start);
            let to = floor_boundary(text, to.clamp(start, end) - start);
            if from < to {
                let x0 = text_width(ui, &text[..from], &font, base_color);
                let x1 = text_width(ui, &text[..to], &font, base_color);
                painter.rect_filled(
                    Rect::from_min_max(pos2(x + x0, clip.top()), pos2(x + x1, clip.bottom())),
                    0.0,
                    theme::p().selection,
                );
            }
        }
    }

    let mut local = Vec::new();
    if let Some((start, end)) = cell.range {
        for span in spans {
            let from = floor_boundary(text, span.start.max(start) - start);
            let to = floor_boundary(text, span.end.min(end) - start);
            if from < to {
                if let Some(mapped) = StyledSpan::new(from, to, span.style) {
                    local.push(mapped);
                }
            }
        }
    }
    let job = styled_job(text, base_color, bold, &local, font_size);
    let galley = ui.fonts(|fonts| fonts.layout_job(job));
    let text_pos = pos2(x, rect.top() + (rect.height() - galley.size().y) * 0.5);
    painter.galley(text_pos, galley.clone(), base_color);

    if let Some(range) = cell.range {
        hits.push(CellHit {
            rect: Rect::from_min_max(pos2(x, rect.top()), pos2(right, rect.bottom())),
            text_pos,
            text,
            range,
            galley,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_wrapped(
    ui: &mut egui::Ui,
    session: &Session,
    view: ViewId,
    row_h: f32,
    gutter_w: f32,
    cols: usize,
    viewport: Rect,
    cache: &WrapCache,
    row_cache: &mut RowCache,
    font_size: f32,
    find: Option<&SearchMatcher>,
    out: &mut Response,
) {
    let total_visual = cache.total_visual();
    let content_height = total_visual as f32 * row_h;
    let width = ui.available_width();
    ui.set_min_size(vec2(width, content_height));
    let origin = ui.min_rect().min;

    let first_visual = (viewport.min.y / row_h).floor().max(0.0) as u64;
    let last_visual = (viewport.max.y / row_h).ceil().max(0.0) as u64;
    let mut row = cache.row_at_visual(first_visual);
    while row < cache.counts.len() {
        let start_visual = cache.prefix[row];
        if start_visual > last_visual {
            break;
        }
        let height = cache.count(row) as f32 * row_h;
        let rect = Rect::from_min_size(
            pos2(origin.x, origin.y + start_visual as f32 * row_h),
            vec2(width, height),
        );
        let resp = ui.interact(
            rect,
            ui.id().with(("wrap-row", row)),
            Sense::click_and_drag(),
        );
        draw_wrapped_row(
            ui, session, view, row, rect, &resp, row_h, gutter_w, cols, font_size, find, row_cache,
            out,
        );
        row += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_wrapped_row(
    ui: &mut egui::Ui,
    session: &Session,
    view: ViewId,
    idx: usize,
    rect: Rect,
    resp: &egui::Response,
    row_h: f32,
    gutter_w: f32,
    cols: usize,
    font_size: f32,
    find: Option<&SearchMatcher>,
    row_cache: &mut RowCache,
    out: &mut Response,
) {
    let Some(lr) = session.line_ref(view, idx) else {
        return;
    };
    let text = session.line_text(lr);
    let paint = row_cache.paint(session, lr, find);
    let row_style = paint.style;
    let base_color = paint.base_color;
    let spans = paint.spans.as_slice();

    let stripe = if idx % 2 == 0 {
        theme::p().row_even
    } else {
        theme::p().row_odd
    };
    let bg = row_style.bg.map(theme::color).unwrap_or(stripe);
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, bg);
    if out.selected == Some(lr) {
        painter.rect_filled(rect, 0.0, theme::p().selected_row);
    } else if resp.hovered() {
        painter.rect_filled(rect, 0.0, theme::p().hover);
    }
    painter.rect_filled(
        Rect::from_min_size(rect.left_top(), vec2(3.0, rect.height())),
        egui::Rounding {
            nw: 0.0,
            sw: 0.0,
            ne: 1.5,
            se: 1.5,
        },
        theme::doc_color(lr.doc),
    );

    let bookmark_center = pos2(rect.left() + 15.0, rect.top() + row_h * 0.5);
    if session.is_bookmarked(lr) {
        painter.circle_stroke(
            bookmark_center,
            6.5,
            Stroke::new(1.0_f32, theme::p().bookmark.gamma_multiply(0.35)),
        );
        painter.circle_filled(bookmark_center, 3.5, theme::p().bookmark);
    } else if resp.hovered() {
        painter.circle_stroke(
            bookmark_center,
            4.0,
            Stroke::new(1.0_f32, theme::p().border),
        );
    }

    painter.line_segment(
        [
            pos2(rect.left() + gutter_w - 4.0, rect.top()),
            pos2(rect.left() + gutter_w - 4.0, rect.bottom()),
        ],
        Stroke::new(1.0_f32, theme::p().border_soft),
    );

    painter.text(
        pos2(rect.left() + gutter_w - 8.0, rect.top() + row_h * 0.5),
        Align2::RIGHT_CENTER,
        (lr.line as usize + 1).to_string(),
        FontId::monospace(font_size - 1.0),
        if out.selected == Some(lr) {
            theme::p().accent_strong
        } else {
            theme::p().faint
        },
    );

    let segments = wrap_segments(text, cols);
    let font = FontId::monospace(font_size);
    let selection = out.selection;
    let mut galleys = Vec::with_capacity(segments.len());
    for (seg, (start, end)) in segments.iter().copied().enumerate() {
        let y = rect.top() + seg as f32 * row_h + 1.0;
        let text_pos = pos2(rect.left() + gutter_w, y);
        if let Some(sel) = selection {
            if let Some((from, to)) = sel.row_range(idx) {
                let from = floor_boundary(text, from.clamp(start, end));
                let to = floor_boundary(text, to.clamp(start, end));
                if from < to {
                    let x0 = text_width(ui, &text[start..from], &font, base_color);
                    let x1 = text_width(ui, &text[start..to], &font, base_color);
                    painter.rect_filled(
                        Rect::from_min_max(
                            pos2(text_pos.x + x0, y),
                            pos2(text_pos.x + x1, y + row_h - 2.0),
                        ),
                        0.0,
                        theme::p().selection,
                    );
                }
            }
        }
        let mut local = Vec::new();
        let segment = &text[start..end];
        for span in spans {
            let from = floor_boundary(segment, span.start.max(start) - start);
            let to = floor_boundary(segment, span.end.min(end) - start);
            if from < to {
                if let Some(mapped) = StyledSpan::new(from, to, span.style) {
                    local.push(mapped);
                }
            }
        }
        let job = styled_job(segment, base_color, row_style.bold, &local, font_size);
        let galley = ui.fonts(|fonts| fonts.layout_job(job));
        painter.galley(text_pos, galley.clone(), base_color);
        galleys.push((text_pos, galley, start, end));
    }

    if resp.clicked() || resp.double_clicked() {
        out.selected = Some(lr);
    }
    let gutter_cut = rect.left() + gutter_w;
    let byte_at = |pointer: egui::Pos2| -> usize {
        if pointer.x <= gutter_cut || galleys.is_empty() {
            return 0;
        }
        let seg_idx =
            (((pointer.y - rect.top()) / row_h).floor().max(0.0) as usize).min(galleys.len() - 1);
        let (seg_pos, galley, seg_start, seg_end) = &galleys[seg_idx];
        let local = vec2(pointer.x - seg_pos.x, pointer.y - seg_pos.y);
        let char_idx = galley.cursor_from_pos(local).ccursor.index;
        seg_start + byte_of_char(&text[*seg_start..*seg_end], char_idx)
    };
    let mut menu_word = None;
    if let Some(pointer) = resp.interact_pointer_pos() {
        if resp.clicked() && pointer.x < rect.left() + 6.0 + 16.0 {
            out.toggle_bookmark = Some(lr);
        }
        if pointer.x > gutter_cut && !galleys.is_empty() {
            let seg_idx =
                (((pointer.y - rect.top()) / row_h).floor() as usize).min(galleys.len() - 1);
            let (text_pos, galley, seg_start, _) = &galleys[seg_idx];
            let local = vec2(pointer.x - text_pos.x, pointer.y - text_pos.y);
            let char_idx = galley.cursor_from_pos(local).ccursor.index;
            let global_char = text[..*seg_start].chars().count() + char_idx;
            if resp.double_clicked() {
                if let Some(word) = word_at_char(text, global_char) {
                    out.highlight = Some((lr, word.to_string()));
                }
            }
            if resp.secondary_clicked() {
                menu_word = word_at_char(text, global_char).map(str::to_string);
            }
        }
        if resp.drag_started() {
            let origin = ui
                .input(|input| input.pointer.press_origin())
                .unwrap_or(pointer);
            if origin.x <= gutter_cut {
                begin_drag(ui, DragMode::Lines);
                out.selection = Some(line_selection(idx, idx));
            } else {
                begin_drag(ui, DragMode::Text);
                out.selection = Some(Selection::new(idx, byte_at(origin)));
            }
        }
        if resp.triple_clicked() && pointer.x > gutter_cut {
            out.selection = Some(line_selection(idx, idx));
        } else if resp.clicked() && !resp.double_clicked() && !resp.triple_clicked() {
            let shift = ui.input(|input| input.modifiers.shift);
            if shift {
                let anchor = out
                    .selection
                    .map(|sel| (sel.anchor_row, sel.anchor_byte))
                    .unwrap_or((idx, byte_at(pointer)));
                out.selection = Some(Selection {
                    anchor_row: anchor.0,
                    anchor_byte: anchor.1,
                    focus_row: idx,
                    focus_byte: byte_at(pointer),
                });
            } else if pointer.x > rect.left() + 6.0 + 16.0 && pointer.x <= gutter_cut {
                out.selection = Some(line_selection(idx, idx));
            } else if pointer.x > gutter_cut {
                out.selection = None;
            }
        }
    }
    extend_drag(ui, rect, idx, out, byte_at);
    row_context_menu(
        ui,
        resp,
        session,
        lr,
        idx,
        text,
        menu_word,
        None,
        out.selection,
        out,
    );
}

fn text_width(ui: &egui::Ui, text: &str, font: &FontId, color: Color32) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    ui.fonts(|fonts| {
        fonts
            .layout_no_wrap(text.to_string(), font.clone(), color)
            .size()
            .x
    })
}

fn char_cells(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

fn wrap_segments(text: &str, cols: usize) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut width = 0usize;
    for (idx, ch) in text.char_indices() {
        let cells = char_cells(ch);
        if width > 0 && width + cells > cols {
            segments.push((start, idx));
            start = idx;
            width = 0;
        }
        width += cells;
    }
    segments.push((start, text.len()));
    segments
}

fn wrap_count(text: &str, cols: usize) -> usize {
    if text.is_empty() {
        return 1;
    }
    let mut lines = 1usize;
    let mut width = 0usize;
    for ch in text.chars() {
        let cells = char_cells(ch);
        if width > 0 && width + cells > cols {
            lines += 1;
            width = 0;
        }
        width += cells;
    }
    lines
}

fn byte_of_char(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

fn append_find_spans(text: &str, matcher: &SearchMatcher, out: &mut Vec<StyledSpan>) {
    let mut ranges = Vec::new();
    matcher.find_all(text, &mut ranges);
    for (start, end) in ranges {
        let style = TextStyle {
            bg: Some(theme::p().find_bg),
            ..Default::default()
        };
        if let Some(span) = StyledSpan::new(start, end, style) {
            out.push(span);
        }
    }
}

/// Builds the styled layout for one line. Spans must lie on char boundaries of
/// `text`; `RowCache` normalizes them once when it decorates a row.
fn styled_job(
    text: &str,
    base_color: Color32,
    base_bold: bool,
    spans: &[StyledSpan],
    font_size: f32,
) -> LayoutJob {
    let font = FontId::monospace(font_size);
    let bold_font = FontId::new(font_size, FontFamily::Name("mono-bold".into()));

    let mut cuts = Vec::with_capacity(spans.len() * 2 + 2);
    cuts.push(0);
    cuts.push(text.len());
    for span in spans {
        if span.start < span.end {
            cuts.push(span.start);
            cuts.push(span.end);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();

    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        let mut format = TextFormat {
            font_id: if base_bold {
                bold_font.clone()
            } else {
                font.clone()
            },
            color: base_color,
            ..Default::default()
        };
        for span in spans.iter() {
            if span.start <= start && span.end >= end {
                if let Some(fg) = span.style.fg {
                    format.color = theme::ink(fg);
                }
                if let Some(bg) = span.style.bg {
                    format.background = theme::color(bg);
                }
                if span.style.bold {
                    format.font_id = bold_font.clone();
                }
            }
        }
        job.append(&text[start..end], 0.0, format);
    }
    job
}

fn floor_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use neolog_core::matcher::SearchOptions;
    use neolog_core::plugin::PluginRegistry;
    use neolog_core::style::Rgb;
    use neolog_core::style::{HighlightRule, RuleScope, TextStyle};
    use neolog_plugins::builtin_plugins;

    fn session_with(text: &str) -> (Session, LineRef) {
        let mut session = Session::new(PluginRegistry::new(builtin_plugins()));
        let doc = session.open_text("cache.log", text);
        let view = session.create_doc_view(doc).unwrap();
        let lr = session.line_ref(view, 0).unwrap();
        (session, lr)
    }

    #[test]
    fn paint_cache_reuses_rows_until_revision_changes() {
        let (mut session, lr) = session_with("ERROR boom\n");
        let mut cache = RowCache::default();
        cache.begin(&session, 13.5, None);
        let spans = cache.paint(&session, lr, None).spans.len();
        assert!(spans >= 1, "generic plugin decorates the severity keyword");

        cache.begin(&session, 13.5, None);
        cache.paint(&session, lr, None);
        assert_eq!(cache.rows.len(), 1);

        session.add_rule(HighlightRule::plain(
            "boom",
            RuleScope::Global,
            "boom",
            false,
            TextStyle {
                fg: Some(Rgb(1, 2, 3)),
                ..Default::default()
            },
            None,
        ));
        cache.begin(&session, 13.5, None);
        assert!(cache.rows.is_empty());
        assert!(cache.paint(&session, lr, None).spans.len() > spans);
    }

    #[test]
    fn paint_cache_tracks_find_query_and_font_size() {
        let (session, lr) = session_with("alpha beta alpha\n");
        let matcher = SearchMatcher::build("alpha", SearchOptions::default())
            .unwrap()
            .unwrap();
        let mut cache = RowCache::default();
        cache.begin(&session, 13.5, Some(&matcher));
        assert_eq!(cache.paint(&session, lr, Some(&matcher)).spans.len(), 2);

        cache.begin(&session, 13.5, None);
        assert!(cache.rows.is_empty());
        assert_eq!(cache.paint(&session, lr, None).spans.len(), 0);

        cache.begin(&session, 18.0, None);
        assert!(cache.rows.is_empty());
    }

    #[test]
    fn line_selection_covers_whole_rows_both_directions() {
        let down = line_selection(2, 4);
        for row in 2..=4 {
            assert_eq!(down.row_range(row), Some((0, usize::MAX)));
        }
        assert_eq!(down.row_range(1), None);
        assert_eq!(down.row_range(5), None);

        let up = line_selection(4, 2);
        for row in 2..=4 {
            assert_eq!(up.row_range(row), Some((0, usize::MAX)));
        }
        assert_eq!(up.row_range(1), None);
        assert_eq!(up.row_range(5), None);

        let single = line_selection(3, 3);
        assert_eq!(single.row_range(3), Some((0, usize::MAX)));
        assert!(!single.is_empty());
    }
}
