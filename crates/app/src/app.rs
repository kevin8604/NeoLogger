use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Align, Align2, Key, Layout, Modifiers, RichText, TextEdit, Ui};
use neolog_core::matcher::{SearchMatcher, SearchOptions};
use neolog_core::DocId;
use neolog_core::{
    format_ts, Base, HighlightRule, LineRef, PluginRegistry, ReloadMode, Rgb, RuleScope, Session,
    SessionSnapshot, Severity, TextEncoding, TextStyle, ViewId,
};
use neolog_plugins::builtin_plugins;
use notify::Watcher;

use crate::{config, demo, log_view, stats, theme};

pub struct NeoLogApp {
    session: Session,
    active: Vec<ViewId>,
    /// Current leaf per root view: a tab is a root, and switching back to it
    /// restores the filtered level the user was reading.
    leaves: HashMap<ViewId, ViewId>,
    filter_focus: bool,
    filter_error: Option<String>,
    filter_help: bool,
    highlight_pop: Option<PendingHighlight>,
    synced_view: Option<ViewId>,
    filter_input: String,
    keyword_input: String,
    highlight_color: [u8; 3],
    selected: Option<LineRef>,
    selection: Option<log_view::Selection>,
    merge_selected: Vec<bool>,
    pending_scroll: Option<log_view::Scroll>,
    status: String,
    open_requested: bool,
    follow: bool,
    tab_mru: Vec<ViewId>,
    side_root: Option<ViewId>,
    side_sync: bool,
    side_selected: Option<LineRef>,
    side_selection: Option<log_view::Selection>,
    side_scroll: Option<log_view::Scroll>,
    side_offset: f32,
    side_wrap: log_view::WrapCache,
    side_fold: log_view::FoldCache,
    side_columns: log_view::ColumnsCache,
    side_rows: log_view::RowCache,
    sync_main: f32,
    sync_side: f32,
    presets: Vec<config::Preset>,
    preset_open: bool,
    preset_name: String,
    dir_pop: Option<PathBuf>,
    dir_glob: String,
    watched: Vec<WatchedRoot>,
    watcher: Option<DirWatcher>,
    wrap: bool,
    wrap_cache: log_view::WrapCache,
    watch: Vec<WatchFile>,
    last_watch: Instant,
    saved_on_close: bool,
    saved_structure: u64,
    save_failed_rev: u64,
    font_size: f32,
    find_open: bool,
    find_input: String,
    find_focus: bool,
    find_options: SearchOptions,
    find_history: Vec<String>,
    find_cache: FindCache,
    goto_open: bool,
    goto_input: String,
    goto_focus: bool,
    global_open: bool,
    global_focus: bool,
    global_cache: Option<GlobalCache>,
    global_error: Option<String>,
    stats_open: bool,
    stats_cache: stats::StatsCache,
    last_scroll_offset: f32,
    last_viewport_height: f32,
    fold: bool,
    fold_cache: log_view::FoldCache,
    columns: bool,
    columns_cache: log_view::ColumnsCache,
    row_cache: log_view::RowCache,
    rename_open: bool,
    rename_target: Option<ViewId>,
    rename_title: String,
    rename_note: String,
    rename_focus: bool,
    help_open: bool,
    about_open: bool,
    filter_bar_open: bool,
    sidebar_open: bool,
    sidebar_guard: bool,
}

#[derive(Default)]
struct FindCache {
    key: Option<(ViewId, u64, String, SearchOptions)>,
    matcher: Option<SearchMatcher>,
    error: Option<String>,
    matches: Vec<usize>,
    cursor: Option<usize>,
}

#[derive(Clone, Copy)]
struct GlobalHit {
    doc: DocId,
    line: u32,
}

struct GlobalCache {
    query: String,
    revision: u64,
    matcher: SearchMatcher,
    hits: Vec<GlobalHit>,
    truncated: bool,
}

/// First bytes hashed for change detection: catches in-place rewrites that keep
/// the same length, while an append leaves this prefix untouched.
const HEAD_BYTES: usize = 4096;
const FONT_MIN: f32 = 9.0;
const FONT_MAX: f32 = 28.0;

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    head: u64,
    tail: u64,
}

struct WatchFile {
    stamp: Option<FileStamp>,
    missing: bool,
}

struct WatchedRoot {
    path: PathBuf,
    patterns: Vec<String>,
    merge: Option<ViewId>,
}

struct DirWatcher {
    watcher: notify::RecommendedWatcher,
    rx: std::sync::mpsc::Receiver<Result<notify::Event, notify::Error>>,
    dirs: HashSet<PathBuf>,
}

impl DirWatcher {
    fn open() -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = notify::recommended_watcher(tx).map_err(|error| error.to_string())?;
        Ok(Self {
            watcher,
            rx,
            dirs: HashSet::new(),
        })
    }

    fn watch_dir(&mut self, path: &Path) {
        if !path.is_dir() || !self.dirs.insert(path.to_path_buf()) {
            return;
        }
        let _ = self.watcher.watch(path, notify::RecursiveMode::Recursive);
    }

    fn kicked(&self) -> bool {
        let mut any = false;
        while self.rx.try_recv().is_ok() {
            any = true;
        }
        any
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChangeKind {
    Append,
    Rewrite,
}

#[derive(Clone)]
struct PendingHighlight {
    line: LineRef,
    word: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reading {
    Plain,
    Wrap,
    Fold,
    Columns,
}

fn hash_bytes(buf: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    buf.hash(&mut hasher);
    hasher.finish()
}

fn hash_prefix(path: &Path, max: usize) -> Option<u64> {
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(max.min(HEAD_BYTES));
    file.take(max as u64).read_to_end(&mut buf).ok()?;
    Some(hash_bytes(&buf))
}

fn hash_tail(path: &Path, len: u64) -> Option<u64> {
    let mut file = std::fs::File::open(path).ok()?;
    let start = len.saturating_sub(HEAD_BYTES as u64);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(HEAD_BYTES as u64).read_to_end(&mut buf).ok()?;
    Some(hash_bytes(&buf))
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let len = meta.len();
    Some(FileStamp {
        len,
        modified: meta.modified().ok(),
        created: meta.created().ok(),
        head: hash_prefix(path, HEAD_BYTES).unwrap_or(0),
        tail: hash_tail(path, len).unwrap_or(0),
    })
}

/// `None` means nothing changed. A longer file whose old prefix is intact is an
/// append. Any same-length change, including a rewrite past the first 4KB, is a
/// rewrite so bookmarks are not kept against a different body.
fn change_kind(path: &Path, previous: &FileStamp, current: &FileStamp) -> Option<ChangeKind> {
    if previous.len == current.len
        && previous.modified == current.modified
        && previous.created == current.created
        && previous.head == current.head
        && previous.tail == current.tail
    {
        return None;
    }
    if previous.created != current.created || current.len < previous.len {
        return Some(ChangeKind::Rewrite);
    }
    if current.len > previous.len {
        let probe = previous.len.min(HEAD_BYTES as u64) as usize;
        if hash_prefix(path, probe) != Some(previous.head) {
            return Some(ChangeKind::Rewrite);
        }
        return Some(ChangeKind::Append);
    }
    Some(ChangeKind::Rewrite)
}

impl NeoLogApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        let ui_prefs = config::load_ui();
        let theme_choice = ui_prefs
            .theme
            .as_deref()
            .and_then(theme::Theme::from_id)
            .unwrap_or_default();
        theme::set_theme(theme_choice);
        theme::apply_style(&cc.egui_ctx);

        let report = neolog_plugins::load_dir(&config::plugins_dir());
        let mut plugins = builtin_plugins();
        plugins.extend(report.plugins);
        let mut session = Session::new(PluginRegistry::new(plugins));

        let mut active = Vec::new();
        let mut status = String::new();
        let mut restored = false;
        match config::load_session() {
            Ok(Some(snapshot)) => match session.restore(&snapshot) {
                Ok(report) if !session.roots().is_empty() || !session.docs().is_empty() => {
                    active = snapshot
                        .active
                        .iter()
                        .rev()
                        .find(|id| session.view(**id).is_some())
                        .map(|id| session.view_path(*id))
                        .or_else(|| session.roots().last().map(|id| session.view_path(*id)))
                        .unwrap_or_default();
                    restored = true;
                    status = format!(
                        "已恢复会话 · {} 个文件 · {} 个视图",
                        session.docs().len(),
                        session.roots().len()
                    );
                    if report.missing > 0 {
                        status.push_str(&format!(" · {} 个文件不可用", report.missing));
                    }
                }
                Ok(_) => {}
                Err(error) => status = format!("会话恢复失败: {error}"),
            },
            Ok(None) => {}
            Err(error) => status = format!("{error}"),
        }

        if !restored {
            let demo_view = demo::load(&mut session);
            active = demo_view
                .map(|view| session.view_path(view))
                .unwrap_or_default();
            status = format!("已加载示例日志 · {} 行", session.total_lines());
        }
        if !report.errors.is_empty() {
            for error in &report.errors {
                eprintln!("动态插件加载失败: {error}");
            }
            status.push_str(&format!(" · {} 个动态插件加载失败", report.errors.len()));
        }

        let mut leaves = HashMap::new();
        if let (Some(root), Some(leaf)) = (active.first().copied(), active.last().copied()) {
            leaves.insert(root, leaf);
        }
        let synced_view = active.last().copied();
        let filter_input = active
            .last()
            .and_then(|view| session.view(*view))
            .map(|view| view.query.clone())
            .unwrap_or_default();
        let merge_selected = vec![false; session.docs().len()];

        let saved_structure = session.structure_rev();
        let user = config::load_user();
        let tab_seed = active.first().copied();
        let mut app = Self {
            session,
            active,
            leaves,
            filter_focus: false,
            filter_error: None,
            filter_help: false,
            highlight_pop: None,
            synced_view,
            filter_input,
            keyword_input: String::new(),
            highlight_color: [255, 196, 64],
            selected: None,
            selection: None,
            merge_selected,
            pending_scroll: None,
            status,
            open_requested: false,
            follow: false,
            tab_mru: tab_seed.into_iter().collect(),
            side_root: None,
            side_sync: false,
            side_selected: None,
            side_selection: None,
            side_scroll: None,
            side_offset: 0.0,
            side_wrap: log_view::WrapCache::default(),
            side_fold: log_view::FoldCache::default(),
            side_columns: log_view::ColumnsCache::default(),
            side_rows: log_view::RowCache::default(),
            sync_main: 0.0,
            sync_side: 0.0,
            presets: user.presets,
            preset_open: false,
            preset_name: String::new(),
            dir_pop: None,
            dir_glob: "*.log,*.txt,*.out,*.err,*.jsonl,*.log.*".to_string(),
            watched: Vec::new(),
            watcher: None,
            wrap: false,
            wrap_cache: log_view::WrapCache::default(),
            watch: Vec::new(),
            last_watch: Instant::now(),
            saved_on_close: false,
            saved_structure,
            save_failed_rev: u64::MAX,
            font_size: log_view::FONT_SIZE,
            find_open: false,
            find_input: String::new(),
            find_focus: false,
            find_options: SearchOptions::default(),
            find_history: user.find_history,
            find_cache: FindCache::default(),
            goto_open: false,
            goto_input: String::new(),
            goto_focus: false,
            global_open: false,
            global_focus: false,
            global_cache: None,
            global_error: None,
            stats_open: ui_prefs.stats_open.unwrap_or(false),
            stats_cache: stats::StatsCache::default(),
            last_scroll_offset: 0.0,
            last_viewport_height: 0.0,
            fold: false,
            fold_cache: log_view::FoldCache::default(),
            columns: false,
            columns_cache: log_view::ColumnsCache::default(),
            row_cache: log_view::RowCache::default(),
            rename_open: false,
            rename_target: None,
            rename_title: String::new(),
            rename_note: String::new(),
            rename_focus: false,
            help_open: false,
            about_open: false,
            filter_bar_open: ui_prefs.filter_bar_open.unwrap_or(true),
            sidebar_open: ui_prefs.sidebar_open.unwrap_or(false),
            sidebar_guard: false,
        };
        app.arm_watch();
        app
    }

    fn arm_watch(&mut self) {
        self.watch = self
            .session
            .docs()
            .iter()
            .map(|doc| {
                let stamp = file_stamp(&doc.path);
                WatchFile {
                    missing: stamp.is_none(),
                    stamp,
                }
            })
            .collect();
        self.last_watch = Instant::now();
    }

    fn check_tail(&mut self, ctx: &egui::Context) {
        if !self.follow {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        if self.last_watch.elapsed() < Duration::from_millis(600) {
            return;
        }
        self.last_watch = Instant::now();
        if self.watch.len() != self.session.docs().len() {
            self.arm_watch();
            return;
        }

        let mut changed = false;
        let mut notes = Vec::new();
        let mut rewritten = Vec::new();
        let mut failure = None;
        for doc_id in 0..self.session.docs().len() {
            let (path, name, old_lines) = {
                let doc = &self.session.docs()[doc_id];
                (doc.path.clone(), doc.short_name(), doc.len())
            };
            let current = file_stamp(&path);
            let Some(current) = current else {
                if !self.watch[doc_id].missing {
                    self.watch[doc_id].missing = true;
                    self.session.mark_unavailable(doc_id as DocId);
                    notes.push(format!("{name} 已消失，保留现有内容"));
                }
                continue;
            };
            let kind = match self.watch[doc_id].stamp {
                Some(previous) => change_kind(&path, &previous, &current),
                None => Some(ChangeKind::Rewrite),
            };
            let Some(kind) = kind else {
                // The file came back unchanged after a transient miss.
                self.watch[doc_id].missing = false;
                continue;
            };
            let mode = match kind {
                ChangeKind::Append => ReloadMode::Append,
                ChangeKind::Rewrite => ReloadMode::Rewrite,
            };
            match self.session.reload_doc(doc_id as DocId, mode) {
                Ok(report) => {
                    self.watch[doc_id].stamp = Some(current);
                    self.watch[doc_id].missing = false;
                    changed = true;
                    match kind {
                        ChangeKind::Append => notes.push(format!(
                            "{name} 追加 {} 行",
                            report.lines.saturating_sub(old_lines)
                        )),
                        ChangeKind::Rewrite => {
                            rewritten.push(doc_id as DocId);
                            notes.push(format!(
                                "{name} 已轮转/重写（{} 行，丢弃 {} 个书签）",
                                report.lines, report.dropped_bookmarks
                            ));
                        }
                    }
                }
                Err(error) => failure = Some(format!("{name}: {error}")),
            }
        }
        if changed {
            self.selection = None;
            if let Some(line) = self.selected {
                let gone = self
                    .session
                    .doc(line.doc)
                    .is_none_or(|doc| line.line as usize >= doc.len());
                if gone || rewritten.contains(&line.doc) {
                    self.selected = None;
                }
            }
            self.pending_scroll = Some(log_view::Scroll::Offset(f32::MAX));
        }
        if changed || !notes.is_empty() {
            self.status = if changed {
                let detail = if notes.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", notes.join("；"))
                };
                format!("跟随后更新 · 共 {} 行{detail}", self.session.total_lines())
            } else {
                format!("跟随检查: {}", notes.join("；"))
            };
        }
        if let Some(error) = failure {
            self.status = format!("跟随重载失败: {error}");
        }
        self.ingest_new_files();
    }

    fn reload_all(&mut self) {
        let mut reloaded = 0usize;
        let mut rewritten = 0usize;
        let mut dropped = 0usize;
        let mut skipped = 0usize;
        let mut rewritten_docs = Vec::new();
        let mut failures = Vec::new();
        for doc_id in 0..self.session.docs().len() {
            let (path, name) = {
                let doc = &self.session.docs()[doc_id];
                (doc.path.clone(), doc.short_name())
            };
            let Some(current) = file_stamp(&path) else {
                self.session.mark_unavailable(doc_id as DocId);
                skipped += 1;
                continue;
            };
            let kind = match self.watch.get(doc_id) {
                Some(watch) if !watch.missing => watch
                    .stamp
                    .and_then(|previous| change_kind(&path, &previous, &current)),
                _ => Some(ChangeKind::Rewrite),
            };
            let mode = match kind {
                Some(ChangeKind::Rewrite) => ReloadMode::Rewrite,
                _ => ReloadMode::Append,
            };
            match self.session.reload_doc(doc_id as DocId, mode) {
                Ok(report) => {
                    reloaded += 1;
                    if let Some(watch) = self.watch.get_mut(doc_id) {
                        watch.stamp = Some(current);
                        watch.missing = false;
                    }
                    if mode == ReloadMode::Rewrite {
                        rewritten += 1;
                        dropped += report.dropped_bookmarks;
                        rewritten_docs.push(doc_id as DocId);
                    }
                }
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }
        self.selection = None;
        if let Some(line) = self.selected {
            let gone = self
                .session
                .doc(line.doc)
                .is_none_or(|doc| line.line as usize >= doc.len());
            if gone || rewritten_docs.contains(&line.doc) {
                self.selected = None;
            }
        }
        if self.follow {
            self.pending_scroll = Some(log_view::Scroll::Offset(f32::MAX));
        }
        self.last_watch = Instant::now();
        if self.watch.len() != self.session.docs().len() {
            self.arm_watch();
        }

        let mut message = format!(
            "已重载 {reloaded} 个文件 · 共 {} 行",
            self.session.total_lines()
        );
        if rewritten > 0 {
            message.push_str(&format!(" · 轮转/重写 {rewritten} 个"));
            if dropped > 0 {
                message.push_str(&format!("（丢弃 {dropped} 个书签）"));
            }
        }
        if skipped > 0 {
            message.push_str(&format!(" · 跳过 {skipped} 个内存/缺失文件"));
        }
        if !failures.is_empty() {
            message.push_str(&format!(" · {} 个失败: {}", failures.len(), failures[0]));
        }
        self.status = message;
    }

    fn snapshot(&self) -> SessionSnapshot {
        let mut snapshot = self.session.snapshot();
        snapshot.active = self.active.clone();
        snapshot
    }

    fn save_session(&mut self, quiet: bool) {
        let rev = self.session.structure_rev();
        let docs_blocked = self
            .session
            .docs()
            .iter()
            .any(|doc| !doc.missing && !doc.path.is_file());
        let persistable = self
            .session
            .docs()
            .iter()
            .any(|doc| doc.missing || doc.path.is_file());
        if self.session.docs().is_empty() || docs_blocked || !persistable {
            if !quiet {
                self.status = "示例或内存文档无法持久化，未保存会话".to_string();
            }
            self.saved_structure = rev;
            return;
        }
        match config::save_session(&self.snapshot()) {
            Ok(()) => {
                self.saved_structure = rev;
                self.save_failed_rev = u64::MAX;
                if !quiet {
                    self.status = format!("会话已保存到 {}", config::session_path().display());
                }
            }
            Err(error) => {
                self.save_failed_rev = rev;
                if !quiet || self.status != error {
                    self.status = error;
                }
            }
        }
    }

    fn copy_selection(&self) -> Option<String> {
        let view = self.active.last().copied()?;
        if let Some(selection) = self.selection.filter(|selection| !selection.is_empty()) {
            let ((start_row, start_byte), (end_row, end_byte)) = selection.bounds();
            let mut parts = Vec::new();
            for row in start_row..=end_row {
                let lr = self.session.line_ref(view, row)?;
                let text = self.session.line_text(lr);
                let from = if row == start_row {
                    floor_char(text, start_byte)
                } else {
                    0
                };
                let to = if row == end_row {
                    floor_char(text, end_byte)
                } else {
                    text.len()
                };
                parts.push(text[from..to].to_string());
            }
            return Some(parts.join("\n"));
        }
        let lr = self.selected?;
        self.session
            .view_docs(view)
            .contains(&lr.doc)
            .then(|| self.session.event_text(lr))
    }

    fn copy_to_clipboard(&mut self, ctx: &egui::Context) {
        match self.copy_selection() {
            Some(text) => {
                let event = self
                    .selection
                    .as_ref()
                    .is_none_or(|selection| selection.is_empty());
                ctx.copy_text(text);
                self.status = if event {
                    "已复制整条事件".to_string()
                } else {
                    "已复制到剪贴板".to_string()
                };
            }
            None => self.status = "没有可复制的文本".to_string(),
        }
    }

    fn select_all(&mut self) {
        let Some(view) = self.active.last().copied() else {
            self.status = "没有活动视图".to_string();
            return;
        };
        let rows = self.session.visible_len(view);
        if rows == 0 {
            self.status = "当前视图没有可选择的日志行".to_string();
            return;
        }
        self.selection = Some(log_view::Selection {
            anchor_row: 0,
            anchor_byte: 0,
            focus_row: rows - 1,
            focus_byte: usize::MAX,
        });
        self.status = format!("已选择 {rows} 行 · Ctrl+C 复制");
    }

    fn open_find(&mut self) {
        self.find_open = true;
        self.find_focus = true;
    }

    fn open_global_search(&mut self) {
        self.global_open = true;
        self.find_open = true;
        self.global_focus = true;
        self.global_error = None;
        if !self.find_input.is_empty() {
            self.run_global_search();
        }
    }

    fn open_goto(&mut self) {
        self.goto_open = true;
        self.goto_focus = true;
        self.goto_input.clear();
    }

    fn toggle_selected_bookmark(&mut self) {
        let Some(line) = self.selected else {
            return;
        };
        let marked = self.session.toggle_bookmark(line);
        self.status = if marked {
            format!("已加书签 · 第 {} 行", line.line as usize + 1)
        } else {
            "已移除书签".to_string()
        };
    }

    fn zoom_font(&mut self, delta: f32) {
        let next = (self.font_size + delta).clamp(FONT_MIN, FONT_MAX);
        if (next - self.font_size).abs() > 0.05 {
            self.font_size = next;
            self.status = format!("日志字号 {next:.1} · Ctrl+滚轮 缩放");
        }
    }

    fn reset_zoom(&mut self) {
        self.font_size = log_view::FONT_SIZE;
        self.status = format!("日志字号已重置为 {:.1}", log_view::FONT_SIZE);
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.open_paths(dropped);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let zoom = ctx.input(|input| input.zoom_delta());
        if (zoom - 1.0).abs() > 0.001 {
            let next = (self.font_size * zoom).clamp(FONT_MIN, FONT_MAX);
            if (next - self.font_size).abs() > 0.05 {
                self.font_size = next;
                self.status = format!("日志字号 {next:.1} · Ctrl+滚轮 缩放");
            }
        }

        let ctrl_shift = Modifiers {
            shift: true,
            ..Modifiers::CTRL
        };
        if ctx.input_mut(|input| input.consume_key(ctrl_shift, Key::F)) {
            self.open_global_search();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::F)) {
            self.open_find();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::G)) {
            self.open_goto();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F5)) {
            self.reload_all();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F9)) {
            self.toggle_sidebar();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F1)) {
            self.help_open = true;
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::S)) {
            self.save_session(false);
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::Q)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if (self.rename_open
            || self.find_open
            || self.goto_open
            || self.global_open
            || self.help_open
            || self.about_open)
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            if self.rename_open {
                self.rename_open = false;
            } else if self.goto_open {
                self.goto_open = false;
            } else if self.global_open {
                self.global_open = false;
            } else if self.help_open {
                self.help_open = false;
            } else if self.about_open {
                self.about_open = false;
            } else {
                self.find_open = false;
            }
            return;
        }
        if self.highlight_pop.is_some()
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.highlight_pop = None;
            return;
        }
        if self.filter_help
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.filter_help = false;
            return;
        }
        if self.selection.is_some()
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.selection = None;
            self.status = "已清除选择".to_string();
            return;
        }
        if self.sidebar_open
            && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.set_sidebar(false);
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::SHIFT, Key::F3)) {
            self.find_step(false);
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F3)) {
            self.find_step(true);
            return;
        }

        if ctx.wants_keyboard_input() {
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F2)) {
            self.rename_active();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::W)) {
            self.close_active_view();
            return;
        }
        if ctx.input_mut(|input| {
            input.consume_key(Modifiers::CTRL, Key::Equals)
                || input.consume_key(Modifiers::CTRL, Key::Plus)
        }) {
            self.zoom_font(1.0);
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::Minus)) {
            self.zoom_font(-1.0);
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::Num0)) {
            self.reset_zoom();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::B)) {
            self.toggle_selected_bookmark();
        }
        if ctx.input_mut(|input| input.consume_key(ctrl_shift, Key::O)) {
            self.pick_dir();
            return;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::O)) {
            self.open_requested = true;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::C)) {
            self.copy_to_clipboard(ctx);
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::A)) {
            self.select_all();
        }

        let page = (self.last_viewport_height * 0.9).max(60.0);
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Home)) {
            self.pending_scroll = Some(log_view::Scroll::Offset(0.0));
            self.status = "已跳到顶部".to_string();
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::End)) {
            self.pending_scroll = Some(log_view::Scroll::Offset(f32::MAX));
            self.status = "已跳到底部".to_string();
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::PageUp)) {
            let target = (self.last_scroll_offset - page).max(0.0);
            self.pending_scroll = Some(log_view::Scroll::Offset(target));
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::PageDown)) {
            self.pending_scroll = Some(log_view::Scroll::Offset(self.last_scroll_offset + page));
        }
    }

    fn ensure_find(&mut self) {
        let Some(view) = self.active.last().copied() else {
            self.find_cache = FindCache::default();
            return;
        };
        let key = (
            view,
            self.session.revision(),
            self.find_input.clone(),
            self.find_options,
        );
        if self.find_cache.key.as_ref() == Some(&key) {
            return;
        }
        self.find_cache = FindCache {
            key: Some(key),
            ..Default::default()
        };
        match SearchMatcher::build(&self.find_input, self.find_options) {
            Ok(None) => {}
            Err(error) => self.find_cache.error = Some(error),
            Ok(Some(matcher)) => {
                for row in 0..self.session.visible_len(view) {
                    if let Some(lr) = self.session.line_ref(view, row) {
                        if matcher.is_match(self.session.line_text(lr)) {
                            self.find_cache.matches.push(row);
                        }
                    }
                }
                self.find_cache.matcher = Some(matcher);
            }
        }
    }

    fn record_history(&mut self) {
        if self.find_input.is_empty() {
            return;
        }
        if self.find_history.first() == Some(&self.find_input) {
            return;
        }
        self.find_history.retain(|item| item != &self.find_input);
        self.find_history.insert(0, self.find_input.clone());
        self.find_history.truncate(12);
        self.save_user_state();
    }

    fn save_user_state(&mut self) {
        let state = config::UserState {
            find_history: self.find_history.clone(),
            presets: self.presets.clone(),
        };
        if let Err(error) = config::save_user(&state) {
            self.status = error;
        }
    }

    fn find_step(&mut self, forward: bool) {
        let Some(view) = self.active.last().copied() else {
            return;
        };
        self.ensure_find();
        if let Some(error) = &self.find_cache.error {
            self.status = format!("查找失败: {error}");
            return;
        }
        self.record_history();
        let total = self.find_cache.matches.len();
        if total == 0 {
            self.status = if self.find_input.is_empty() {
                "请输入要查找的内容（Ctrl+F）".to_string()
            } else {
                format!("未找到: {}", self.find_input)
            };
            return;
        }
        let cursor = match (forward, self.find_cache.cursor) {
            (true, Some(cursor)) => (cursor + 1) % total,
            (true, None) => 0,
            (false, Some(0)) | (false, None) => total - 1,
            (false, Some(cursor)) => cursor - 1,
        };
        self.find_cache.cursor = Some(cursor);
        let row = self.find_cache.matches[cursor];
        if let Some(line) = self.session.line_ref(view, row) {
            self.selected = Some(line);
            self.selection = None;
            self.pending_scroll = Some(log_view::Scroll::Row(row));
            self.status = format!(
                "查找 {}/{} · 第 {} 行",
                cursor + 1,
                total,
                line.line as usize + 1
            );
        }
    }

    fn run_global_search(&mut self) {
        self.global_cache = None;
        self.record_history();
        match SearchMatcher::build(&self.find_input, self.find_options) {
            Err(error) => self.global_error = Some(error),
            Ok(None) => self.global_error = Some("请输入搜索内容".to_string()),
            Ok(Some(matcher)) => {
                self.global_error = None;
                const LIMIT: usize = 5000;
                let mut hits = Vec::new();
                let mut truncated = false;
                'docs: for doc in self.session.docs() {
                    for line in 0..doc.len() {
                        if matcher.is_match(doc.line_text(line)) {
                            if hits.len() >= LIMIT {
                                truncated = true;
                                break 'docs;
                            }
                            hits.push(GlobalHit {
                                doc: doc.id,
                                line: line as u32,
                            });
                        }
                    }
                }
                let count = hits.len();
                self.global_cache = Some(GlobalCache {
                    query: self.find_input.clone(),
                    revision: self.session.revision(),
                    matcher,
                    hits,
                    truncated,
                });
                self.status = if truncated {
                    format!("全局搜索: 命中超过 {LIMIT} 条，已截断")
                } else {
                    format!("全局搜索: {count} 条命中")
                };
            }
        }
    }

    /// Switches to a view that shows `doc` and selects `line`. Prefers an
    /// unfiltered doc tab; falls back to scanning roots, then opens a new tab.
    fn activate_doc_line(&mut self, doc: DocId, line: u32) {
        let unfiltered = self.session.roots().iter().copied().find(|view| {
            self.session.view(*view).is_some_and(|node| {
                node.filters.is_all() && matches!(node.base, Base::Doc(id) if id == doc)
            })
        });
        let target = match unfiltered {
            Some(view) => Some((view, line as usize)),
            None => {
                let found = self.session.roots().iter().copied().find_map(|view| {
                    let len = self.session.visible_len(view);
                    (0..len)
                        .find(|row| {
                            self.session
                                .line_ref(view, *row)
                                .is_some_and(|lr| lr.doc == doc && lr.line == line)
                        })
                        .map(|row| (view, row))
                });
                found.or_else(|| {
                    self.session
                        .create_doc_view(doc)
                        .map(|view| (view, line as usize))
                })
            }
        };
        let Some((view, row)) = target else {
            return;
        };
        let path = self.session.view_path(view);
        self.set_active_path(path);
        if let Some(lr) = self.session.line_ref(view, row) {
            self.selected = Some(lr);
            self.selection = None;
            self.pending_scroll = Some(log_view::Scroll::Row(row));
        }
        if let Some(cache) = &mut self.global_cache {
            cache.revision = self.session.revision();
        }
        let name = self
            .session
            .doc(doc)
            .map(|doc| doc.short_name())
            .unwrap_or_default();
        self.status = format!("已跳到 {name} 第 {} 行", line as usize + 1);
    }

    fn goto_jump(&mut self) {
        let Some(view) = self.active.last().copied() else {
            return;
        };
        let total = self.session.visible_len(view);
        let Ok(value) = self.goto_input.trim().parse::<usize>() else {
            self.status = format!("请输入行号（1..={total}）");
            return;
        };
        if value == 0 || value > total {
            self.status = format!("行号超出范围（1..={total}）");
            return;
        }
        let row = value - 1;
        if let Some(line) = self.session.line_ref(view, row) {
            self.selected = Some(line);
            self.selection = None;
            self.pending_scroll = Some(log_view::Scroll::Row(row));
            self.goto_open = false;
            self.status = format!("已跳到第 {value} 行 · 共 {total} 行");
        }
    }

    fn find_bar(&mut self, ui: &mut Ui) {
        if !self.find_open {
            return;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("查找").color(theme::p().dim));
            let id = egui::Id::new("find-input");
            let resp = ui.add(
                TextEdit::singleline(&mut self.find_input)
                    .id(id)
                    .desired_width(160.0)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("在当前视图内查找（不改过滤）· Enter 下一个"),
            );
            if self.find_focus {
                ui.memory_mut(|memory| memory.request_focus(id));
                self.find_focus = false;
            }
            if resp.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                self.find_step(true);
                ui.memory_mut(|memory| memory.request_focus(id));
            }

            ui.toggle_value(&mut self.find_options.regex, ".*")
                .on_hover_text("正则表达式");
            ui.toggle_value(&mut self.find_options.case_sensitive, "Aa")
                .on_hover_text("区分大小写");
            ui.toggle_value(&mut self.find_options.whole_word, "全词")
                .on_hover_text("整词匹配（字母/数字/下划线视为词字符）");

            let mut picked = None;
            if !self.find_history.is_empty() {
                egui::ComboBox::from_id_salt("find-history")
                    .selected_text(RichText::new("历史").small())
                    .width(84.0)
                    .show_ui(ui, |ui| {
                        let items = self.find_history.clone();
                        for item in items {
                            if ui
                                .selectable_label(
                                    false,
                                    RichText::new(&item).monospace().size(12.0),
                                )
                                .clicked()
                            {
                                picked = Some(item);
                            }
                        }
                    });
            }
            if let Some(item) = picked {
                self.find_input = item;
                self.find_step(true);
            }

            self.ensure_find();
            if let Some(error) = &self.find_cache.error {
                ui.label(
                    RichText::new(error)
                        .small()
                        .color(egui::Color32::from_rgb(255, 120, 120)),
                );
            } else {
                let total = self.find_cache.matches.len();
                let current = self.find_cache.cursor.map_or(0, |cursor| cursor + 1);
                let counter = if self.find_input.is_empty() {
                    "未输入".to_string()
                } else if total == 0 {
                    "无匹配".to_string()
                } else {
                    format!("{current}/{total}")
                };
                ui.label(RichText::new(counter).small().color(theme::p().dim));
            }
            if ui.button("上一个").clicked() {
                self.find_step(false);
            }
            if ui.button("下一个").clicked() {
                self.find_step(true);
            }
            if ui
                .button("全局")
                .on_hover_text("在所有打开的文件中搜索（Ctrl+Shift+F）")
                .clicked()
            {
                self.global_open = true;
                self.global_focus = false;
                self.global_error = None;
                if !self.find_input.is_empty() {
                    self.run_global_search();
                }
            }
            if ui.button("关闭").clicked() {
                self.find_open = false;
            }
        });
    }

    fn goto_window(&mut self, ctx: &egui::Context) {
        if !self.goto_open {
            return;
        }
        let mut open = true;
        let mut submit = false;
        egui::Window::new("跳转到行")
            .anchor(Align2::CENTER_TOP, [0.0, 90.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                let id = egui::Id::new("goto-input");
                ui.horizontal(|ui| {
                    ui.label("行号");
                    let resp = ui.add(
                        TextEdit::singleline(&mut self.goto_input)
                            .id(id)
                            .desired_width(110.0),
                    );
                    if self.goto_focus {
                        ui.memory_mut(|memory| memory.request_focus(id));
                        self.goto_focus = false;
                    }
                    submit = resp.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
                    if ui.button("跳转").clicked() {
                        submit = true;
                    }
                });
                let total = self
                    .active
                    .last()
                    .map(|view| self.session.visible_len(*view))
                    .unwrap_or(0);
                ui.label(
                    RichText::new(format!("当前视图 1..={total} · Enter 确认"))
                        .small()
                        .color(theme::p().dim),
                );
            });
        self.goto_open = open;
        if submit {
            self.goto_jump();
        }
    }

    fn open_rename(&mut self, id: ViewId) {
        let Some(view) = self.session.view(id) else {
            return;
        };
        self.rename_target = Some(id);
        self.rename_title = view.title.clone();
        self.rename_note = view.note.clone();
        self.rename_focus = true;
        self.rename_open = true;
    }

    fn rename_window(&mut self, ctx: &egui::Context) {
        if !self.rename_open {
            return;
        }
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("重命名 / 备注")
            .anchor(Align2::CENTER_TOP, [0.0, 120.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(360.0);
                let title_id = egui::Id::new("rename-title");
                ui.label("名称");
                let title_resp = ui.add(
                    TextEdit::singleline(&mut self.rename_title)
                        .id(title_id)
                        .hint_text("留空恢复自动名称")
                        .desired_width(f32::INFINITY),
                );
                if self.rename_focus {
                    ui.memory_mut(|memory| memory.request_focus(title_id));
                    self.rename_focus = false;
                }
                ui.label(RichText::new("备注").small().color(theme::p().dim));
                ui.add(
                    TextEdit::multiline(&mut self.rename_note)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("例如：排查 08:12 的重启原因"),
                );
                ui.label(
                    RichText::new("备注显示在标签页悬停提示与视图栈 · Ctrl+Enter 保存")
                        .small()
                        .color(theme::p().dim),
                );
                ui.horizontal(|ui| {
                    if ui.button("保存").clicked() {
                        save = true;
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
                if title_resp.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                    save = true;
                }
                if ui.input(|input| input.modifiers.ctrl && input.key_pressed(Key::Enter)) {
                    save = true;
                }
            });
        if cancel {
            open = false;
        }
        self.rename_open = open;
        if save {
            if let Some(id) = self.rename_target {
                let title = self.rename_title.trim().to_string();
                let note = self.rename_note.trim().to_string();
                if self.session.set_view_meta(id, &title, &note) {
                    let shown = self
                        .session
                        .view(id)
                        .map(|view| view.title.clone())
                        .unwrap_or(title);
                    self.status = format!("已更新视图“{shown}”");
                }
            }
            self.rename_open = false;
        }
        if !self.rename_open {
            self.rename_target = None;
        }
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        if !self.help_open {
            return;
        }
        let mut open = self.help_open;
        let mut close = false;
        egui::Window::new("快捷键与提示")
            .anchor(Align2::CENTER_TOP, [0.0, 110.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(540.0);
                egui::ScrollArea::vertical()
                    .max_height(520.0)
                    .show(ui, |ui| {
                        shortcut_section(
                            ui,
                            "文件与工作区",
                            &[
                                ("Ctrl+O", "打开日志文件（也可直接拖入窗口）"),
                                ("Ctrl+Shift+O", "打开目录，并用通配符跟上新文件"),
                                ("F5", "从磁盘重新加载全部文件"),
                                ("F9", "打开 / 收起工作区侧栏（浮层，不占日志宽度）"),
                                ("Ctrl+S", "保存当前工作区会话"),
                                ("Ctrl+Q", "退出 neo-log（会先自动保存）"),
                            ],
                        );
                        shortcut_section(
                            ui,
                            "查找与导航",
                            &[
                                ("Ctrl+F", "在当前视图中查找"),
                                ("F3 / Shift+F3", "下一个 / 上一个命中"),
                                ("Ctrl+Shift+F", "在所有文件中搜索"),
                                ("Ctrl+G", "按当前视图行号跳转"),
                                ("Home / End", "跳到顶部 / 底部"),
                                ("PgUp / PgDn", "向上 / 向下翻页"),
                            ],
                        );
                        shortcut_section(
                            ui,
                            "过滤与视图",
                            &[
                                ("Enter", "过滤栏内回车：在当前文件下新开一个子标签"),
                                ("子标签", "过滤结果挂在父标签下面，点「全部」切回父视图，× 才关闭子标签"),
                                ("Ctrl+B", "为选中行切换书签"),
                                ("Ctrl+W", "关闭当前标签页（含其派生视图）"),
                                ("F2", "重命名当前视图 / 写备注"),
                                ("Ctrl+C", "复制选区；无选区时复制当前行"),
                                ("Ctrl+A", "选择当前视图的全部日志行"),
                                ("Esc", "先关对话框与查找浮层，再收起侧栏；无浮层时清除选区"),
                            ],
                        );
                        shortcut_section(
                            ui,
                            "显示与缩放",
                            &[
                                ("Ctrl+滚轮", "放大 / 缩小日志字号（9–28 px）"),
                                ("Ctrl++ / Ctrl+-", "增大 / 减小字号"),
                                ("Ctrl+0", "恢复默认字号"),
                                ("☰ 菜单", "打开、查找在标签条上；其余命令在 ☰ 里"),
                                ("显示菜单", "过滤栏、跟随、阅读方式（原文/换行/折叠/分列）、统计窗口"),
                                ("主题菜单", "深色 / 浅色 / 高对比，立即生效并随界面配置保存"),
                            ],
                        );
                        shortcut_section(
                            ui,
                            "鼠标与右键",
                            &[
                                ("双击单词", "立即添加文字高亮"),
                                ("拖拽 / Shift+点击", "跨行选择文本 / 扩选"),
                                ("三击或点行号", "选中整行；拖过行号栏选择连续多行"),
                                ("右键日志行", "复制、书签、高亮、筛选、按 trace/request id 串时间线"),
                                ("右键标签页", "重命名、放到右侧对照或关闭；关闭后回到最近用过的标签"),
                                ("点击直方图", "按时间桶筛选或跳转"),
                            ],
                        );
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(
                                "提示：日志区保持全宽。侧栏和查找都浮在日志上；过滤结果是父标签下的子标签，点「全部」切回父视图；阅读方式在同一行；过滤栏可从显示菜单收起。级别计数在状态栏右侧，点击可筛选。",
                            )
                            .size(10.5)
                            .color(theme::p().faint),
                        );
                    });
                ui.add_space(4.0);
                if ui.button("关闭").clicked() {
                    close = true;
                }
            });
        if close {
            open = false;
        }
        self.help_open = open;
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let mut open = self.about_open;
        let session_path = config::session_path();
        let plugins_dir = config::plugins_dir();
        egui::Window::new("关于 neo-log")
            .anchor(Align2::CENTER_TOP, [0.0, 140.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(430.0);
                ui.label(RichText::new("neo-log").size(20.0).strong());
                ui.label(
                    RichText::new(format!(
                        "v{} · Notepad++ 风格的 Rust 日志分析工作台",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .size(11.0)
                    .color(theme::p().dim),
                );
                ui.add_space(6.0);
                ui.label("多文件按时间合并 · 多级过滤视图 · 可插拔日志插件 · 大文件虚拟化渲染");
                ui.separator();
                ui.label(RichText::new("会话文件").size(10.5).color(theme::p().faint));
                ui.monospace(session_path.display().to_string());
                ui.label(
                    RichText::new("动态插件目录")
                        .size(10.5)
                        .color(theme::p().faint),
                );
                ui.monospace(plugins_dir.display().to_string());
                ui.add_space(4.0);
                ui.label(
                    RichText::new("关闭窗口时会自动保存工作区，下次启动自动恢复。")
                        .size(10.5)
                        .color(theme::p().faint),
                );
            });
        self.about_open = open;
    }

    fn global_window(&mut self, ctx: &egui::Context) {
        if !self.global_open {
            return;
        }
        let mut open = true;
        let mut search = false;
        let mut jump = None;
        egui::Window::new("全局搜索")
            .open(&mut open)
            .default_pos([240.0, 110.0])
            .default_size([760.0, 460.0])
            .resizable(true)
            .show(ctx, |ui| {
                let id = egui::Id::new("global-input");
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        TextEdit::singleline(&mut self.find_input)
                            .id(id)
                            .desired_width(320.0)
                            .font(egui::TextStyle::Monospace)
                            .hint_text("在所有打开的文件中搜索"),
                    );
                    if self.global_focus {
                        ui.memory_mut(|memory| memory.request_focus(id));
                        self.global_focus = false;
                    }
                    if resp.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                        search = true;
                        ui.memory_mut(|memory| memory.request_focus(id));
                    }
                    if ui.button("搜索").clicked() {
                        search = true;
                    }
                    ui.toggle_value(&mut self.find_options.regex, ".*")
                        .on_hover_text("正则表达式");
                    ui.toggle_value(&mut self.find_options.case_sensitive, "Aa")
                        .on_hover_text("区分大小写");
                    ui.toggle_value(&mut self.find_options.whole_word, "全词")
                        .on_hover_text("整词匹配");
                });
                if let Some(error) = &self.global_error {
                    ui.label(
                        RichText::new(error)
                            .small()
                            .color(egui::Color32::from_rgb(255, 120, 120)),
                    );
                }
                let Some(cache) = &self.global_cache else {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("输入关键字后按 Enter 搜索所有文件")
                            .small()
                            .color(theme::p().dim),
                    );
                    return;
                };
                let stale = cache.revision != self.session.revision();
                let mut header = format!("“{}” · {} 条命中", cache.query, cache.hits.len());
                if cache.truncated {
                    header.push_str(" · 已截断");
                }
                if stale {
                    header.push_str(" · 结果基于旧版本，Enter 重新搜索");
                }
                ui.label(RichText::new(header).small().color(theme::p().dim));
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_rows(ui, 18.0, cache.hits.len(), |ui, range| {
                        for idx in range {
                            let hit = cache.hits[idx];
                            let Some(doc) = self.session.doc(hit.doc) else {
                                continue;
                            };
                            let text = doc.line_text(hit.line as usize);
                            let snippet = hit_snippet(text, &cache.matcher);
                            let time = doc
                                .time_at(hit.line as usize)
                                .map(format_ts)
                                .unwrap_or_default();
                            let label = if time.is_empty() {
                                format!("{}:{}  {snippet}", doc.short_name(), hit.line as usize + 1)
                            } else {
                                format!(
                                    "{}:{}  {time}  {snippet}",
                                    doc.short_name(),
                                    hit.line as usize + 1
                                )
                            };
                            if ui
                                .selectable_label(
                                    false,
                                    RichText::new(label).monospace().size(12.0),
                                )
                                .clicked()
                            {
                                jump = Some(hit);
                            }
                        }
                    });
            });
        self.global_open = open;
        if let Some(hit) = jump {
            self.activate_doc_line(hit.doc, hit.line);
            self.global_open = false;
        }
        if search {
            self.run_global_search();
        }
    }

    fn fix_active(&mut self) {
        self.prune_leaves();
        if let Some(leaf) = self
            .active
            .last()
            .copied()
            .filter(|view| self.session.view(*view).is_some())
        {
            let path = self.session.view_path(leaf);
            if path != self.active {
                self.active = path;
                self.synced_view = None;
            }
            self.record_leaf();
            return;
        }
        match self.session.roots().last().copied() {
            Some(root) => {
                let leaf = resolve_leaf(&self.session, &self.leaves, root);
                self.active = self.session.view_path(leaf);
                self.synced_view = None;
                self.record_leaf();
            }
            None => {
                if !self.active.is_empty() {
                    self.active.clear();
                    self.synced_view = None;
                }
            }
        }
    }

    fn prune_leaves(&mut self) {
        let stale: Vec<ViewId> = self
            .leaves
            .iter()
            .filter(|(root, leaf)| {
                self.session.view(**root).is_none() || !under_root(&self.session, **leaf, **root)
            })
            .map(|(root, _)| *root)
            .collect();
        for root in stale {
            self.leaves.remove(&root);
        }
    }

    fn record_leaf(&mut self) {
        if let (Some(root), Some(leaf)) =
            (self.active.first().copied(), self.active.last().copied())
        {
            self.leaves.insert(root, leaf);
        }
    }

    /// Switches to a tab: a root view plus the derived level it was left on.
    fn activate_root(&mut self, root: ViewId) {
        if self.session.view(root).is_none() {
            return;
        }
        let leaf = resolve_leaf(&self.session, &self.leaves, root);
        let path = self.session.view_path(leaf);
        self.set_active_path(path);
        self.touch_mru(root);
    }

    fn touch_mru(&mut self, root: ViewId) {
        self.tab_mru
            .retain(|id| *id != root && self.session.view(*id).is_some());
        self.tab_mru.insert(0, root);
    }

    /// Activates the existing tab for a document instead of stacking duplicates.
    fn activate_doc(&mut self, doc: DocId) {
        if let Some(root) = doc_root(&self.session, doc) {
            self.activate_root(root);
        } else if let Some(root) = self.session.create_doc_view(doc) {
            self.activate_root(root);
        }
    }

    fn close_tab(&mut self, root: ViewId) {
        let was_active = self.active.first().copied() == Some(root);
        self.session.remove_view(root);
        self.leaves.remove(&root);
        if self.side_root == Some(root) {
            self.side_root = None;
            self.side_scroll = None;
        }
        self.tab_mru
            .retain(|id| *id != root && self.session.view(*id).is_some());
        if !was_active {
            return;
        }
        if let Some(next) = self.tab_mru.first().copied() {
            self.activate_root(next);
            return;
        }
        match self.session.roots().last().copied() {
            Some(next) => self.activate_root(next),
            None => {
                self.active.clear();
                self.synced_view = None;
                self.filter_input.clear();
                self.selected = None;
                self.selection = None;
            }
        }
    }

    fn sync_filter_input(&mut self) {
        let active = self.active.last().copied();
        if self.synced_view != active {
            self.filter_input = active
                .and_then(|view| self.session.view(view))
                .map(|view| view.query.clone())
                .unwrap_or_default();
            self.synced_view = active;
        }
    }

    fn set_active_path(&mut self, path: Vec<ViewId>) {
        self.active = path;
        self.record_leaf();
        self.selected = None;
        self.selection = None;
        self.synced_view = None;
        self.sync_filter_input();
    }

    fn discard_demo_workspace(&mut self) {
        if self.session.docs().is_empty()
            || self.session.docs().iter().any(|doc| doc.path.is_file())
        {
            return;
        }
        self.session = Session::new(self.session.plugins().clone());
        self.active.clear();
        self.leaves.clear();
        self.tab_mru.clear();
        self.side_root = None;
        self.side_scroll = None;
        self.filter_error = None;
        self.highlight_pop = None;
        self.synced_view = None;
        self.filter_input.clear();
        self.selected = None;
        self.selection = None;
        self.merge_selected.clear();
        self.fold_cache = log_view::FoldCache::default();
        self.columns_cache = log_view::ColumnsCache::default();
        self.row_cache = log_view::RowCache::default();
        self.stats_cache = stats::StatsCache::default();
        self.find_cache = FindCache::default();
        self.global_cache = None;
        self.global_error = None;
    }

    fn open_paths(&mut self, paths: Vec<PathBuf>) {
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        for path in paths {
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
        for dir in dirs {
            let glob = self.dir_glob.clone();
            self.open_directory(dir, &glob);
        }
        if files.is_empty() {
            return;
        }
        self.discard_demo_workspace();
        let mut opened = 0;
        let mut last_view = None;
        for path in files {
            match self.session.open_path(&path) {
                Ok(doc) => {
                    if let Some(view) = self.session.create_doc_view(doc) {
                        last_view = Some(view);
                    }
                    opened += 1;
                }
                Err(error) => {
                    self.status = format!("打开 {} 失败: {error}", path.display());
                }
            }
        }
        self.merge_selected.resize(self.session.docs().len(), false);
        if let Some(view) = last_view {
            self.activate_root(view);
        }
        self.arm_watch();
        if opened > 0 {
            self.status = format!(
                "已打开 {opened} 个文件 · 共 {} 行",
                self.session.total_lines()
            );
        }
    }

    fn pick_files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("日志文件", &["log", "txt", "out", "err", "jsonl"])
            .add_filter("所有文件", &["*"])
            .pick_files()
        {
            self.open_paths(paths);
        }
    }

    fn load_demo(&mut self) {
        if let Some(view) = demo::load(&mut self.session) {
            self.merge_selected.resize(self.session.docs().len(), false);
            self.activate_root(view);
            self.status = format!("已加载示例日志 · {} 行", self.session.total_lines());
        }
    }

    fn merge_selected_docs(&mut self) {
        let docs: Vec<DocId> = self
            .session
            .docs()
            .iter()
            .enumerate()
            .filter(|(idx, _)| self.merge_selected.get(*idx).copied().unwrap_or(false))
            .map(|(_, doc)| doc.id)
            .collect();
        if docs.is_empty() {
            self.status = "请先在左侧勾选要合并的日志".to_string();
            return;
        }
        match self.session.create_merged_view(&docs, "") {
            Ok(view) => {
                let count = docs.len();
                self.activate_root(view);
                self.status = format!("已按时间合并 {count} 个文件");
            }
            Err(error) => self.status = error,
        }
    }

    fn close_active_view(&mut self) {
        let Some(root) = self.active.first().copied() else {
            return;
        };
        let title = self
            .session
            .view(root)
            .map(|view| view.title.clone())
            .unwrap_or_default();
        self.close_tab(root);
        self.status = format!("已关闭标签页 {title}");
    }

    fn rename_active(&mut self) {
        if let Some(id) = self.active.last().copied() {
            self.open_rename(id);
        }
    }

    fn remove_active_level(&mut self) {
        let Some(id) = self.active.last().copied() else {
            return;
        };
        if self.session.view(id).and_then(|view| view.parent).is_none() {
            self.status = "根视图没有可关闭的子标签".to_string();
            return;
        }
        self.close_child_tab(id);
    }

    fn close_child_tab(&mut self, id: ViewId) {
        let Some(node) = self.session.view(id) else {
            return;
        };
        let Some(parent) = node.parent else {
            self.status = "根标签用顶部的 × 关闭".to_string();
            return;
        };
        let title = node.title.clone();
        let showing = self.active.iter().any(|view| *view == id);
        self.session.remove_view(id);
        if showing {
            self.set_active_path(self.session.view_path(parent));
        } else {
            self.prune_leaves();
        }
        self.status = format!("已关闭子标签 {title}");
    }

    fn clear_rules(&mut self) {
        let ids: Vec<u64> = self.session.rules().iter().map(|rule| rule.id).collect();
        for id in &ids {
            self.session.remove_rule(*id);
        }
        self.status = format!("已清除 {} 条高亮规则", ids.len());
    }

    fn reading(&self) -> Reading {
        if self.wrap {
            Reading::Wrap
        } else if self.fold {
            Reading::Fold
        } else if self.columns {
            Reading::Columns
        } else {
            Reading::Plain
        }
    }

    fn set_reading(&mut self, mode: Reading) {
        if self.reading() == mode {
            return;
        }
        self.wrap = mode == Reading::Wrap;
        self.fold = mode == Reading::Fold;
        self.columns = mode == Reading::Columns;
        if matches!(mode, Reading::Fold | Reading::Columns) {
            self.selection = None;
        }
        self.status = match mode {
            Reading::Plain => "阅读方式：原文".to_string(),
            Reading::Wrap => "阅读方式：换行".to_string(),
            Reading::Fold => "阅读方式：折叠相邻重复行".to_string(),
            Reading::Columns => "阅读方式：分列".to_string(),
        };
    }

    fn focus_filter(&mut self) {
        self.filter_bar_open = true;
        self.filter_focus = true;
        self.filter_help = false;
        self.save_ui_prefs();
    }

    fn set_follow(&mut self, on: bool) {
        self.follow = on;
        if on {
            let mut released = 0usize;
            let ids: Vec<DocId> = self.session.doc_ids().collect();
            for doc_id in ids {
                let can = self
                    .session
                    .doc(doc_id)
                    .is_some_and(|doc| !doc.missing && doc.path.is_file() && !doc.unmapped);
                if can && self.session.unpin_doc(doc_id).is_ok() {
                    released += 1;
                }
            }
            self.arm_watch();
            self.ensure_watcher();
            self.status = if released > 0 {
                format!("已开启跟随：放开 {released} 个文件的映射，目录有变动会立刻刷新")
            } else {
                "已开启跟随：文件追加或等长改写时自动刷新并滚到底部".to_string()
            };
        } else {
            self.status = "已关闭跟随".to_string();
        }
    }

    fn open_correlation(&mut self, key: &str, value: &str) {
        let docs: Vec<DocId> = self
            .session
            .docs()
            .iter()
            .filter(|doc| !doc.missing)
            .map(|doc| doc.id)
            .collect();
        if docs.is_empty() {
            self.status = "没有可串联的日志文件".to_string();
            return;
        }
        let title = format!("{key}={}", truncate_chars(value, 18));
        let query = id_query(value);
        let view = match self.session.create_merged_view(&docs, &title) {
            Ok(view) => view,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        if let Err(error) = self.session.update_view(view, &query) {
            self.status = error;
            return;
        }
        let _ = self.session.set_view_meta(
            view,
            &title,
            &format!("跨 {} 个文件的同一 {key}", docs.len()),
        );
        self.activate_root(view);
        let rows = self.session.visible_len(view);
        self.status = format!("已按 {key} 串起 {rows} 行");
    }

    fn export_active(&mut self) {
        let Some(view) = self.active.last().copied() else {
            self.status = "没有可导出的视图".to_string();
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .set_file_name("neo-log.txt")
            .add_filter("文本", &["txt", "log"])
            .save_file()
        else {
            return;
        };
        let mut out = String::new();
        if let Some(node) = self.session.view(view) {
            out.push_str(&format!("# view: {}\n", node.title));
            if !node.query.is_empty() {
                out.push_str(&format!("# query: {}\n", node.query));
            }
        }
        let rows = self.session.visible_len(view);
        for row in 0..rows {
            let Some(lr) = self.session.line_ref(view, row) else {
                continue;
            };
            out.push_str(self.session.line_text(lr));
            out.push('\n');
        }
        out.push_str("\n# bookmarks\n");
        for row in 0..rows {
            let Some(lr) = self.session.line_ref(view, row) else {
                continue;
            };
            if !self.session.is_bookmarked(lr) {
                continue;
            }
            let name = self
                .session
                .doc(lr.doc)
                .map(|doc| doc.short_name())
                .unwrap_or_default();
            out.push_str(&format!(
                "# {name}:{} {}\n",
                lr.line as usize + 1,
                self.session.line_text(lr)
            ));
        }
        out.push_str("# rules\n");
        for rule in self.session.rules() {
            let scope = match &rule.scope {
                RuleScope::Global => "全局".to_string(),
                RuleScope::Doc(doc) => self
                    .session
                    .doc(*doc)
                    .map(|doc| doc.short_name())
                    .unwrap_or_else(|| format!("文档 {doc}")),
                RuleScope::Plugin(plugin) => format!("插件 {plugin}"),
            };
            let matcher = match &rule.matcher {
                neolog_core::style::Matcher::Plain(needle) => needle.raw().to_string(),
                neolog_core::style::Matcher::Regex(regex) => format!("/{}/", regex.as_str()),
            };
            out.push_str(&format!("# {} · {} · {}\n", rule.label, scope, matcher));
        }
        match std::fs::write(&path, out) {
            Ok(()) => self.status = format!("已导出 {rows} 行到 {}", path.display()),
            Err(error) => self.status = format!("导出失败: {error}"),
        }
    }

    fn save_preset(&mut self) {
        let query = self.filter_input.trim().to_string();
        if query.is_empty() {
            self.status = "先写一条过滤条件，再存成预设".to_string();
            return;
        }
        let name = {
            let typed = self.preset_name.trim();
            if typed.is_empty() {
                truncate_chars(&query, 24)
            } else {
                typed.to_string()
            }
        };
        self.presets.retain(|preset| preset.name != name);
        self.presets.insert(
            0,
            config::Preset {
                name,
                query: query.clone(),
            },
        );
        self.presets.truncate(40);
        self.preset_name.clear();
        self.save_user_state();
        self.status = format!("已保存查询预设: {query}");
    }

    fn apply_preset(&mut self, query: String) {
        self.filter_input = query;
        self.preset_open = false;
        if self.active.last().is_some() {
            self.apply_filter();
        }
    }

    fn place_side(&mut self, root: ViewId) {
        if self.session.view(root).is_none() {
            return;
        }
        self.side_root = Some(root);
        self.side_selected = None;
        self.side_selection = None;
        self.side_scroll = None;
        self.side_offset = 0.0;
        let title = self
            .session
            .view(root)
            .map(|view| view.title.clone())
            .unwrap_or_default();
        self.status = format!("右侧对照: {title}");
    }

    fn pick_dir(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.dir_pop = Some(path);
        }
    }

    fn open_directory(&mut self, dir: PathBuf, glob: &str) {
        let patterns = split_globs(glob);
        let patterns = if patterns.is_empty() {
            split_globs("*.log,*.txt,*.out,*.err,*.jsonl,*.log.*")
        } else {
            patterns
        };
        let mut files = Vec::new();
        walk_logs(&dir, &patterns, 4, &mut files);
        files.sort();
        files.dedup();
        if files.is_empty() {
            self.status = format!("目录里没有匹配 {glob} 的日志: {}", dir.display());
            self.watch_directory(dir, patterns, None);
            return;
        }
        self.discard_demo_workspace();
        let mut docs = Vec::new();
        for path in files {
            if let Some(doc) = self.open_one(&path, false) {
                docs.push(doc);
            }
        }
        let merge = if docs.len() > 1 {
            let title = dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "目录".to_string());
            self.session.create_merged_view(&docs, title).ok()
        } else {
            docs.first()
                .and_then(|doc| self.session.create_doc_view(*doc))
        };
        if let Some(view) = merge {
            self.activate_root(view);
        }
        self.watch_directory(dir, patterns, merge.filter(|_| docs.len() > 1));
        self.merge_selected.resize(self.session.docs().len(), false);
        self.arm_watch();
        self.status = format!(
            "已打开目录 · {} 个文件 · 共 {} 行",
            docs.len(),
            self.session.total_lines()
        );
    }

    fn open_one(&mut self, path: &Path, activate: bool) -> Option<DocId> {
        if self.session.docs().iter().any(|doc| doc.path == path) {
            return self
                .session
                .docs()
                .iter()
                .find(|doc| doc.path == path)
                .map(|doc| doc.id);
        }
        match self.session.open_path(path) {
            Ok(doc) => {
                if activate {
                    if let Some(view) = self.session.create_doc_view(doc) {
                        self.activate_root(view);
                    }
                }
                Some(doc)
            }
            Err(error) => {
                self.status = format!("打开 {} 失败: {error}", path.display());
                None
            }
        }
    }

    fn watch_directory(&mut self, path: PathBuf, patterns: Vec<String>, merge: Option<ViewId>) {
        if let Some(existing) = self.watched.iter_mut().find(|root| root.path == path) {
            existing.patterns = patterns;
            if merge.is_some() {
                existing.merge = merge;
            }
        } else {
            self.watched.push(WatchedRoot {
                path: path.clone(),
                patterns,
                merge,
            });
        }
        self.ensure_watcher();
        if let Some(watcher) = &mut self.watcher {
            watcher.watch_dir(&path);
        }
    }

    fn ensure_watcher(&mut self) {
        if self.watcher.is_none() {
            match DirWatcher::open() {
                Ok(watcher) => self.watcher = Some(watcher),
                Err(error) => {
                    self.status = format!("文件系统通知不可用，仍会轮询: {error}");
                    return;
                }
            }
        }
        let parents: Vec<PathBuf> = self
            .session
            .docs()
            .iter()
            .filter_map(|doc| doc.path.parent().map(|path| path.to_path_buf()))
            .collect();
        let roots: Vec<PathBuf> = self.watched.iter().map(|root| root.path.clone()).collect();
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        for path in parents.iter().chain(roots.iter()) {
            watcher.watch_dir(path);
        }
    }

    fn ingest_new_files(&mut self) {
        let mut found = Vec::new();
        for root in &self.watched {
            walk_logs(&root.path, &root.patterns, 4, &mut found);
        }
        let open_names: Vec<(PathBuf, String)> = self
            .session
            .docs()
            .iter()
            .filter(|doc| doc.path.is_file())
            .map(|doc| {
                let stem = doc
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (
                    doc.path.parent().unwrap_or(Path::new("")).to_path_buf(),
                    stem,
                )
            })
            .collect();
        for (parent, stem) in &open_names {
            let Ok(entries) = std::fs::read_dir(parent) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if name.starts_with(stem) && path.is_file() {
                    found.push(path);
                }
            }
        }
        found.sort();
        found.dedup();
        let mut added = 0usize;
        let mut opened = Vec::new();
        for path in found {
            if self.session.docs().iter().any(|doc| doc.path == path) {
                continue;
            }
            if let Some(doc) = self.open_one(&path, false) {
                opened.push((path, doc));
                added += 1;
            }
        }
        for (path, doc) in opened {
            let parent = path.parent().map(|path| path.to_path_buf());
            let merges: Vec<ViewId> = self
                .watched
                .iter()
                .filter(|root| {
                    parent
                        .as_ref()
                        .is_some_and(|dir| dir.starts_with(&root.path))
                })
                .filter_map(|root| root.merge)
                .collect();
            for view in merges {
                let _ = self.session.append_merged_doc(view, doc);
            }
        }
        if added > 0 {
            self.merge_selected.resize(self.session.docs().len(), false);
            self.arm_watch();
            self.ensure_watcher();
            self.status = format!("目录里新增 {added} 个日志，已并入当前时间线");
        }
    }

    fn set_stats(&mut self, on: bool) {
        self.stats_open = on;
        self.status = if on {
            "已打开统计窗口 · 可拖动/缩放，不占用日志宽度".to_string()
        } else {
            "已关闭统计窗口".to_string()
        };
        self.save_ui_prefs();
    }

    fn set_filter_bar(&mut self, on: bool) {
        self.filter_bar_open = on;
        self.status = if on {
            "已显示过滤栏".to_string()
        } else {
            "已隐藏过滤栏 · 过滤条件保持不变，可从“显示”菜单恢复".to_string()
        };
        self.save_ui_prefs();
    }

    fn active_scope(&self) -> RuleScope {
        match self.active.last().map(|id| self.session.view_docs(*id)) {
            Some(docs) if docs.len() == 1 => RuleScope::Doc(docs[0]),
            _ => RuleScope::Global,
        }
    }

    fn add_keyword_rule(&mut self, text_only: bool) {
        let word = self.keyword_input.trim().to_string();
        if word.is_empty() {
            self.status = "请先输入要高亮的关键词".to_string();
            return;
        }
        let color = self.highlight_rgb();
        let scope = self.active_scope();
        let (style, row_bg) = if text_only {
            (
                TextStyle {
                    fg: Some(color),
                    bold: true,
                    ..Default::default()
                },
                None,
            )
        } else {
            (TextStyle::default(), Some(color))
        };
        let id = self.session.add_rule(HighlightRule::plain(
            word.clone(),
            scope,
            word.clone(),
            false,
            style,
            row_bg,
        ));
        self.status = format!("高亮规则 #{id}: {word}");
    }

    fn add_word_rule(&mut self, line: LineRef, word: &str) {
        self.add_row_word_rule(line, word, true);
    }

    fn add_row_word_rule(&mut self, line: LineRef, word: &str, row_bg: bool) {
        let color = self.highlight_rgb();
        let (style, background) = if row_bg {
            (TextStyle::default(), Some(color))
        } else {
            (
                TextStyle {
                    fg: Some(color),
                    bold: true,
                    ..Default::default()
                },
                None,
            )
        };
        let id = self.session.add_rule(HighlightRule::plain(
            word.to_string(),
            RuleScope::Doc(line.doc),
            word.to_string(),
            false,
            style,
            background,
        ));
        self.status = if row_bg {
            format!("已高亮 \"{word}\" 所在行 (#{id})")
        } else {
            format!("已为 \"{word}\" 添加文字高亮 (#{id})")
        };
    }

    fn highlight_rgb(&self) -> Rgb {
        Rgb(
            self.highlight_color[0],
            self.highlight_color[1],
            self.highlight_color[2],
        )
    }

    fn fork_active(&mut self) {
        let Some(parent) = self.active.last().copied() else {
            self.status = "没有活动标签页，先打开日志".to_string();
            return;
        };
        let query = self.filter_input.clone();
        match self.session.fork_view(parent, &query) {
            Ok(child) => {
                let path = self.session.view_path(child);
                self.filter_error = None;
                self.set_active_path(path);
                self.status = if query.trim().is_empty() {
                    "已派生一个未过滤的子视图".to_string()
                } else {
                    format!("已派生子视图: {query}")
                };
            }
            Err(error) => {
                self.filter_error = Some(error.clone());
                self.status = format!("过滤失败: {error}");
            }
        }
    }

    fn apply_filter(&mut self) {
        let Some(view) = self.active.last().copied() else {
            return;
        };
        match self.session.update_view(view, &self.filter_input) {
            Ok(()) => {
                self.selection = None;
                self.filter_error = None;
                self.status = format!("已替换当前过滤: {}", self.filter_input);
            }
            Err(error) => {
                self.filter_error = Some(error.clone());
                self.status = format!("过滤失败: {error}");
            }
        }
    }

    fn fork_query(&mut self, query: String) {
        self.filter_input = query;
        self.fork_active();
    }

    fn fork_from(&mut self, parent: ViewId, query: String, side: bool) {
        self.filter_input = query.clone();
        match self.session.fork_view(parent, &query) {
            Ok(child) => {
                let path = self.session.view_path(child);
                self.filter_error = None;
                if side {
                    if let Some(root) = path.first().copied() {
                        self.leaves.insert(root, child);
                        self.side_root = Some(root);
                    }
                    self.status = format!("右侧已过滤: {query}");
                } else {
                    self.set_active_path(path);
                    self.status = if query.trim().is_empty() {
                        "已派生一个未过滤的子视图".to_string()
                    } else {
                        format!("已派生子视图: {query}")
                    };
                }
            }
            Err(error) => {
                self.filter_error = Some(error.clone());
                self.status = format!("过滤失败: {error}");
            }
        }
    }

    fn fork_bookmarks(&mut self) {
        self.fork_query("bm:".to_string());
    }

    fn menu_button(&mut self, ui: &mut Ui) {
        let response = ui.menu_button(RichText::new("☰").size(14.0), |ui| {
            ui.horizontal(|ui| {
                theme::brand(ui);
                ui.label(
                    RichText::new("neo-log · LOG WORKBENCH")
                        .size(9.5)
                        .color(theme::p().faint),
                );
            });
            ui.separator();
            ui.menu_button("文件", |ui| self.file_menu(ui));
            ui.menu_button("编辑", |ui| self.edit_menu(ui));
            ui.menu_button("显示", |ui| self.display_menu(ui));
            ui.menu_button("主题", |ui| self.theme_menu(ui));
            ui.menu_button("视图", |ui| self.tree_menu(ui));
            ui.menu_button("高亮", |ui| self.highlight_menu(ui));
            ui.menu_button("帮助", |ui| self.help_menu(ui));
        });
        response
            .response
            .on_hover_text("菜单：文件 / 编辑 / 显示 / 主题 / 视图 / 高亮 / 帮助");
    }

    fn theme_menu(&mut self, ui: &mut Ui) {
        let current = theme::current();
        for choice in theme::Theme::ALL {
            if ui.radio(current == choice, choice.label()).clicked() {
                self.apply_theme(ui.ctx(), choice);
                ui.close_menu();
            }
        }
        ui.separator();
        ui.label(
            RichText::new("主题随界面配置保存，重启后沿用")
                .size(10.5)
                .color(theme::p().faint),
        );
    }

    fn apply_theme(&mut self, ctx: &egui::Context, choice: theme::Theme) {
        theme::set_theme(choice);
        theme::apply_style(ctx);
        // RowPaint caches galleys and base colors from the previous palette.
        self.row_cache = log_view::RowCache::default();
        self.status = format!("主题已切换为{}", choice.label());
        self.save_ui_prefs();
    }

    fn ui_prefs(&self) -> config::UiPrefs {
        config::UiPrefs {
            theme: Some(theme::current().id().to_string()),
            sidebar_open: Some(self.sidebar_open),
            stats_open: Some(self.stats_open),
            filter_bar_open: Some(self.filter_bar_open),
        }
    }

    fn save_ui_prefs(&mut self) {
        if let Err(error) = config::save_ui(&self.ui_prefs()) {
            self.status = error;
        }
    }

    fn set_sidebar(&mut self, on: bool) {
        self.sidebar_open = on;
        self.sidebar_guard = on;
        self.status = if on {
            "已打开工作区侧栏 · F9 或点击日志区收起".to_string()
        } else {
            "已收起工作区侧栏 · 宽度让给日志区".to_string()
        };
        self.save_ui_prefs();
    }

    fn toggle_sidebar(&mut self) {
        self.set_sidebar(!self.sidebar_open);
    }

    fn file_menu(&mut self, ui: &mut Ui) {
        if menu_item(ui, "打开日志…", Some("Ctrl+O"), true) {
            self.open_requested = true;
        }
        if menu_item(ui, "打开目录…", Some("Ctrl+Shift+O"), true) {
            self.pick_dir();
            ui.close_menu();
        }
        if menu_item(
            ui,
            "重新加载全部",
            Some("F5"),
            !self.session.docs().is_empty(),
        ) {
            self.reload_all();
        }
        let merge_count = self.merge_selected.iter().filter(|flag| **flag).count();
        let merge_label = format!("按时间合并已勾选文件 ({merge_count})");
        if menu_item(ui, &merge_label, None, merge_count > 0) {
            self.merge_selected_docs();
        }
        if menu_item(ui, "导出当前视图…", None, self.active.last().is_some()) {
            self.export_active();
            ui.close_menu();
        }
        ui.separator();
        if menu_item(ui, "保存会话", Some("Ctrl+S"), true) {
            self.save_session(false);
        }
        if menu_item(ui, "加载示例日志", None, true) {
            self.load_demo();
        }
        ui.separator();
        if menu_item(ui, "退出", Some("Ctrl+Q"), true) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn edit_menu(&mut self, ui: &mut Ui) {
        let has_selection = self
            .selection
            .is_some_and(|selection| !selection.is_empty())
            || self.selected.is_some();
        if menu_item(ui, "复制", Some("Ctrl+C"), has_selection) {
            self.copy_to_clipboard(ui.ctx());
        }
        if menu_item(ui, "全选", Some("Ctrl+A"), self.active.last().is_some()) {
            self.select_all();
        }
        if menu_item(ui, "清除选择", Some("Esc"), self.selection.is_some()) {
            self.selection = None;
        }
        ui.separator();
        if menu_item(ui, "查找…", Some("Ctrl+F"), true) {
            self.open_find();
        }
        if menu_item(ui, "查找下一个", Some("F3"), self.find_open) {
            self.find_step(true);
        }
        if menu_item(ui, "查找上一个", Some("Shift+F3"), self.find_open) {
            self.find_step(false);
        }
        if menu_item(ui, "全局搜索…", Some("Ctrl+Shift+F"), true) {
            self.open_global_search();
        }
        if menu_item(ui, "跳转到行…", Some("Ctrl+G"), true) {
            self.open_goto();
        }
        ui.separator();
        if menu_item(ui, "切换书签", Some("Ctrl+B"), self.selected.is_some()) {
            self.toggle_selected_bookmark();
        }
        if menu_item(ui, "仅书签视图", None, self.active.last().is_some()) {
            self.fork_bookmarks();
        }
    }

    fn display_menu(&mut self, ui: &mut Ui) {
        let mut sidebar = self.sidebar_open;
        if ui
            .checkbox(&mut sidebar, "工作区侧栏")
            .on_hover_text("浮层抽屉，不占用日志区宽度 (F9)")
            .changed()
        {
            self.set_sidebar(sidebar);
        }
        let mut filter_bar = self.filter_bar_open;
        if ui
            .checkbox(&mut filter_bar, "过滤栏")
            .on_hover_text("收起或展开顶部过滤栏；过滤条件、视图与书签保持不变")
            .changed()
        {
            self.set_filter_bar(filter_bar);
        }
        let mut follow = self.follow;
        if ui.checkbox(&mut follow, "跟随尾部").changed() {
            self.set_follow(follow);
        }
        ui.separator();
        ui.label(RichText::new("阅读方式").size(10.5).color(theme::p().faint));
        let mode = self.reading();
        for (choice, label) in [
            (Reading::Plain, "原文"),
            (Reading::Wrap, "换行"),
            (Reading::Fold, "折叠重复"),
            (Reading::Columns, "分列"),
        ] {
            if ui.radio(mode == choice, label).clicked() {
                self.set_reading(choice);
                ui.close_menu();
            }
        }
        let mut stats = self.stats_open;
        if ui
            .checkbox(&mut stats, "统计窗口")
            .on_hover_text("浮动窗口，可拖动/缩放，不占用日志宽度")
            .changed()
        {
            self.set_stats(stats);
        }
        ui.separator();
        if menu_item(ui, "放大字号", Some("Ctrl++"), self.font_size < FONT_MAX) {
            self.zoom_font(1.0);
        }
        if menu_item(ui, "缩小字号", Some("Ctrl+-"), self.font_size > FONT_MIN) {
            self.zoom_font(-1.0);
        }
        let reset_enabled = (self.font_size - log_view::FONT_SIZE).abs() > 0.05;
        if menu_item(ui, "重置字号", Some("Ctrl+0"), reset_enabled) {
            self.reset_zoom();
        }
        ui.separator();
        let has_view = self.active.last().is_some();
        if menu_item(ui, "跳到顶部", Some("Home"), has_view) {
            self.pending_scroll = Some(log_view::Scroll::Offset(0.0));
            self.status = "已跳到顶部".to_string();
        }
        if menu_item(ui, "跳到底部", Some("End"), has_view) {
            self.pending_scroll = Some(log_view::Scroll::Offset(f32::MAX));
            self.status = "已跳到底部".to_string();
        }
    }

    fn tree_menu(&mut self, ui: &mut Ui) {
        let active = self.active.last().copied();
        if menu_item(ui, "派生过滤视图", None, active.is_some()) {
            self.fork_active();
        }
        if menu_item(ui, "编辑过滤条件…", None, active.is_some()) {
            self.focus_filter();
        }
        if menu_item(ui, "替换当前过滤", None, active.is_some()) {
            self.apply_filter();
        }
        ui.separator();
        if menu_item(ui, "重命名 / 备注…", Some("F2"), active.is_some()) {
            self.rename_active();
        }
        let has_parent = active
            .and_then(|id| self.session.view(id))
            .and_then(|view| view.parent)
            .is_some();
        if menu_item(ui, "关闭当前子标签", None, has_parent) {
            self.remove_active_level();
        }
        ui.separator();
        if menu_item(
            ui,
            "关闭当前标签页",
            Some("Ctrl+W"),
            self.active.first().is_some(),
        ) {
            self.close_active_view();
        }
    }

    fn highlight_menu(&mut self, ui: &mut Ui) {
        ui.label(RichText::new("关键词").size(10.5).color(theme::p().faint));
        ui.add(
            TextEdit::singleline(&mut self.keyword_input)
                .desired_width(180.0)
                .hint_text("输入要高亮的词"),
        );
        ui.horizontal(|ui| {
            ui.label(RichText::new("颜色").size(10.5).color(theme::p().faint));
            ui.color_edit_button_srgb(&mut self.highlight_color);
        });
        ui.separator();
        let has_keyword = !self.keyword_input.trim().is_empty();
        if menu_item(ui, "整行高亮关键词", None, has_keyword) {
            self.add_keyword_rule(false);
        }
        if menu_item(ui, "文字高亮关键词", None, has_keyword) {
            self.add_keyword_rule(true);
        }
        ui.separator();
        let rules = self.session.rules().len();
        if menu_item(ui, &format!("清除全部高亮规则 ({rules})"), None, rules > 0) {
            self.clear_rules();
        }
    }

    fn help_menu(&mut self, ui: &mut Ui) {
        if menu_item(ui, "快捷键与提示", Some("F1"), true) {
            self.help_open = true;
        }
        if menu_item(ui, "关于 neo-log", None, true) {
            self.about_open = true;
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::none()
                    .fill(theme::p().chrome)
                    .inner_margin(egui::Margin::symmetric(8.0, 3.0)),
            )
            .show(ctx, |ui| {
                self.tab_strip(ui);
                if self.active.last().is_some() {
                    self.path_bar(ui);
                    if self.filter_bar_open {
                        self.filter_bar(ui);
                    }
                }
            });
    }

    fn tab_strip(&mut self, ui: &mut Ui) {
        let roots = self.session.roots().to_vec();
        let active_root = self.active.first().copied();
        let mut activate = None;
        let mut close = None;
        let mut rename = None;
        let mut side = None;
        let p = theme::p();

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            self.menu_button(ui);
            let toggle_tint = if self.sidebar_open {
                p.accent_strong
            } else {
                p.dim
            };
            let toggle = ui
                .add(
                    egui::Button::new(RichText::new("◧").size(13.0).color(toggle_tint))
                        .frame(false)
                        .min_size(egui::Vec2::new(22.0, 22.0)),
                )
                .on_hover_text("工作区侧栏：打开 / 收起 (F9) · 浮层显示，不占日志宽度");
            if toggle.clicked() {
                self.toggle_sidebar();
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("打开").size(12.0).color(p.text))
                        .frame(false)
                        .min_size(egui::Vec2::new(0.0, 22.0)),
                )
                .on_hover_text("打开日志文件 (Ctrl+O)")
                .clicked()
            {
                self.open_requested = true;
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("查找").size(12.0).color(if self.find_open {
                        p.accent_strong
                    } else {
                        p.text
                    }))
                    .frame(false)
                    .min_size(egui::Vec2::new(0.0, 22.0)),
                )
                .on_hover_text("在当前视图查找，浮在日志右上角 (Ctrl+F)")
                .clicked()
            {
                if self.find_open {
                    self.find_open = false;
                } else {
                    self.open_find();
                }
            }
            let (line, _) =
                ui.allocate_exact_size(egui::Vec2::new(1.0, 16.0), egui::Sense::hover());
            ui.painter().rect_filled(line, 0.0, p.border_soft);

            let tabs_width = ui.available_width().max(90.0);
            ui.allocate_ui_with_layout(
                egui::Vec2::new(tabs_width, 26.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("tab-scroll")
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                if roots.is_empty() {
                                    ui.label(
                                        RichText::new("打开日志后在此显示标签页")
                                            .size(11.0)
                                            .color(p.faint),
                                    );
                                }
                                for root in &roots {
                                    let Some(view) = self.session.view(*root) else {
                                        continue;
                                    };
                                    let title = truncate_chars(&view.title, 22);
                                    let kind = if matches!(view.base, Base::Merged(_)) {
                                        "⇄"
                                    } else {
                                        "▤"
                                    };
                                    let is_active = active_root == Some(*root);
                                    let mut hint = view.title.clone();
                                    if !view.note.is_empty() {
                                        hint.push_str(&format!("\n备注: {}", view.note));
                                    }
                                    hint.push_str("\n右键可重命名或关闭标签页");

                                    let frame = egui::Frame::none()
                                        .fill(if is_active { p.accent_soft } else { p.chrome })
                                        .stroke(egui::Stroke::new(
                                            1.0_f32,
                                            if is_active {
                                                p.accent.gamma_multiply(0.85)
                                            } else {
                                                p.border_soft
                                            },
                                        ))
                                        .rounding(egui::Rounding::same(6.0))
                                        .inner_margin(egui::Margin::symmetric(6.0, 1.0));
                                    frame.show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 4.0;
                                            ui.label(RichText::new(kind).size(9.5).color(
                                                if is_active { p.accent_strong } else { p.faint },
                                            ));
                                            let resp = ui.add(
                                                egui::Button::new(
                                                    RichText::new(&title).size(12.0).color(
                                                        if is_active { p.text } else { p.dim },
                                                    ),
                                                )
                                                .frame(false)
                                                .min_size(egui::Vec2::new(0.0, 20.0)),
                                            );
                                            if resp.clicked() {
                                                activate = Some(*root);
                                            }
                                            resp.on_hover_text(&hint).context_menu(|ui| {
                                                if ui.button("重命名 / 备注…").clicked() {
                                                    rename = Some(*root);
                                                    ui.close_menu();
                                                }
                                                if ui.button("在右侧对照").clicked() {
                                                    side = Some(*root);
                                                    ui.close_menu();
                                                }
                                                if ui.button("关闭标签页").clicked() {
                                                    close = Some(*root);
                                                    ui.close_menu();
                                                }
                                            });
                                            let resp = ui
                                                .add(
                                                    egui::Button::new(
                                                        RichText::new("×")
                                                            .size(11.0)
                                                            .color(p.faint),
                                                    )
                                                    .frame(false)
                                                    .min_size(egui::Vec2::new(14.0, 20.0)),
                                                )
                                                .on_hover_text("关闭标签页及其派生视图");
                                            if resp.clicked() {
                                                close = Some(*root);
                                            }
                                        });
                                    });
                                }
                            });
                        });
                },
            );
        });

        if let Some(root) = close {
            let title = self
                .session
                .view(root)
                .map(|view| view.title.clone())
                .unwrap_or_default();
            self.close_tab(root);
            self.status = format!("已关闭标签页 {title}");
        } else if let Some(root) = activate {
            self.activate_root(root);
        }
        if let Some(root) = rename {
            self.open_rename(root);
        }
        if let Some(root) = side {
            self.place_side(root);
        }
    }

    fn path_bar(&mut self, ui: &mut Ui) {
        let path = self.active.clone();
        let Some(view_id) = path.last().copied() else {
            return;
        };
        let rows = self.session.visible_len(view_id);
        let total = self.session.total_lines();
        let mode = self.reading();
        let root = path.first().copied();
        let tabs = root
            .map(|root| view_tabs(&self.session, root))
            .unwrap_or_default();
        let active_id = path.last().copied();
        let mut jump = None;
        let mut remove = None;
        let mut rename = None;
        let mut reading = None;
        let mut follow = None;

        ui.add_space(1.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let crumb_w = (ui.available_width() - 340.0).max(80.0);
            ui.allocate_ui_with_layout(
                egui::Vec2::new(crumb_w, 22.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("path-crumb")
                        .auto_shrink([false, true])
                        .max_height(22.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 3.0;
                                if tabs.len() <= 1 {
                                    if let Some(id) = active_id {
                                        if let Some(node) = self.session.view(id) {
                                            ui.label(
                                                RichText::new(truncate_chars(&node.title, 36))
                                                    .size(11.5)
                                                    .color(theme::p().dim),
                                            );
                                        }
                                    }
                                    return;
                                }
                                for id in &tabs {
                                    let Some(node) = self.session.view(*id) else {
                                        continue;
                                    };
                                    let current = active_id == Some(*id);
                                    let is_root = root == Some(*id);
                                    let depth =
                                        depth_under(&self.session, *id, root.unwrap_or(*id));
                                    let name = if is_root && node.query.is_empty() {
                                        "全部".to_string()
                                    } else {
                                        truncate_chars(&node.title, 22)
                                    };
                                    let label = if depth > 1 {
                                        format!("› {name}")
                                    } else {
                                        name
                                    };
                                    let hint = if node.query.is_empty() {
                                        "未过滤 · 点击切回这一层，不会关掉子标签".to_string()
                                    } else {
                                        format!("过滤: {}\n点击切换，× 关闭这个子标签", node.query)
                                    };
                                    let frame = egui::Frame::none()
                                        .fill(if current {
                                            theme::p().accent_soft
                                        } else {
                                            theme::p().surface
                                        })
                                        .stroke(egui::Stroke::new(
                                            1.0_f32,
                                            if current {
                                                theme::p().accent.gamma_multiply(0.8)
                                            } else {
                                                theme::p().border_soft
                                            },
                                        ))
                                        .rounding(egui::Rounding::same(5.0))
                                        .inner_margin(egui::Margin::symmetric(6.0, 1.0));
                                    frame.show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 2.0;
                                            let resp = ui
                                                .add(
                                                    egui::Button::new(
                                                        RichText::new(&label).size(11.0).color(
                                                            if current {
                                                                theme::p().text
                                                            } else {
                                                                theme::p().dim
                                                            },
                                                        ),
                                                    )
                                                    .frame(false)
                                                    .min_size(egui::Vec2::new(0.0, 18.0)),
                                                )
                                                .on_hover_text(hint);
                                            if resp.clicked() && !current {
                                                jump = Some(*id);
                                            }
                                            resp.context_menu(|ui| {
                                                if ui.button("重命名 / 备注…").clicked() {
                                                    rename = Some(*id);
                                                    ui.close_menu();
                                                }
                                                if !is_root && ui.button("关闭这个子标签").clicked()
                                                {
                                                    remove = Some(*id);
                                                    ui.close_menu();
                                                }
                                            });
                                            if !is_root
                                                && ui
                                                    .add(
                                                        egui::Button::new(
                                                            RichText::new("×")
                                                                .size(10.0)
                                                                .color(theme::p().faint),
                                                        )
                                                        .frame(false)
                                                        .min_size(egui::Vec2::new(12.0, 18.0)),
                                                    )
                                                    .on_hover_text("关闭这个子标签，回到上一层")
                                                    .clicked()
                                            {
                                                remove = Some(*id);
                                            }
                                        });
                                    });
                                }
                            });
                        });
                },
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{rows}/{total}"))
                        .size(10.5)
                        .color(theme::p().faint),
                )
                .on_hover_text("当前视图行数 / 全部文件行数");
                if mode_button(ui, "跟随", self.follow) {
                    follow = Some(!self.follow);
                }
                for (choice, label) in [
                    (Reading::Columns, "分列"),
                    (Reading::Fold, "折叠"),
                    (Reading::Wrap, "换行"),
                    (Reading::Plain, "原文"),
                ] {
                    if mode_button(ui, label, mode == choice) {
                        reading = Some(choice);
                    }
                }
            });
        });

        if let Some(id) = remove {
            self.close_child_tab(id);
        } else if let Some(id) = jump {
            self.set_active_path(self.session.view_path(id));
            let title = self
                .session
                .view(id)
                .map(|view| view.title.clone())
                .unwrap_or_default();
            self.status = format!("已切换到 {title}");
        }
        if let Some(id) = rename {
            self.open_rename(id);
        }
        if let Some(mode) = reading {
            self.set_reading(mode);
        }
        if let Some(on) = follow {
            self.set_follow(on);
        }
    }

    fn filter_bar(&mut self, ui: &mut Ui) {
        let focus = std::mem::take(&mut self.filter_focus);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new("过滤").size(11.0).color(theme::p().faint));
            let width = (ui.available_width() - 220.0).clamp(140.0, 640.0);
            let resp = ui.add(
                TextEdit::singleline(&mut self.filter_input)
                    .desired_width(width)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("error OR timeout · around:2 · time:23:00:00..01:00:00"),
            );
            if focus {
                resp.request_focus();
            }
            let enter = resp.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
            if ui
                .button("派生")
                .on_hover_text("在当前视图下新开一个子标签，父视图还在")
                .clicked()
                || enter
            {
                self.fork_active();
            }
            if ui
                .button("替换当前")
                .on_hover_text("改写这一级的过滤，不新增层级")
                .clicked()
            {
                self.apply_filter();
            }
            if ui.button("?").on_hover_text("过滤语法").clicked() {
                self.filter_help = !self.filter_help;
            }
            if ui.button("预设").on_hover_text("保存或套用查询").clicked() {
                self.preset_name = truncate_chars(self.filter_input.trim(), 24);
                self.preset_open = true;
            }
            if ui.button("预设").on_hover_text("保存或套用查询").clicked() {
                self.preset_name = truncate_chars(self.filter_input.trim(), 24);
                self.preset_open = true;
            }
        });
        if let Some(error) = &self.filter_error {
            ui.label(
                RichText::new(error)
                    .size(11.0)
                    .color(theme::ink(Severity::Error.color())),
            );
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("工作区").size(10.5).color(theme::p().faint));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .small_button("×")
                    .on_hover_text("收起侧栏 (F9) · 点击日志区也会收起")
                    .clicked()
                {
                    self.set_sidebar(false);
                }
            });
        });
        ui.add_space(2.0);

        let docs: Vec<(
            DocId,
            String,
            usize,
            String,
            String,
            usize,
            TextEncoding,
            bool,
        )> = self
            .session
            .docs()
            .iter()
            .map(|doc| {
                (
                    doc.id,
                    doc.short_name(),
                    doc.len(),
                    doc.plugin_id.clone(),
                    self.session.plugin_for(doc.id).name().to_string(),
                    self.session.bookmark_count(doc.id),
                    doc.encoding,
                    doc.missing,
                )
            })
            .collect();
        let plugins: Vec<(String, String)> = self
            .session
            .plugins()
            .all()
            .map(|plugin| (plugin.id().to_string(), plugin.name().to_string()))
            .collect();
        let docs_len = docs.len();
        self.merge_selected.resize(docs_len, false);

        let mut plugin_change = None;
        let mut encoding_change = None;
        let mut open_doc_view = None;
        let mut merge_now = false;

        theme::card().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("文档").size(12.0).strong());
                theme::chip(
                    ui,
                    docs.len().to_string(),
                    theme::p().dim,
                    theme::p().surface_high,
                );
            });
            ui.add_space(1.0);
            if docs.is_empty() {
                ui.label(
                    RichText::new("尚未打开文件")
                        .size(11.5)
                        .color(theme::p().faint),
                );
            }
            for (idx, (doc_id, name, len, plugin_id, plugin_name, bookmarks, encoding, missing)) in
                docs.iter().enumerate()
            {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.merge_selected[idx], "合并");
                    let (dot, _) =
                        ui.allocate_exact_size(egui::Vec2::splat(8.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 3.5, theme::doc_color(*doc_id));
                    if ui
                        .add(egui::Button::new(RichText::new(name).size(12.5)).frame(false))
                        .on_hover_text("打开或切换到该文档视图")
                        .clicked()
                    {
                        open_doc_view = Some(*doc_id);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = if *missing {
                            "不可用".to_string()
                        } else {
                            format!("{len} 行")
                        };
                        ui.label(RichText::new(label).size(10.0).color(if *missing {
                            theme::ink(Severity::Error.color())
                        } else {
                            theme::p().faint
                        }));
                    });
                });
                ui.horizontal(|ui| {
                    ui.add_space(24.0);
                    egui::ComboBox::from_id_salt(("plugin", *doc_id))
                        .selected_text(RichText::new(plugin_name).size(11.0))
                        .width(108.0)
                        .show_ui(ui, |ui| {
                            for (id, label) in &plugins {
                                if ui.selectable_label(id == plugin_id, label).clicked() {
                                    plugin_change = Some((*doc_id, id.clone()));
                                }
                            }
                        });
                    egui::ComboBox::from_id_salt(("encoding", *doc_id))
                        .selected_text(RichText::new(encoding.label()).size(11.0))
                        .width(84.0)
                        .show_ui(ui, |ui| {
                            for choice in [TextEncoding::Utf8, TextEncoding::Gb18030] {
                                if ui
                                    .selectable_label(choice == *encoding, choice.label())
                                    .clicked()
                                {
                                    encoding_change = Some((*doc_id, choice));
                                }
                            }
                        });
                    if *bookmarks > 0 {
                        theme::chip(
                            ui,
                            format!("★ {bookmarks}"),
                            theme::p().bookmark,
                            theme::p().surface_high,
                        );
                    }
                });
                if idx + 1 < docs.len() {
                    ui.add_space(2.0);
                    theme::hairline(ui);
                    ui.add_space(2.0);
                }
            }
            let selected = self.merge_selected.iter().filter(|flag| **flag).count();
            if ui
                .add_enabled(
                    selected > 0,
                    egui::Button::new(format!("按时间合并已选 ({selected})")),
                )
                .clicked()
            {
                merge_now = true;
            }
        });

        if merge_now {
            self.merge_selected_docs();
        }

        if let Some((doc, plugin)) = plugin_change {
            if self.session.set_doc_plugin(doc, &plugin) {
                self.status = format!("已切换插件: {plugin}（时间轴已重建）");
            }
        }
        if let Some((doc, encoding)) = encoding_change {
            match self.session.set_doc_encoding(doc, encoding) {
                Ok(()) => self.status = format!("已按 {} 重新读取", encoding.label()),
                Err(error) => self.status = format!("按 {} 重读失败: {error}", encoding.label()),
            }
        }
        if let Some(doc) = open_doc_view {
            self.activate_doc(doc);
        }

        self.rules_panel(ui);
        self.bookmarks_panel(ui);
    }

    fn sidebar_drawer(&mut self, ctx: &egui::Context, body: egui::Rect) {
        if !self.sidebar_open {
            return;
        }
        let width = 318.0_f32.min((body.width() * 0.85).max(220.0));
        let response = egui::Area::new(egui::Id::new("sidebar-drawer"))
            .order(egui::Order::Middle)
            .fixed_pos(body.left_top())
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.set_height(body.height());
                egui::Frame::none()
                    .fill(theme::p().chrome)
                    .stroke(egui::Stroke::new(1.0_f32, theme::p().border))
                    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                    .show(ui, |ui| {
                        ui.set_width(width - 20.0);
                        egui::ScrollArea::vertical()
                            .id_salt("sidebar-scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                self.sidebar(ui);
                            });
                    });
            });
        let rect = response.response.rect;
        if self.sidebar_guard {
            self.sidebar_guard = false;
            return;
        }
        let Some(pos) = ctx.input(|input| input.pointer.interact_pos()) else {
            return;
        };
        let clicked = ctx.input(|input| input.pointer.any_click());
        let over_background = ctx
            .layer_id_at(pos)
            .is_none_or(|layer| layer.order == egui::Order::Background);
        if clicked && over_background && body.contains(pos) && !rect.contains(pos) {
            self.set_sidebar(false);
        }
    }

    fn stats_window(&mut self, ctx: &egui::Context) {
        if !self.stats_open {
            return;
        }
        let mut open = true;
        let mut action = None;
        egui::Window::new("统计")
            .id(egui::Id::new("stats-window"))
            .open(&mut open)
            .resizable(true)
            .default_width(320.0)
            .default_height(420.0)
            .default_pos(egui::pos2(
                ctx.screen_rect().right() - 360.0,
                ctx.screen_rect().top() + 96.0,
            ))
            .frame(egui::Frame::window(&ctx.style()).fill(theme::p().chrome))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("stats-scroll")
                    .auto_shrink([false, false])
                    .max_height(540.0)
                    .show(ui, |ui| match self.active.last().copied() {
                        Some(view) => {
                            action = stats::show(ui, &self.session, view, &mut self.stats_cache);
                        }
                        None => {
                            ui.add_space(6.0);
                            ui.label(RichText::new("打开日志后可查看统计").color(theme::p().dim));
                        }
                    });
            });
        if !open {
            self.set_stats(false);
            return;
        }
        match action {
            Some(stats::Action::Jump { row, note }) => {
                if let Some(view) = self.active.last().copied() {
                    if let Some(line) = self.session.line_ref(view, row) {
                        self.selected = Some(line);
                        self.selection = None;
                        self.pending_scroll = Some(log_view::Scroll::Row(row));
                        self.status = note;
                    }
                }
            }
            Some(stats::Action::Filter(query)) => {
                self.filter_input = query;
                self.fork_active();
            }
            None => {}
        }
    }

    fn rules_panel(&mut self, ui: &mut Ui) {
        let rules: Vec<(u64, String, String, bool, Option<Rgb>, Option<Rgb>)> = self
            .session
            .rules()
            .iter()
            .map(|rule| {
                let scope = match &rule.scope {
                    RuleScope::Global => "全局".to_string(),
                    RuleScope::Doc(doc) => self
                        .session
                        .doc(*doc)
                        .map(|doc| doc.short_name())
                        .unwrap_or_else(|| format!("文档 {doc}")),
                    RuleScope::Plugin(plugin) => format!("插件 {plugin}"),
                };
                (
                    rule.id,
                    rule.label.clone(),
                    scope,
                    rule.row_bg.is_some(),
                    rule.row_bg,
                    rule.style.fg,
                )
            })
            .collect();
        let mut remove = None;
        theme::card().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("高亮规则").size(12.0).strong());
                theme::chip(
                    ui,
                    rules.len().to_string(),
                    theme::p().dim,
                    theme::p().surface_high,
                );
            });
            ui.add_space(1.0);
            if rules.is_empty() {
                ui.label(
                    RichText::new("双击日志中的词，或右键选择高亮")
                        .size(11.0)
                        .color(theme::p().faint),
                );
            }
            for (id, label, scope, is_row_bg, row_bg, fg) in rules {
                ui.horizontal(|ui| {
                    let swatch = row_bg.or(fg).unwrap_or(Rgb(255, 255, 255));
                    ui.label(RichText::new("■").size(9.0).color(theme::color(swatch)));
                    ui.label(RichText::new(label).size(12.0));
                    ui.label(
                        RichText::new(format!(
                            "{scope} · {}",
                            if is_row_bg { "行背景" } else { "文字" }
                        ))
                        .size(10.0)
                        .color(theme::p().faint),
                    );
                    if ui
                        .add(egui::Button::new(RichText::new("×").size(11.0)).frame(false))
                        .on_hover_text("删除该规则")
                        .clicked()
                    {
                        remove = Some(id);
                    }
                });
            }
        });
        if let Some(id) = remove {
            self.session.remove_rule(id);
            self.status = format!("已删除高亮规则 #{id}");
        }
    }

    fn bookmarks_panel(&mut self, ui: &mut Ui) {
        let Some(view) = self.active.last().copied() else {
            return;
        };
        let rows = self.session.bookmarked_rows(view);
        theme::card().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("书签").size(12.0).strong());
                theme::chip(
                    ui,
                    rows.len().to_string(),
                    theme::p().dim,
                    theme::p().surface_high,
                );
            });
            ui.add_space(1.0);
            if rows.is_empty() {
                ui.label(
                    RichText::new("选中行后按 Ctrl+B，或点击行号左侧圆点")
                        .size(11.0)
                        .color(theme::p().faint),
                );
            }
            for row in rows.iter().take(500) {
                let Some(line) = self.session.line_ref(view, *row) else {
                    continue;
                };
                let doc = self
                    .session
                    .doc(line.doc)
                    .map(|doc| doc.short_name())
                    .unwrap_or_default();
                let snippet =
                    truncate_chars(&self.session.line_text(line).replace(['\n', '\r'], " "), 42);
                let label = format!("{doc}:{}  {snippet}", line.line as usize + 1);
                if ui
                    .selectable_label(
                        self.selected == Some(line),
                        RichText::new(label).monospace().size(11.0),
                    )
                    .clicked()
                {
                    self.selected = Some(line);
                    self.selection = None;
                    self.pending_scroll = Some(log_view::Scroll::Row(*row));
                }
            }
        });
    }

    fn main_view(&mut self, ui: &mut Ui) {
        if self.session.roots().is_empty() {
            self.empty_state(ui);
            return;
        }
        let active_ok = self
            .active
            .last()
            .copied()
            .is_some_and(|id| self.session.view(id).is_some());
        if !active_ok {
            self.empty_state(ui);
            return;
        }
        if self
            .side_root
            .is_some_and(|root| self.session.view(root).is_none())
        {
            self.side_root = None;
        }
        let applying_main = self.pending_scroll.is_some();
        let applying_side = self.side_scroll.is_some();
        let row_h = log_view::row_height(ui.ctx(), self.font_size);
        if self.side_root.is_none() {
            self.paint_active(ui);
            self.finish_sync(applying_main, false, row_h);
            return;
        }
        let width = ui.available_width();
        let height = ui.available_height();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width((width * 0.5 - 3.0).max(120.0));
                ui.set_height(height);
                self.paint_active(ui);
            });
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.set_height(height);
                self.paint_side(ui);
            });
        });
        self.finish_sync(applying_main, applying_side, row_h);
    }

    fn paint_active(&mut self, ui: &mut Ui) {
        if self.session.roots().is_empty() {
            self.empty_state(ui);
            return;
        }
        let Some(active) = self
            .active
            .last()
            .copied()
            .filter(|id| self.session.view(*id).is_some())
        else {
            self.empty_state(ui);
            return;
        };

        let (rows, has_parent) = {
            let Some(view) = self.session.view(active) else {
                self.empty_state(ui);
                return;
            };
            (view.rows.len(self.session.docs()), view.parent.is_some())
        };

        if rows == 0 {
            ui.add_space(48.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("当前视图没有匹配的日志行")
                        .size(13.0)
                        .color(theme::p().dim),
                );
                ui.add_space(8.0);
                if ui.button("编辑条件").clicked() {
                    self.focus_filter();
                }
                if has_parent && ui.button("回到上一层").clicked() {
                    if let Some(parent) = self.session.view(active).and_then(|view| view.parent) {
                        self.set_active_path(self.session.view_path(parent));
                        self.status = "已切回上一层，子标签还在".to_string();
                    }
                }
                if has_parent && ui.button("关闭这个子标签").clicked() {
                    self.remove_active_level();
                }
            });
            return;
        }

        let find = if self.find_open {
            self.find_cache.matcher.as_ref()
        } else {
            None
        };
        let response = log_view::show(
            ui,
            &self.session,
            active,
            log_view::Options {
                selected: self.selected,
                scroll: self.pending_scroll.take(),
                selection: self.selection,
                wrap: self.wrap,
                fold: self.fold,
                columns: self.columns,
                font_size: self.font_size,
                find,
            },
            &mut self.wrap_cache,
            &mut self.fold_cache,
            &mut self.columns_cache,
            &mut self.row_cache,
        );
        self.selected = response.selected;
        self.selection = response.selection;
        self.last_scroll_offset = response.scroll_offset;
        self.last_viewport_height = response.viewport_height;

        if let Some(group) = response.toggle_fold {
            self.fold_cache.toggle(group);
            self.selection = None;
        }
        if let Some(line) = response.toggle_bookmark {
            let lines = self.session.event_len(line);
            let marked = self.session.toggle_bookmark(line);
            self.status = if marked {
                if lines > 1 {
                    format!("已加书签 · 整条事件 {lines} 行")
                } else {
                    format!("已加书签 · 第 {} 行", line.line as usize + 1)
                }
            } else if lines > 1 {
                format!("已移除整条事件的书签 · {lines} 行")
            } else {
                "已移除书签".to_string()
            };
        }
        if let Some((line, word)) = response.highlight {
            self.add_word_rule(line, &word);
        }
        if let Some(action) = response.action {
            let origin = active;
            self.handle_row_action(ui.ctx(), action, origin, false);
        }
    }

    fn paint_side(&mut self, ui: &mut Ui) {
        let Some(root) = self.side_root else {
            return;
        };
        let view = resolve_leaf(&self.session, &self.leaves, root);
        ui.horizontal(|ui| {
            let title = self
                .session
                .view(view)
                .map(|node| node.title.clone())
                .unwrap_or_default();
            ui.label(
                RichText::new(truncate_chars(&title, 28))
                    .size(11.0)
                    .strong(),
            );
            let mut sync = self.side_sync;
            if ui.checkbox(&mut sync, "同步滚动").changed() {
                self.side_sync = sync;
            }
            if ui.small_button("关闭").clicked() {
                self.side_root = None;
                self.side_scroll = None;
            }
        });
        if self.side_root.is_none() {
            return;
        }
        let rows = self.session.visible_len(view);
        if rows == 0 {
            ui.label(RichText::new("这一侧没有匹配的日志行").color(theme::p().dim));
            return;
        }
        let find = if self.find_open {
            self.find_cache.matcher.as_ref()
        } else {
            None
        };
        let response = log_view::show(
            ui,
            &self.session,
            view,
            log_view::Options {
                selected: self.side_selected,
                scroll: self.side_scroll.take(),
                selection: self.side_selection,
                wrap: self.wrap,
                fold: self.fold,
                columns: self.columns,
                font_size: self.font_size,
                find,
            },
            &mut self.side_wrap,
            &mut self.side_fold,
            &mut self.side_columns,
            &mut self.side_rows,
        );
        self.side_selected = response.selected;
        self.side_selection = response.selection;
        self.side_offset = response.scroll_offset;
        if let Some(group) = response.toggle_fold {
            self.side_fold.toggle(group);
            self.side_selection = None;
        }
        if let Some(line) = response.toggle_bookmark {
            let _ = self.session.toggle_bookmark(line);
        }
        if let Some((line, word)) = response.highlight {
            self.add_word_rule(line, &word);
        }
        if let Some(action) = response.action {
            self.handle_row_action(ui.ctx(), action, view, true);
        }
    }

    fn finish_sync(&mut self, applying_main: bool, applying_side: bool, row_h: f32) {
        if applying_main {
            self.sync_main = self.last_scroll_offset;
        }
        if applying_side {
            self.sync_side = self.side_offset;
        }
        if !self.side_sync || self.side_root.is_none() || applying_main || applying_side {
            if !applying_main {
                self.sync_main = self.last_scroll_offset;
            }
            if !applying_side {
                self.sync_side = self.side_offset;
            }
            return;
        }
        let main_moved = (self.last_scroll_offset - self.sync_main).abs() > 2.0;
        let side_moved = (self.side_offset - self.sync_side).abs() > 2.0;
        if main_moved == side_moved {
            self.sync_main = self.last_scroll_offset;
            self.sync_side = self.side_offset;
            return;
        }
        let to_side = main_moved;
        let from = if to_side {
            self.active.last().copied()
        } else {
            self.side_root
                .map(|root| resolve_leaf(&self.session, &self.leaves, root))
        };
        let Some(from) = from else {
            return;
        };
        let offset = if to_side {
            self.last_scroll_offset
        } else {
            self.side_offset
        };
        let row = (offset / row_h.max(1.0)).floor().max(0.0) as usize;
        let target_view = if to_side {
            self.side_root
                .map(|root| resolve_leaf(&self.session, &self.leaves, root))
        } else {
            self.active.last().copied()
        };
        let Some(target_view) = target_view else {
            return;
        };
        let target = self
            .session
            .line_ref(
                from,
                row.min(self.session.visible_len(from).saturating_sub(1)),
            )
            .and_then(|lr| self.session.line_time(lr))
            .map(|time| row_at_time(&self.session, target_view, time))
            .unwrap_or(row);
        if to_side {
            self.side_scroll = Some(log_view::Scroll::Row(target));
            self.sync_main = self.last_scroll_offset;
        } else {
            self.pending_scroll = Some(log_view::Scroll::Row(target));
            self.sync_side = self.side_offset;
        }
    }

    fn handle_row_action(
        &mut self,
        ctx: &egui::Context,
        action: log_view::RowAction,
        origin: ViewId,
        side: bool,
    ) {
        use log_view::RowAction;
        match action {
            RowAction::CopyLine(line) => {
                let lines = self.session.event_len(line);
                let text = self.session.event_text(line);
                ctx.copy_text(text);
                self.status = if lines > 1 {
                    format!("已复制整条事件 · {lines} 行")
                } else {
                    format!("已复制第 {} 行", line.line as usize + 1)
                };
            }
            RowAction::CopyText(text) => {
                ctx.copy_text(text);
                self.status = "已复制所选内容".to_string();
            }
            RowAction::ToggleBookmark(line) => {
                let lines = self.session.event_len(line);
                let marked = self.session.toggle_bookmark(line);
                self.status = if marked {
                    if lines > 1 {
                        format!("已加书签 · 整条事件 {lines} 行")
                    } else {
                        format!("已加书签 · 第 {} 行", line.line as usize + 1)
                    }
                } else if lines > 1 {
                    format!("已移除整条事件的书签 · {lines} 行")
                } else {
                    "已移除书签".to_string()
                };
            }
            RowAction::Highlight { line, word, .. } => {
                self.highlight_pop = Some(PendingHighlight { line, word });
            }
            RowAction::FilterWord(word) => self.fork_from(origin, literal_query(&word), side),
            RowAction::FilterLevel(severity) => self.fork_from(
                origin,
                format!("level={}", severity.label().to_ascii_lowercase()),
                side,
            ),
            RowAction::FilterModule(module) => self.fork_from(origin, module_query(&module), side),
            RowAction::Correlate { key, value } => self.open_correlation(&key, &value),
            RowAction::GlobalSearch(word) => {
                self.find_input = word;
                self.open_global_search();
            }
            RowAction::ToggleFold(group) => {
                if side {
                    self.side_fold.toggle(group);
                    self.side_selection = None;
                } else {
                    self.fold_cache.toggle(group);
                    self.selection = None;
                }
            }
        }
    }

    fn empty_state(&mut self, ui: &mut Ui) {
        ui.add_space(64.0);
        ui.vertical_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(52.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(rect, egui::Rounding::same(15.0), theme::p().accent);
            for idx in 0..3 {
                let inset = if idx == 1 { 13.0 } else { 15.0 };
                let y = rect.top() + 18.0 + idx as f32 * 8.0;
                painter.line_segment(
                    [
                        egui::pos2(rect.left() + inset, y),
                        egui::pos2(rect.right() - inset, y),
                    ],
                    egui::Stroke::new(3.0_f32, theme::p().bg),
                );
            }
            ui.add_space(14.0);
            ui.label(RichText::new("neo-log").size(28.0).strong());
            ui.label(
                RichText::new("多文件按时间合并 · 多级过滤视图 · 书签 · 插件高亮")
                    .size(12.0)
                    .color(theme::p().dim),
            );
            ui.add_space(20.0);
            if theme::primary_button(ui, "打开日志文件").clicked() {
                self.open_requested = true;
            }
            ui.add_space(4.0);
            if ui.button("加载示例日志").clicked() {
                self.load_demo();
            }
            ui.add_space(12.0);
            theme::chip(
                ui,
                "支持拖拽文件 · Ctrl+O",
                theme::p().faint,
                theme::p().surface,
            );
        });
    }

    fn find_overlay(&mut self, ctx: &egui::Context, body: egui::Rect) {
        if !self.find_open {
            return;
        }
        let width = 520.0_f32.min((body.width() - 12.0).max(280.0));
        let pos = egui::pos2(
            (body.right() - width - 6.0).max(body.left()),
            body.top() + 4.0,
        );
        egui::Area::new(egui::Id::new("find-overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme::p().chrome)
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        theme::p().accent.gamma_multiply(0.65),
                    ))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                    .show(ui, |ui| {
                        ui.set_max_width(width);
                        self.find_bar(ui);
                    });
            });
    }

    fn filter_help_window(&mut self, ctx: &egui::Context) {
        if !self.filter_help {
            return;
        }
        let mut open = true;
        egui::Window::new("过滤语法")
            .anchor(Align2::CENTER_TOP, [0.0, 72.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.label(
                    RichText::new(
                        "空格和 AND 都是与。OR 优先级更低，括号可以改组合。Enter 派生新的一层。",
                    )
                    .size(11.0)
                    .color(theme::p().dim),
                );
                ui.add_space(4.0);
                for (sample, meaning) in [
                    ("error OR timeout", "命中任一"),
                    ("(error OR timeout) camera", "先或，再与"),
                    ("around:2 error", "命中行前后各 2 行，并带上整条事件"),
                    ("\"connection reset\"", "带空格的短语"),
                    ("!debug", "排除"),
                    ("re:failed.*", "正则，不区分大小写"),
                    ("re-s:Failed", "正则，区分大小写"),
                    ("level>=warn", "级别比较"),
                    ("module:planner", "模块名包含"),
                    ("field:pid=4242", "字段相等"),
                    ("field:msg~=timeout", "字段包含"),
                    ("time:08:00:00..09:00:00", "一天中的时刻"),
                    ("time:23:00:00..01:00:00", "跨午夜"),
                    (
                        "time:2026-10-31T08:00:00..2026-10-31T09:30:00",
                        "绝对时间窗",
                    ),
                    ("id:abc-123", "跨文件串同一 trace / request id"),
                    ("bm:", "书签，加减后这个视图会跟着变"),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(sample)
                                .monospace()
                                .size(11.0)
                                .color(theme::p().text),
                        );
                        ui.label(RichText::new(meaning).size(11.0).color(theme::p().dim));
                    });
                }
            });
        self.filter_help = open;
    }

    fn preset_window(&mut self, ctx: &egui::Context) {
        if !self.preset_open {
            return;
        }
        let mut open = true;
        let mut save = false;
        let mut apply = None;
        let mut remove = None;
        egui::Window::new("查询预设")
            .anchor(Align2::CENTER_TOP, [0.0, 96.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(420.0);
                ui.label(
                    RichText::new(
                        "把当前过滤条件留下来，下次直接套用。查找历史也写在同一份配置里。",
                    )
                    .size(11.0)
                    .color(theme::p().dim),
                );
                ui.horizontal(|ui| {
                    ui.add(
                        TextEdit::singleline(&mut self.preset_name)
                            .hint_text("预设名称")
                            .desired_width(180.0),
                    );
                    if ui.button("保存当前查询").clicked() {
                        save = true;
                    }
                });
                ui.separator();
                if self.presets.is_empty() {
                    ui.label(
                        RichText::new("还没有预设")
                            .size(11.0)
                            .color(theme::p().faint),
                    );
                }
                let presets = self.presets.clone();
                for (idx, preset) in presets.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&preset.name).size(12.0));
                        ui.label(
                            RichText::new(&preset.query)
                                .monospace()
                                .size(11.0)
                                .color(theme::p().dim),
                        );
                        if ui.button("使用").clicked() {
                            apply = Some(preset.query.clone());
                        }
                        if ui.small_button("删除").clicked() {
                            remove = Some(idx);
                        }
                    });
                }
            });
        self.preset_open = open;
        if save {
            self.save_preset();
        }
        if let Some(query) = apply {
            self.apply_preset(query);
        }
        if let Some(idx) = remove {
            self.presets.remove(idx);
            self.save_user_state();
        }
    }

    fn dir_window(&mut self, ctx: &egui::Context) {
        let Some(path) = self.dir_pop.clone() else {
            return;
        };
        let mut open = true;
        let mut confirm = false;
        egui::Window::new("打开目录")
            .anchor(Align2::CENTER_TOP, [0.0, 120.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.label(
                    RichText::new(path.display().to_string())
                        .monospace()
                        .size(11.0),
                );
                ui.horizontal(|ui| {
                    ui.label(RichText::new("通配符").size(11.0).color(theme::p().dim));
                    ui.add(
                        TextEdit::singleline(&mut self.dir_glob)
                            .desired_width(280.0)
                            .font(egui::TextStyle::Monospace),
                    );
                });
                ui.label(
                    RichText::new("逗号分隔。新出现的匹配文件会并入这个目录的时间线。")
                        .size(11.0)
                        .color(theme::p().faint),
                );
                if ui.button("打开").clicked() {
                    confirm = true;
                }
            });
        if !open {
            self.dir_pop = None;
        }
        if confirm {
            let glob = self.dir_glob.clone();
            self.dir_pop = None;
            self.open_directory(path, &glob);
        }
    }

    fn highlight_window(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.highlight_pop.clone() else {
            return;
        };
        let mut open = true;
        let mut apply: Option<bool> = None;
        egui::Window::new("高亮这个词")
            .anchor(Align2::CENTER_TOP, [0.0, 88.0])
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(280.0);
                ui.label(
                    RichText::new(truncate_chars(&pending.word, 32))
                        .monospace()
                        .size(13.0),
                );
                ui.horizontal(|ui| {
                    ui.label(RichText::new("颜色").size(11.0).color(theme::p().faint));
                    ui.color_edit_button_srgb(&mut self.highlight_color);
                });
                ui.horizontal(|ui| {
                    if ui.button("文字").clicked() {
                        apply = Some(false);
                    }
                    if ui.button("整行").clicked() {
                        apply = Some(true);
                    }
                });
            });
        if let Some(row_bg) = apply {
            self.add_row_word_rule(pending.line, &pending.word, row_bg);
            self.highlight_pop = None;
        } else if !open {
            self.highlight_pop = None;
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        theme::hairline(ui);
        let levels = self
            .active
            .last()
            .copied()
            .map(|view| stats::cached_levels(&mut self.stats_cache, &self.session, view));
        let mut filter_level = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let status_color = if self.filter_error.is_some() {
                theme::ink(Severity::Error.color())
            } else {
                theme::p().dim
            };
            ui.label(RichText::new(&self.status).size(10.5).color(status_color));
            if let Some(line) = self.selected {
                ui.label(RichText::new("│").size(10.0).color(theme::p().border));
                let doc = self
                    .session
                    .doc(line.doc)
                    .map(|doc| doc.short_name())
                    .unwrap_or_default();
                ui.label(
                    RichText::new(format!("{doc}:{}", line.line as usize + 1))
                        .monospace()
                        .size(10.0)
                        .color(theme::p().text),
                );
                if let Some(time) = self.session.line_time(line) {
                    ui.label(
                        RichText::new(format_ts(time))
                            .monospace()
                            .size(10.0)
                            .color(theme::p().faint),
                    );
                }
            }
            if let Some(levels) = levels {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    for level in [
                        Severity::Fatal,
                        Severity::Error,
                        Severity::Warn,
                        Severity::Info,
                        Severity::Debug,
                        Severity::Trace,
                    ] {
                        let count = levels[level as usize];
                        if count == 0 {
                            continue;
                        }
                        let short = match level {
                            Severity::Trace => "TRC",
                            Severity::Debug => "DBG",
                            Severity::Info => "INF",
                            Severity::Warn => "WRN",
                            Severity::Error => "ERR",
                            Severity::Fatal => "FTL",
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(format!("{short} {count}"))
                                        .size(10.0)
                                        .color(theme::ink(level.color())),
                                )
                                .frame(false)
                                .min_size(egui::Vec2::new(0.0, 16.0)),
                            )
                            .on_hover_text(format!("只显示 {}", level.label()))
                            .clicked()
                        {
                            filter_level = Some(level);
                        }
                    }
                });
            }
        });
        if let Some(level) = filter_level {
            self.fork_query(format!("level={}", level.label().to_ascii_lowercase()));
        }
    }
}

impl eframe::App for NeoLogApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.saved_on_close && ctx.input(|input| input.viewport().close_requested()) {
            self.saved_on_close = true;
            self.save_user_state();
            self.save_session(true);
        }
        let kicked = self
            .watcher
            .as_ref()
            .is_some_and(|watcher| watcher.kicked());
        if kicked {
            self.ingest_new_files();
            if self.follow {
                self.last_watch = Instant::now() - Duration::from_secs(2);
            }
        }
        let structure = self.session.structure_rev();
        if structure != self.saved_structure && structure != self.save_failed_rev {
            self.save_session(true);
        }
        self.handle_drops(ctx);
        self.handle_shortcuts(ctx);
        self.fix_active();
        self.sync_filter_input();
        self.check_tail(ctx);

        self.top_bar(ctx);

        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::none()
                    .fill(theme::p().chrome)
                    .inner_margin(egui::Margin::symmetric(10.0, 2.0)),
            )
            .show(ctx, |ui| self.status_bar(ui));

        let body = ctx.available_rect();

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme::p().bg)
                    .inner_margin(egui::Margin::ZERO),
            )
            .show(ctx, |ui| self.main_view(ui));

        self.sidebar_drawer(ctx, body);
        self.find_overlay(ctx, body);
        self.stats_window(ctx);
        self.filter_help_window(ctx);
        self.preset_window(ctx);
        self.dir_window(ctx);
        self.highlight_window(ctx);

        self.goto_window(ctx);
        self.global_window(ctx);
        self.rename_window(ctx);
        self.help_window(ctx);
        self.about_window(ctx);

        if self.open_requested {
            self.open_requested = false;
            self.pick_files();
        }
    }
}

fn id_query(value: &str) -> String {
    if value
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '(' | ')'))
    {
        format!("\"id:{}\"", value.replace('"', ""))
    } else {
        format!("id:{value}")
    }
}

fn row_at_time(session: &Session, view: ViewId, time: i64) -> usize {
    let len = session.visible_len(view);
    if len == 0 {
        return 0;
    }
    let mut lo = 0;
    let mut hi = len;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let stamp = session
            .line_ref(view, mid)
            .and_then(|lr| session.line_time(lr))
            .unwrap_or(i64::MAX);
        if stamp < time {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.min(len - 1)
}

fn split_globs(text: &str) -> Vec<String> {
    text.split([',', ';'])
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn walk_logs(dir: &Path, patterns: &[String], depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        if path.is_dir() {
            walk_logs(&path, patterns, depth - 1, out);
        } else if patterns.iter().any(|pattern| glob_match(name, pattern)) {
            out.push(path);
        }
    }
}

fn glob_match(name: &str, pattern: &str) -> bool {
    fn rec(name: &[u8], pattern: &[u8]) -> bool {
        if pattern.is_empty() {
            return name.is_empty();
        }
        if pattern[0] == b'*' {
            return rec(name, &pattern[1..]) || (!name.is_empty() && rec(&name[1..], pattern));
        }
        if pattern[0] == b'?' {
            return !name.is_empty() && rec(&name[1..], &pattern[1..]);
        }
        !name.is_empty()
            && name[0].eq_ignore_ascii_case(&pattern[0])
            && rec(&name[1..], &pattern[1..])
    }
    rec(name.as_bytes(), pattern.as_bytes())
}

fn mode_button(ui: &mut Ui, label: &str, on: bool) -> bool {
    let p = theme::p();
    ui.add(
        egui::Button::new(RichText::new(label).size(11.0).color(if on {
            p.accent_strong
        } else {
            p.dim
        }))
        .frame(true)
        .fill(if on {
            p.accent_soft
        } else {
            egui::Color32::TRANSPARENT
        })
        .stroke(egui::Stroke::NONE)
        .min_size(egui::Vec2::new(0.0, 18.0)),
    )
    .clicked()
}

fn menu_item(ui: &mut Ui, label: &str, shortcut: Option<&str>, enabled: bool) -> bool {
    let mut button = egui::Button::new(label);
    if let Some(shortcut) = shortcut {
        button = button.shortcut_text(shortcut);
    }
    if ui.add_enabled(enabled, button).clicked() {
        ui.close_menu();
        true
    } else {
        false
    }
}

fn shortcut_section(ui: &mut Ui, title: &str, rows: &[(&str, &str)]) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(title)
            .size(12.0)
            .strong()
            .color(theme::p().accent_strong),
    );
    ui.add_space(2.0);
    egui::Grid::new(format!("shortcut-grid-{title}"))
        .num_columns(2)
        .spacing([18.0, 3.0])
        .show(ui, |ui| {
            for (keys, action) in rows {
                ui.label(
                    RichText::new(*keys)
                        .monospace()
                        .size(11.0)
                        .color(theme::p().text),
                );
                ui.label(RichText::new(*action).size(11.0).color(theme::p().dim));
                ui.end_row();
            }
        });
}

fn view_tabs(session: &Session, root: ViewId) -> Vec<ViewId> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        out.push(id);
        if let Some(node) = session.view(id) {
            for child in node.children.iter().rev() {
                stack.push(*child);
            }
        }
    }
    out
}

fn depth_under(session: &Session, id: ViewId, root: ViewId) -> usize {
    let mut depth = 0;
    let mut cursor = Some(id);
    while let Some(current) = cursor {
        if current == root {
            return depth;
        }
        depth += 1;
        if depth > 64 {
            break;
        }
        cursor = session.view(current).and_then(|view| view.parent);
    }
    depth
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn under_root(session: &Session, leaf: ViewId, root: ViewId) -> bool {
    let mut cursor = Some(leaf);
    while let Some(current) = cursor {
        if current == root {
            return true;
        }
        cursor = session.view(current).and_then(|view| view.parent);
    }
    false
}

/// The level a tab reopens on: its remembered leaf while that view still hangs
/// under the same root, otherwise the root itself.
fn resolve_leaf(session: &Session, leaves: &HashMap<ViewId, ViewId>, root: ViewId) -> ViewId {
    leaves
        .get(&root)
        .copied()
        .filter(|leaf| under_root(session, *leaf, root))
        .unwrap_or(root)
}

/// One tab per document: opening a file name reuses its existing tab.
fn doc_root(session: &Session, doc: DocId) -> Option<ViewId> {
    session.roots().iter().copied().find(|root| {
        matches!(
            session.view(*root).map(|view| &view.base),
            Some(Base::Doc(id)) if *id == doc
        )
    })
}

/// Compact form of the derived levels shown on the active tab.
fn stack_label(session: &Session, path: &[ViewId]) -> String {
    if path.len() < 2 {
        return String::new();
    }
    let parts: Vec<String> = path[1..]
        .iter()
        .filter_map(|id| session.view(*id))
        .map(|view| view.title.clone())
        .collect();
    truncate_chars(&parts.join(" › "), 34)
}

/// Turns a literal token into a single filter term. Regex metacharacters are
/// escaped so a word such as `level=warn` or `a.b` is matched as text instead of
/// being reparsed as a level filter or a regex.
fn literal_query(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 3);
    out.push_str("re:");
    for ch in term.chars() {
        if matches!(
            ch,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | '^'
                | '$'
                | '#'
        ) {
            out.push('\\');
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push_str("\\s+");
        } else {
            out.push(ch);
        }
    }
    out
}

fn module_query(module: &str) -> String {
    if module.chars().any(char::is_whitespace) {
        format!("module:\"{}\"", module.replace('"', ""))
    } else {
        format!("module:{module}")
    }
}

fn floor_char(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn hit_snippet(text: &str, matcher: &SearchMatcher) -> String {
    let mut ranges = Vec::new();
    matcher.find_all(text, &mut ranges);
    let (start, end) = ranges.first().copied().unwrap_or((0, 0));
    let mut from = start.saturating_sub(36);
    while from > 0 && !text.is_char_boundary(from) {
        from -= 1;
    }
    let mut to = (end + 140).min(text.len());
    while to < text.len() && !text.is_char_boundary(to) {
        to += 1;
    }
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.push_str(text[from..to].trim_end());
    if to < text.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_log(tag: &str, content: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "neolog-{tag}-{}-{}.log",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn appends_are_not_rewrites() {
        let path = temp_log("append", "a\nb\nc\n");
        let before = file_stamp(&path).unwrap();
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        let after = file_stamp(&path).unwrap();
        assert_eq!(
            change_kind(&path, &before, &after),
            Some(ChangeKind::Append)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn same_size_rewrite_and_rotation_are_rewrites() {
        let path = temp_log("rotate", "2026 one\n2026 two\n");
        let before = file_stamp(&path).unwrap();
        std::fs::write(&path, "2026 one\n2026 TWO\n").unwrap();
        let after = file_stamp(&path).unwrap();
        assert_eq!(
            change_kind(&path, &before, &after),
            Some(ChangeKind::Rewrite)
        );

        let rotated = path.with_extension("1.log");
        std::fs::rename(&path, &rotated).unwrap();
        std::fs::write(&path, "2026 three\n").unwrap();
        let fresh = file_stamp(&path).unwrap();
        assert_eq!(
            change_kind(&path, &after, &fresh),
            Some(ChangeKind::Rewrite)
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rotated);
    }

    #[test]
    fn unchanged_stamp_reports_nothing() {
        let path = temp_log("stable", "same\n");
        let stamp = file_stamp(&path).unwrap();
        assert_eq!(change_kind(&path, &stamp, &stamp), None);
        let missing =
            std::env::temp_dir().join(format!("neolog-missing-{}.log", std::process::id()));
        assert!(file_stamp(&missing).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn same_length_tail_change_is_rewrite() {
        let before = FileStamp {
            len: 8000,
            modified: None,
            created: None,
            head: 1,
            tail: 2,
        };
        let after = FileStamp {
            len: 8000,
            modified: None,
            created: None,
            head: 1,
            tail: 9,
        };
        assert_eq!(
            change_kind(Path::new("x.log"), &before, &after),
            Some(ChangeKind::Rewrite)
        );
    }

    #[test]
    fn globs_match_rotated_names() {
        assert!(glob_match("app.log", "*.log"));
        assert!(glob_match("app.LOG", "*.log"));
        assert!(glob_match("app.log.1", "*.log.*"));
        assert!(!glob_match("notes.txt.bak", "*.log"));
        assert_eq!(
            split_globs("*.log, *.txt;*.log.*"),
            vec!["*.log", "*.txt", "*.log.*"]
        );
    }

    #[test]
    fn literal_query_escapes_dsl_and_regex_syntax() {
        assert_eq!(literal_query("level=warn"), "re:level=warn");
        assert_eq!(literal_query("a.b(c)"), "re:a\\.b\\(c\\)");
        assert_eq!(literal_query("two words"), "re:two\\s+words");
        let terms = neolog_core::filter::parse_query(&literal_query("level=warn")).unwrap();
        assert!(terms.only_atom().is_some());
        assert!(neolog_core::filter::parse_query(&literal_query("a.b(c)"))
            .unwrap()
            .only_atom()
            .is_some());
    }

    #[test]
    fn module_query_quotes_spaces() {
        assert_eq!(module_query("planner"), "module:planner");
        assert_eq!(module_query("my node"), "module:\"my node\"");
    }

    #[test]
    fn tab_reopens_remembered_leaf_and_falls_back() {
        let path = temp_log(
            "tabs",
            "2026-01-01 00:00:00 INFO a\n2026-01-01 00:00:01 WARN b\n",
        );
        let mut session = Session::new(PluginRegistry::new(builtin_plugins()));
        let doc = session.open_path(&path).unwrap();
        let root = session.create_doc_view(doc).unwrap();
        let child = session.fork_view(root, "level=warn").unwrap();
        let grand = session.fork_view(child, "module:planner").unwrap();
        let mut leaves = HashMap::new();

        assert_eq!(resolve_leaf(&session, &leaves, root), root);
        leaves.insert(root, grand);
        assert_eq!(resolve_leaf(&session, &leaves, root), grand);
        assert_eq!(
            stack_label(&session, &session.view_path(grand)),
            "level=warn › module:planner"
        );

        session.remove_view(grand);
        assert_eq!(resolve_leaf(&session, &leaves, root), root);

        let other = session.create_doc_view(doc).unwrap();
        assert_eq!(doc_root(&session, doc), Some(root));
        session.remove_view(root);
        assert_eq!(doc_root(&session, doc), Some(other));
        leaves.insert(other, child);
        assert_eq!(resolve_leaf(&session, &leaves, other), other);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn child_views_stay_listed_under_the_parent() {
        let path = temp_log(
            "subtabs",
            "2026-01-01 00:00:00 INFO a\n2026-01-01 00:00:01 WARN b\n",
        );
        let mut session = Session::new(PluginRegistry::new(builtin_plugins()));
        let doc = session.open_path(&path).unwrap();
        let root = session.create_doc_view(doc).unwrap();
        let child = session.fork_view(root, "level=warn").unwrap();
        let other = session.fork_view(root, "level=error").unwrap();
        let grand = session.fork_view(child, "module:planner").unwrap();

        assert_eq!(view_tabs(&session, root), vec![root, child, grand, other]);
        assert_eq!(depth_under(&session, root, root), 0);
        assert_eq!(depth_under(&session, child, root), 1);
        assert_eq!(depth_under(&session, grand, root), 2);
        assert_eq!(session.view(root).unwrap().children, vec![child, other]);

        let _ = std::fs::remove_file(&path);
    }
}
