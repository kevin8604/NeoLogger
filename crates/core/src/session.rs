use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::bits::Bitmap;
use crate::filter::{parse_query, MatchCtx, Query};
use crate::mcap::{self, HistoryKind};
use crate::merge::merge_docs;
use crate::plugin::{LogPlugin, PluginRegistry};
use crate::source::{DocId, LineRef, LogDoc, TextEncoding};
use crate::style::{HighlightRule, RowStyle};
use crate::time;

pub type ViewId = u32;

#[derive(Clone, Debug)]
pub enum Base {
    Doc(DocId),
    Merged(Vec<DocId>),
    View(ViewId),
}

/// How the caller classified the on-disk change when reloading a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadMode {
    /// The file kept its prefix and grew; existing line numbers stay valid.
    Append,
    /// The file was truncated, replaced or rotated; line anchors are invalid.
    Rewrite,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReloadReport {
    pub lines: usize,
    pub dropped_bookmarks: usize,
}

#[derive(Clone, Debug)]
pub enum BitsSource {
    Doc(DocId),
    List(Arc<Vec<LineRef>>),
    Parent(Arc<Rows>),
}

impl BitsSource {
    fn get(&self, idx: usize) -> Option<LineRef> {
        match self {
            BitsSource::Doc(doc) => Some(LineRef::new(*doc, idx)),
            BitsSource::List(list) => list.get(idx).copied(),
            BitsSource::Parent(rows) => Some(rows.get_unchecked(idx)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BitRows {
    pub source: BitsSource,
    pub bits: Arc<Bitmap>,
}

impl BitRows {
    pub fn get(&self, idx: usize) -> Option<LineRef> {
        self.source.get(self.bits.select(idx)?)
    }
}

#[derive(Clone, Debug)]
pub enum Rows {
    /// Zero-copy view over every line of one document.
    Doc(DocId),
    /// Materialized line references (merged views).
    List(Arc<Vec<LineRef>>),
    /// Bitmap over the parent rows: one bit per candidate, ~1/64 the memory.
    Bits(BitRows),
}

impl Rows {
    pub fn len(&self, docs: &[LogDoc]) -> usize {
        match self {
            Rows::Doc(doc) => docs.get(*doc as usize).map_or(0, |d| d.len()),
            Rows::List(list) => list.len(),
            Rows::Bits(rows) => rows.bits.count_ones(),
        }
    }

    pub fn is_empty(&self, docs: &[LogDoc]) -> bool {
        self.len(docs) == 0
    }

    pub fn get(&self, idx: usize, _docs: &[LogDoc]) -> LineRef {
        match self {
            Rows::Doc(doc) => LineRef::new(*doc, idx),
            Rows::List(list) => list[idx],
            Rows::Bits(rows) => rows.get(idx).expect("bitmap row index out of range"),
        }
    }

    fn get_unchecked(&self, idx: usize) -> LineRef {
        match self {
            Rows::Doc(doc) => LineRef::new(*doc, idx),
            Rows::List(list) => list[idx],
            Rows::Bits(rows) => rows.get(idx).expect("bitmap row index out of range"),
        }
    }
}

pub struct ViewNode {
    pub id: ViewId,
    pub title: String,
    pub note: String,
    pub query: String,
    pub base: Base,
    pub filters: Query,
    pub rows: Rows,
    pub parent: Option<ViewId>,
    pub children: Vec<ViewId>,
}

pub struct Session {
    pub(crate) plugins: PluginRegistry,
    pub(crate) docs: Vec<LogDoc>,
    pub(crate) views: Vec<Option<ViewNode>>,
    pub(crate) roots: Vec<ViewId>,
    pub(crate) bookmarks: BTreeMap<DocId, BTreeSet<u32>>,
    pub(crate) rules: Vec<HighlightRule>,
    pub(crate) next_view: ViewId,
    pub(crate) next_rule: u64,
    revision: u64,
    structure_rev: u64,
}

impl Session {
    pub fn new(plugins: PluginRegistry) -> Self {
        Self {
            plugins,
            docs: Vec::new(),
            views: Vec::new(),
            roots: Vec::new(),
            bookmarks: BTreeMap::new(),
            rules: Vec::new(),
            next_view: 0,
            next_rule: 0,
            revision: 0,
            structure_rev: 0,
        }
    }

    /// Bumped whenever rows or line text can change. UI caches (wrap layout,
    /// galley metrics) key on it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Bumped with every session mutation, including views, bookmarks and reloads.
    /// The app autosaves when this moves.
    pub fn structure_rev(&self) -> u64 {
        self.structure_rev
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.structure_rev = self.structure_rev.wrapping_add(1);
    }

    pub fn plugins(&self) -> &PluginRegistry {
        &self.plugins
    }

    pub fn docs(&self) -> &[LogDoc] {
        &self.docs
    }

    pub fn doc(&self, id: DocId) -> Option<&LogDoc> {
        self.docs.get(id as usize)
    }

    pub fn view(&self, id: ViewId) -> Option<&ViewNode> {
        self.views.get(id as usize).and_then(|v| v.as_ref())
    }

    pub fn roots(&self) -> &[ViewId] {
        &self.roots
    }

    pub fn views(&self) -> impl Iterator<Item = &ViewNode> {
        self.views.iter().flatten()
    }

    pub fn doc_ids(&self) -> impl Iterator<Item = DocId> {
        0..self.docs.len() as DocId
    }

    pub fn view_path(&self, id: ViewId) -> Vec<ViewId> {
        let mut path = Vec::new();
        let mut cursor = Some(id);
        while let Some(current) = cursor {
            path.push(current);
            cursor = self.view(current).and_then(|v| v.parent);
        }
        path.reverse();
        path
    }

    pub fn view_docs(&self, id: ViewId) -> Vec<DocId> {
        let mut cursor = Some(id);
        while let Some(current) = cursor {
            let Some(node) = self.view(current) else {
                return Vec::new();
            };
            match &node.base {
                Base::Doc(doc) => return vec![*doc],
                Base::Merged(docs) => return docs.clone(),
                Base::View(parent) => cursor = Some(*parent),
            }
        }
        Vec::new()
    }

    pub fn total_lines(&self) -> usize {
        self.docs.iter().map(|d| d.len()).sum()
    }

    pub fn open_path(&mut self, path: impl AsRef<Path>) -> io::Result<DocId> {
        self.open_path_with(path, None)
    }

    pub fn open_path_with(
        &mut self,
        path: impl AsRef<Path>,
        encoding: Option<TextEncoding>,
    ) -> io::Result<DocId> {
        let id = self.docs.len() as DocId;
        let mut doc = LogDoc::open_with(id, path.as_ref(), encoding)?;
        let plugin = {
            let sample = doc.sample_lines(200);
            self.plugins.detect(path.as_ref(), &sample)
        };
        apply_plugin(&mut doc, plugin.as_ref());
        self.docs.push(doc);
        self.touch();
        Ok(id)
    }

    pub fn open_text(&mut self, name: &str, content: &str) -> DocId {
        let id = self.docs.len() as DocId;
        let mut doc = LogDoc::from_string(id, name, content);
        let plugin = {
            let sample = doc.sample_lines(200);
            self.plugins.detect(Path::new(name), &sample)
        };
        apply_plugin(&mut doc, plugin.as_ref());
        self.docs.push(doc);
        self.touch();
        id
    }

    /// Re-maps a document from disk, keeping its id, plugin and day base.
    /// Callers classify the on-disk change from file metadata: an append keeps
    /// line numbers meaningful, while a rewrite (truncation or rotation) drops
    /// every bookmark for that document. Views are rebuilt afterwards.
    pub fn reload_doc(&mut self, doc_id: DocId, mode: ReloadMode) -> io::Result<ReloadReport> {
        let Some(old) = self.docs.get(doc_id as usize) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "文档不存在"));
        };
        let path = old.path.clone();
        let plugin_id = old.plugin_id.clone();
        let day_base = old.day_base;
        let encoding = old.encoding;
        let unmapped = old.unmapped;
        let mut doc = if unmapped {
            LogDoc::open_owned(doc_id, &path, Some(encoding))?
        } else {
            LogDoc::open_with(doc_id, &path, Some(encoding))?
        };
        doc.day_base = day_base;
        doc.missing = false;
        doc.unmapped = unmapped;
        let plugin = self.plugins.by_id(&plugin_id);
        apply_plugin(&mut doc, plugin.as_ref());
        let lines = doc.len();
        self.docs[doc_id as usize] = doc;
        let dropped_bookmarks = match mode {
            ReloadMode::Rewrite => self.bookmarks.remove(&doc_id).map_or(0, |set| set.len()),
            ReloadMode::Append => match self.bookmarks.get_mut(&doc_id) {
                Some(set) => {
                    let before = set.len();
                    set.retain(|line| (*line as usize) < lines);
                    before - set.len()
                }
                None => 0,
            },
        };
        self.rebuild_all();
        self.rebuild_mcap_series();
        self.touch();
        Ok(ReloadReport {
            lines,
            dropped_bookmarks,
        })
    }

    /// Re-decodes one file with an explicit encoding and rebuilds its timeline.
    pub fn set_doc_encoding(&mut self, doc_id: DocId, encoding: TextEncoding) -> io::Result<()> {
        let Some(old) = self.docs.get(doc_id as usize) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "文档不存在"));
        };
        if old.missing || !old.path.is_file() {
            if let Some(slot) = self.docs.get_mut(doc_id as usize) {
                slot.encoding = encoding;
            }
            self.touch();
            return Ok(());
        }
        let path = old.path.clone();
        let plugin_id = old.plugin_id.clone();
        let day_base = old.day_base;
        let unmapped = old.unmapped;
        let mut doc = if unmapped {
            LogDoc::open_owned(doc_id, &path, Some(encoding))?
        } else {
            LogDoc::open_with(doc_id, &path, Some(encoding))?
        };
        doc.day_base = day_base;
        doc.unmapped = unmapped;
        let plugin = self.plugins.by_id(&plugin_id);
        apply_plugin(&mut doc, plugin.as_ref());
        let lines = doc.len();
        self.docs[doc_id as usize] = doc;
        if let Some(set) = self.bookmarks.get_mut(&doc_id) {
            set.retain(|line| (*line as usize) < lines);
            if set.is_empty() {
                self.bookmarks.remove(&doc_id);
            }
        }
        self.rebuild_all();
        self.touch();
        Ok(())
    }

    /// Marks a previously opened file as unavailable without dropping its views.
    pub fn mark_unavailable(&mut self, doc_id: DocId) -> bool {
        let Some(doc) = self.docs.get_mut(doc_id as usize) else {
            return false;
        };
        if doc.missing || !doc.path.is_absolute() {
            return false;
        }
        doc.missing = true;
        self.touch();
        true
    }

    /// Drops the file mapping and re-reads into memory so rotation can replace it.
    pub fn unpin_doc(&mut self, doc_id: DocId) -> io::Result<ReloadReport> {
        let Some(doc) = self.docs.get_mut(doc_id as usize) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "文档不存在"));
        };
        if doc.missing || !doc.path.is_file() {
            return Ok(ReloadReport {
                lines: doc.len(),
                dropped_bookmarks: 0,
            });
        }
        if doc.unmapped {
            return Ok(ReloadReport {
                lines: doc.len(),
                dropped_bookmarks: 0,
            });
        }
        doc.unmapped = true;
        self.reload_doc(doc_id, ReloadMode::Append)
    }

    /// Adds a document to an existing time-merged view and rebuilds it.
    pub fn append_merged_doc(&mut self, view: ViewId, doc: DocId) -> bool {
        if self.docs.get(doc as usize).is_none() {
            return false;
        }
        let added = {
            let Some(node) = self
                .views
                .get_mut(view as usize)
                .and_then(|slot| slot.as_mut())
            else {
                return false;
            };
            let Base::Merged(docs) = &mut node.base else {
                return false;
            };
            if docs.contains(&doc) {
                return false;
            }
            docs.push(doc);
            true
        };
        if added {
            self.recompute_subtree(view);
            self.touch();
        }
        added
    }

    pub(crate) fn refresh_doc_times(&mut self, doc_id: DocId) {
        let Some(plugin_id) = self
            .docs
            .get(doc_id as usize)
            .map(|doc| doc.plugin_id.clone())
        else {
            return;
        };
        let plugin = self.plugins.by_id(&plugin_id);
        if let Some(doc) = self.docs.get_mut(doc_id as usize) {
            apply_plugin(doc, plugin.as_ref());
        }
        self.rebuild_all();
        self.touch();
    }

    pub(crate) fn push_unavailable(
        &mut self,
        path: impl Into<std::path::PathBuf>,
        plugin_id: &str,
        day_base: i64,
        encoding: Option<TextEncoding>,
    ) -> DocId {
        let id = self.docs.len() as DocId;
        let mut doc = LogDoc::missing(id, path);
        doc.day_base = day_base;
        doc.plugin_id = plugin_id.to_string();
        if let Some(encoding) = encoding {
            doc.encoding = encoding;
        }
        self.docs.push(doc);
        self.touch();
        id
    }

    pub fn set_doc_plugin(&mut self, doc_id: DocId, plugin_id: &str) -> bool {
        let plugin = self.plugins.all().find(|p| p.id() == plugin_id).cloned();
        let Some(plugin) = plugin else {
            return false;
        };
        let Some(doc) = self.docs.get_mut(doc_id as usize) else {
            return false;
        };
        apply_plugin(doc, plugin.as_ref());
        self.rebuild_all();
        self.touch();
        true
    }

    /// Borrows the plugin without cloning the `Arc`; used on per-row hot paths.
    pub fn plugin_for(&self, doc: DocId) -> &dyn LogPlugin {
        self.docs.get(doc as usize).map_or_else(
            || self.plugins.fallback_ref(),
            |d| self.plugins.get(&d.plugin_id),
        )
    }

    pub fn plugin_id_of(&self, doc: DocId) -> &str {
        self.docs
            .get(doc as usize)
            .map(|d| d.plugin_id.as_str())
            .unwrap_or("plain")
    }

    pub fn create_doc_view(&mut self, doc: DocId) -> Option<ViewId> {
        let doc_ref = self.docs.get(doc as usize)?;
        let id = self.next_view;
        let node = ViewNode {
            id,
            title: doc_ref.short_name(),
            note: String::new(),
            query: String::new(),
            base: Base::Doc(doc),
            filters: Query::all(),
            rows: Rows::Doc(doc),
            parent: None,
            children: Vec::new(),
        };
        self.push_view(node);
        self.roots.push(id);
        if self
            .docs
            .get(doc as usize)
            .and_then(|doc| doc.mcap())
            .is_some()
        {
            if self.fork_view(id, "field:log=1").is_ok() {
                if let Some(child) = self.view(id).and_then(|view| view.children.last().copied()) {
                    self.set_view_meta(child, "日志", "日志型通道");
                }
            }
        }
        self.touch();
        Some(id)
    }

    /// Opens the history of a topic, a publisher node, or one property.
    /// Property histories are change-only lines appended after the messages.
    /// Asking for the same history again returns the existing child view.
    pub fn open_mcap_history(&mut self, root: ViewId, kind: HistoryKind) -> Result<ViewId, String> {
        let query = kind.query();
        if let Some(existing) = self.find_query(root, &query) {
            return Ok(existing);
        }
        let doc = self
            .root_doc(root)
            .ok_or_else(|| "这个标签没有 MCAP 文档".to_string())?;
        if self
            .docs
            .get(doc as usize)
            .and_then(|doc| doc.mcap())
            .is_none()
        {
            return Err("当前文档不是 MCAP".to_string());
        }
        let truncated = if let Some(key) = kind.series_key() {
            self.materialize_series(doc, &key)?
        } else {
            false
        };
        let id = self.fork_view(root, &query)?;
        let note = if truncated {
            "原始采样已保留前 10000 行"
        } else {
            ""
        };
        self.set_view_meta(id, &kind.title(), note);
        Ok(id)
    }

    fn root_doc(&self, root: ViewId) -> Option<DocId> {
        match self.view(root).map(|view| &view.base) {
            Some(Base::Doc(doc)) => Some(*doc),
            _ => None,
        }
    }

    fn find_query(&self, root: ViewId, query: &str) -> Option<ViewId> {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let Some(node) = self.view(id) else {
                continue;
            };
            if id != root && node.query == query {
                return Some(id);
            }
            stack.extend(node.children.iter().copied());
        }
        None
    }

    fn materialize_series(&mut self, doc: DocId, key: &str) -> Result<bool, String> {
        let mut keys = self
            .docs
            .get(doc as usize)
            .and_then(|document| document.mcap())
            .map(|catalog| catalog.series.clone())
            .ok_or_else(|| "当前文档不是 MCAP".to_string())?;
        if !keys.iter().any(|existing| existing == key) {
            keys.push(key.to_string());
        }
        Ok(self.rewrite_series(doc, &keys))
    }

    /// Rebuilds property-history lines after a reload or session restore.
    pub(crate) fn rebuild_mcap_series(&mut self) {
        let mut by_doc: BTreeMap<DocId, Vec<String>> = BTreeMap::new();
        let views: Vec<(ViewId, String)> = self
            .views
            .iter()
            .flatten()
            .filter(|view| view.query.starts_with("field:series="))
            .map(|view| (view.id, view.query.clone()))
            .collect();
        for (id, query) in views {
            let Some(key) = query.strip_prefix("field:series=") else {
                continue;
            };
            let Some(root) = self.view_path(id).first().copied() else {
                continue;
            };
            let Some(doc) = self.root_doc(root) else {
                continue;
            };
            if self
                .docs
                .get(doc as usize)
                .and_then(|document| document.mcap())
                .is_none()
            {
                continue;
            }
            let list = by_doc.entry(doc).or_default();
            if !list.iter().any(|existing| existing == key) {
                list.push(key.to_string());
            }
        }
        for (doc, keys) in by_doc {
            self.rewrite_series(doc, &keys);
        }
    }

    fn rewrite_series(&mut self, doc: DocId, keys: &[String]) -> bool {
        if let Some(document) = self.docs.get_mut(doc as usize) {
            document.retain_message_prefix();
            if let Some(catalog) = document.mcap.as_mut() {
                catalog.series = keys.to_vec();
            }
        }
        let message_lines = self
            .docs
            .get(doc as usize)
            .and_then(|document| document.mcap())
            .map(|catalog| catalog.message_lines)
            .unwrap_or(0);
        let lines: Vec<String> = (0..message_lines)
            .filter_map(|idx| {
                self.docs
                    .get(doc as usize)
                    .map(|document| document.line_text(idx).to_string())
            })
            .collect();
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut extra = String::new();
        let mut truncated = false;
        for series in keys {
            let Some((topic, field, raw)) = split_series(series) else {
                continue;
            };
            let (text, cut) = mcap::project_history(&borrowed, &topic, &field, raw);
            truncated |= cut;
            extra.push_str(&text);
        }
        if let Some(document) = self.docs.get_mut(doc as usize) {
            document.append_owned(&extra);
        }
        if let Some(set) = self.bookmarks.get_mut(&doc) {
            set.retain(|line| (*line as usize) < message_lines);
            if set.is_empty() {
                self.bookmarks.remove(&doc);
            }
        }
        let plugin = self.plugin_for_id(doc);
        if let Some(document) = self.docs.get_mut(doc as usize) {
            apply_plugin(document, plugin.as_ref());
        }
        self.rebuild_all();
        self.touch();
        truncated
    }

    fn plugin_for_id(&self, doc: DocId) -> Arc<dyn LogPlugin> {
        self.docs.get(doc as usize).map_or_else(
            || self.plugins.fallback(),
            |document| self.plugins.by_id(&document.plugin_id),
        )
    }

    pub fn create_merged_view(
        &mut self,
        docs: &[DocId],
        title: impl Into<String>,
    ) -> Result<ViewId, String> {
        if docs.is_empty() {
            return Err("没有选择要合并的日志文件".to_string());
        }
        let mut seen = BTreeSet::new();
        let mut ordered = Vec::new();
        for id in docs {
            if self.docs.get(*id as usize).is_none() {
                return Err(format!("文档 {id} 不存在"));
            }
            if seen.insert(*id) {
                ordered.push(*id);
            }
        }
        let base = Base::Merged(ordered.clone());
        let rows = self.compute_rows(&base, &Query::all());
        let title = title.into();
        let title = if title.is_empty() {
            format!("合并 · {} 个文件", ordered.len())
        } else {
            title
        };
        let id = self.next_view;
        let node = ViewNode {
            id,
            title,
            note: String::new(),
            query: String::new(),
            base,
            filters: Query::all(),
            rows,
            parent: None,
            children: Vec::new(),
        };
        self.push_view(node);
        self.roots.push(id);
        self.touch();
        Ok(id)
    }

    pub fn fork_view(&mut self, parent: ViewId, query: &str) -> Result<ViewId, String> {
        let parent_node = self
            .view(parent)
            .ok_or_else(|| "父视图不存在".to_string())?;
        let filters = parse_query(query)?;
        let base = Base::View(parent);
        let rows = self.compute_rows(&base, &filters);
        let trimmed = query.trim();
        let title = if trimmed.is_empty() {
            format!("{} · 全部", parent_node.title)
        } else {
            truncate(trimmed, 22)
        };
        let id = self.next_view;
        let node = ViewNode {
            id,
            title,
            note: String::new(),
            query: trimmed.to_string(),
            base,
            filters,
            rows,
            parent: Some(parent),
            children: Vec::new(),
        };
        self.push_view(node);
        if let Some(parent_node) = self.views[parent as usize].as_mut() {
            parent_node.children.push(id);
        }
        self.touch();
        Ok(id)
    }

    pub fn update_view(&mut self, id: ViewId, query: &str) -> Result<(), String> {
        if self.view(id).is_none() {
            return Err("视图不存在".to_string());
        }
        let filters = parse_query(query)?;
        {
            let node = self.views[id as usize].as_mut().unwrap();
            node.query = query.trim().to_string();
            node.filters = filters;
        }
        self.recompute_subtree(id);
        self.touch();
        Ok(())
    }

    /// Renames a view and replaces its note. An empty title restores the
    /// automatic name derived from the base and query.
    pub fn set_view_meta(&mut self, id: ViewId, title: &str, note: &str) -> bool {
        let Some(node) = self.view(id) else {
            return false;
        };
        let title = if title.trim().is_empty() {
            self.default_view_title(node)
        } else {
            title.trim().to_string()
        };
        let note = note.trim().to_string();
        let Some(node) = self.views[id as usize].as_mut() else {
            return false;
        };
        node.title = title;
        node.note = note;
        self.touch();
        true
    }

    fn default_view_title(&self, node: &ViewNode) -> String {
        if !node.query.is_empty() {
            return node.query.clone();
        }
        match &node.base {
            Base::Doc(doc) => self
                .doc(*doc)
                .map(|doc| doc.short_name())
                .unwrap_or_else(|| "文档".to_string()),
            Base::Merged(docs) => format!("合并 · {} 个文件", docs.len()),
            Base::View(parent) => self
                .view(*parent)
                .map(|parent_node| format!("{} · 全部", parent_node.title))
                .unwrap_or_else(|| "全部".to_string()),
        }
    }

    /// Removes a filtered view but keeps its descendants: they are reparented
    /// to the removed view's parent and recomputed against the new base.
    /// Roots are rejected because their children would lose the document base.
    pub fn remove_view_level(&mut self, id: ViewId) -> bool {
        let Some(node) = self.view(id) else {
            return false;
        };
        let Some(parent) = node.parent else {
            return false;
        };
        let children = node.children.clone();
        let parent_title = self
            .view(parent)
            .map(|parent_node| parent_node.title.clone())
            .unwrap_or_default();
        let position = self.views[parent as usize]
            .as_ref()
            .and_then(|parent_node| parent_node.children.iter().position(|c| *c == id));
        let Some(position) = position else {
            return false;
        };
        if let Some(parent_node) = self.views[parent as usize].as_mut() {
            parent_node
                .children
                .splice(position..=position, children.iter().copied());
        }
        for child in &children {
            if let Some(child_node) = self.views[*child as usize].as_mut() {
                child_node.parent = Some(parent);
                if matches!(child_node.base, Base::View(old) if old == id) {
                    child_node.base = Base::View(parent);
                }
                if child_node.query.is_empty() {
                    child_node.title = format!("{parent_title} · 全部");
                }
            }
        }
        self.views[id as usize] = None;
        for child in &children {
            self.recompute_subtree(*child);
        }
        self.touch();
        true
    }

    pub fn remove_view(&mut self, id: ViewId) {
        let Some(node) = self.view(id) else { return };
        let parent = node.parent;

        let mut stack = vec![id];
        let mut removed = Vec::new();
        while let Some(current) = stack.pop() {
            if let Some(node) = self.view(current) {
                stack.extend(node.children.iter().copied());
            }
            removed.push(current);
        }

        if let Some(parent) = parent {
            if let Some(node) = self.views[parent as usize].as_mut() {
                node.children.retain(|child| *child != id);
            }
        } else {
            self.roots.retain(|root| *root != id);
        }
        for view in removed {
            self.views[view as usize] = None;
        }
        self.touch();
    }

    pub fn visible_len(&self, view: ViewId) -> usize {
        self.view(view).map_or(0, |v| v.rows.len(&self.docs))
    }

    pub fn line_ref(&self, view: ViewId, idx: usize) -> Option<LineRef> {
        let node = self.view(view)?;
        (idx < node.rows.len(&self.docs)).then(|| node.rows.get(idx, &self.docs))
    }

    pub fn line_text(&self, lr: LineRef) -> &str {
        let doc = &self.docs[lr.doc as usize];
        doc.line_text(lr.line as usize)
    }

    /// Text of the whole event, one physical line per row.
    pub fn event_text(&self, lr: LineRef) -> String {
        let Some(doc) = self.doc(lr.doc) else {
            return String::new();
        };
        let (start, end) = doc.event_bounds(lr.line as usize);
        (start..end)
            .map(|line| doc.line_text(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn event_len(&self, lr: LineRef) -> usize {
        self.doc(lr.doc)
            .map(|doc| {
                let (start, end) = doc.event_bounds(lr.line as usize);
                end.saturating_sub(start)
            })
            .unwrap_or(0)
    }

    pub fn line_time(&self, lr: LineRef) -> Option<i64> {
        self.doc(lr.doc).and_then(|d| d.time_at(lr.line as usize))
    }

    pub fn time_range(&self, view: ViewId) -> Option<(i64, i64)> {
        let node = self.view(view)?;
        let len = node.rows.len(&self.docs);
        if len == 0 {
            return None;
        }
        let first = node.rows.get(0, &self.docs);
        let last = node.rows.get(len - 1, &self.docs);
        Some((self.line_time(first)?, self.line_time(last)?))
    }

    pub fn toggle_bookmark(&mut self, lr: LineRef) -> bool {
        let Some(doc) = self.docs.get(lr.doc as usize) else {
            return false;
        };
        if doc.is_empty() || lr.line as usize >= doc.len() {
            return false;
        }
        let (start, end) = doc.event_bounds(lr.line as usize);
        let on = self.is_bookmarked(lr);
        {
            let set = self.bookmarks.entry(lr.doc).or_default();
            if on {
                for line in start..end {
                    set.remove(&(line as u32));
                }
            } else {
                for line in start..end {
                    set.insert(line as u32);
                }
            }
            if set.is_empty() {
                self.bookmarks.remove(&lr.doc);
            }
        }
        self.recompute_bookmark_views();
        self.touch();
        !on
    }

    pub fn is_bookmarked(&self, lr: LineRef) -> bool {
        self.bookmarks
            .get(&lr.doc)
            .is_some_and(|set| set.contains(&lr.line))
    }

    pub fn bookmark_count(&self, doc: DocId) -> usize {
        self.bookmarks.get(&doc).map_or(0, |set| set.len())
    }

    pub fn bookmark_list(&self, doc: DocId) -> Vec<u32> {
        self.bookmarks
            .get(&doc)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn bookmarked_rows(&self, view: ViewId) -> Vec<usize> {
        let Some(node) = self.view(view) else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for idx in 0..node.rows.len(&self.docs) {
            if self.is_bookmarked(node.rows.get(idx, &self.docs)) {
                rows.push(idx);
            }
        }
        rows
    }

    pub fn row_index_of(&self, view: ViewId, lr: LineRef) -> Option<usize> {
        let node = self.view(view)?;
        let len = node.rows.len(&self.docs);
        match &node.rows {
            Rows::Doc(doc) if *doc == lr.doc => {
                let idx = lr.line as usize;
                (idx < len).then_some(idx)
            }
            Rows::Doc(_) => None,
            Rows::List(list) => list.iter().position(|r| *r == lr),
            Rows::Bits(rows) => (0..len).find(|idx| rows.get(*idx) == Some(lr)),
        }
    }

    pub fn add_rule(&mut self, mut rule: HighlightRule) -> u64 {
        self.next_rule += 1;
        rule.id = self.next_rule;
        let id = rule.id;
        self.rules.push(rule);
        self.touch();
        id
    }

    pub fn remove_rule(&mut self, id: u64) {
        self.rules.retain(|rule| rule.id != id);
        self.touch();
    }

    pub fn rules(&self) -> &[HighlightRule] {
        &self.rules
    }

    pub fn rules_for<'a>(&'a self, lr: LineRef) -> impl Iterator<Item = &'a HighlightRule> + 'a {
        let plugin_id = self.plugin_id_of(lr.doc);
        self.rules
            .iter()
            .filter(move |rule| rule.applies_to(lr.doc, plugin_id))
    }

    pub fn plugin_row_style(&self, lr: LineRef) -> RowStyle {
        let Some(doc) = self.doc(lr.doc) else {
            return RowStyle::default();
        };
        let plugin = self.plugins.by_id(&doc.plugin_id);
        plugin.row_style(doc.line_text(lr.line as usize))
    }

    fn push_view(&mut self, node: ViewNode) {
        debug_assert_eq!(node.id as usize, self.views.len());
        self.views.push(Some(node));
        self.next_view = self.views.len() as ViewId;
    }

    fn rebuild_all(&mut self) {
        let roots = self.roots.clone();
        for root in roots {
            self.recompute_subtree(root);
        }
    }

    fn recompute_subtree(&mut self, id: ViewId) {
        let Some(node) = self.view(id) else { return };
        let base = node.base.clone();
        let filters = node.filters.clone();
        let children = node.children.clone();
        let rows = self.compute_rows(&base, &filters);
        if let Some(node) = self.views[id as usize].as_mut() {
            node.rows = rows;
        }
        for child in children {
            self.recompute_subtree(child);
        }
    }

    pub(crate) fn recompute_bookmark_views(&mut self) {
        let ids: Vec<ViewId> = self.views.iter().flatten().map(|view| view.id).collect();
        for id in ids {
            let Some(node) = self.view(id) else {
                continue;
            };
            if !node.filters.uses_bookmarks() {
                continue;
            }
            let parent_uses = node.parent.is_some_and(|parent| {
                self.view(parent)
                    .is_some_and(|parent| parent.filters.uses_bookmarks())
            });
            if !parent_uses {
                self.recompute_subtree(id);
            }
        }
    }

    fn plugin_table(&self) -> Vec<Arc<dyn LogPlugin>> {
        self.docs
            .iter()
            .map(|doc| self.plugins.by_id(&doc.plugin_id))
            .collect()
    }

    pub(crate) fn compute_rows(&self, base: &Base, query: &Query) -> Rows {
        match base {
            Base::Doc(doc) => {
                if query.is_all() {
                    return Rows::Doc(*doc);
                }
                let Some(d) = self.docs.get(*doc as usize) else {
                    return Rows::List(Arc::new(Vec::new()));
                };
                let plugins = self.plugin_table();
                let bits = self.filter_bitmap(d.len(), |i| LineRef::new(*doc, i), query, &plugins);
                Rows::Bits(BitRows {
                    source: BitsSource::Doc(*doc),
                    bits: Arc::new(bits),
                })
            }
            Base::Merged(docs) => {
                let refs: Vec<&LogDoc> = docs
                    .iter()
                    .filter_map(|id| self.docs.get(*id as usize))
                    .collect();
                let merged = Arc::new(merge_docs(&refs));
                if query.is_all() {
                    return Rows::List(merged);
                }
                let plugins = self.plugin_table();
                let bits = self.filter_bitmap(merged.len(), |i| merged[i], query, &plugins);
                Rows::Bits(BitRows {
                    source: BitsSource::List(merged),
                    bits: Arc::new(bits),
                })
            }
            Base::View(parent) => {
                let Some(parent_node) = self.view(*parent) else {
                    return Rows::List(Arc::new(Vec::new()));
                };
                if query.is_all() {
                    return parent_node.rows.clone();
                }
                let parent_rows = Arc::new(parent_node.rows.clone());
                let total = parent_rows.len(&self.docs);
                let plugins = self.plugin_table();
                let bits =
                    self.filter_bitmap(total, |i| parent_rows.get(i, &self.docs), query, &plugins);
                Rows::Bits(BitRows {
                    source: BitsSource::Parent(parent_rows),
                    bits: Arc::new(bits),
                })
            }
        }
    }

    fn filter_bitmap(
        &self,
        total: usize,
        mut lr_at: impl FnMut(usize) -> LineRef,
        query: &Query,
        plugins: &[Arc<dyn LogPlugin>],
    ) -> Bitmap {
        let refs: Vec<LineRef> = (0..total).map(&mut lr_at).collect();
        let needs_bookmarks = query.uses_bookmarks();
        let mut hits = Vec::new();
        for (idx, lr) in refs.iter().copied().enumerate() {
            let Some(doc) = self.docs.get(lr.doc as usize) else {
                continue;
            };
            let line = lr.line as usize;
            if line >= doc.len() {
                continue;
            }
            let ctx = MatchCtx {
                text: doc.line_text(line),
                plugin: plugins[lr.doc as usize].as_ref(),
                time: doc.time_at(line).unwrap_or(0),
                bookmarked: needs_bookmarks && self.is_bookmarked(lr),
            };
            if query.matches(&ctx) {
                hits.push(idx);
            }
        }
        let mut marked = vec![false; total];
        for hit in hits {
            let from = hit.saturating_sub(query.around);
            let to = (hit + query.around + 1).min(total);
            for idx in from..to {
                marked[idx] = true;
            }
        }
        let mut events = BTreeSet::new();
        for (idx, lr) in refs.iter().copied().enumerate() {
            if !marked[idx] {
                continue;
            }
            let Some(doc) = self.docs.get(lr.doc as usize) else {
                continue;
            };
            let (start, _) = doc.event_bounds(lr.line as usize);
            events.insert((lr.doc, start as u32));
        }
        let mut bits = Bitmap::new(total);
        for (idx, lr) in refs.into_iter().enumerate() {
            let Some(doc) = self.docs.get(lr.doc as usize) else {
                continue;
            };
            let (start, _) = doc.event_bounds(lr.line as usize);
            if events.contains(&(lr.doc, start as u32)) {
                bits.set(idx);
            }
        }
        bits.finish();
        bits
    }
}

fn apply_plugin(doc: &mut LogDoc, plugin: &dyn LogPlugin) {
    doc.plugin_id = plugin.id().to_string();
    let day_base = doc.day_base;
    let timeline =
        time::build_timeline(doc.len(), |i| plugin.timestamp(doc.line_text(i), day_base));
    doc.set_times(timeline.times);
    doc.set_owned(timeline.owned);
}

fn split_series(key: &str) -> Option<(String, String, bool)> {
    let raw = key.ends_with("#raw");
    let key = key.strip_suffix("#raw").unwrap_or(key);
    let (topic, field) = key.split_once('#')?;
    if topic.is_empty() || field.is_empty() {
        return None;
    }
    Some((topic.to_string(), field.to_string(), raw))
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PluginRegistry;

    fn session() -> Session {
        Session::new(PluginRegistry::new(vec![]))
    }

    const LOG_A: &str = "\
2026-10-31 10:00:00.000 INFO boot node_a
2026-10-31 10:00:02.000 ERROR camera timeout
2026-10-31 10:00:04.000 INFO ready
";

    const LOG_B: &str = "\
2026-10-31 10:00:01.000 INFO boot node_b
2026-10-31 10:00:03.000 WARN battery low
2026-10-31 10:00:05.000 ERROR lidar lost
";

    #[test]
    fn merged_view_is_time_ordered() {
        let mut s = session();
        let a = s.open_text("a.log", LOG_A);
        let b = s.open_text("b.log", LOG_B);
        let view = s.create_merged_view(&[a, b], "merged").unwrap();
        let lines: Vec<String> = (0..s.visible_len(view))
            .map(|i| s.line_text(s.line_ref(view, i).unwrap()).to_string())
            .collect();
        assert_eq!(lines.len(), 6);
        assert!(lines[0].contains("10:00:00"));
        assert!(lines[1].contains("10:00:01"));
        assert!(lines[5].contains("10:00:05"));
    }

    #[test]
    fn fork_filters_parent_rows() {
        let mut s = session();
        let a = s.open_text("a.log", LOG_A);
        let root = s.create_doc_view(a).unwrap();
        let child = s.fork_view(root, "level>=error").unwrap();
        assert_eq!(s.visible_len(child), 1);
        assert_eq!(s.visible_len(root), 3);
        let child_rows = s.view(child).unwrap().rows.clone();
        assert!(matches!(child_rows, Rows::Bits(_)));
        let grandchild = s.fork_view(child, "camera").unwrap();
        assert_eq!(s.visible_len(grandchild), 1);
        let empty = s.fork_view(grandchild, "lidar").unwrap();
        assert_eq!(s.visible_len(empty), 0);
        assert_eq!(s.view_path(empty), vec![root, child, grandchild, empty]);
    }

    #[test]
    fn bookmarks_are_stable_across_views() {
        let mut s = session();
        let a = s.open_text("a.log", LOG_A);
        let root = s.create_doc_view(a).unwrap();
        let lr = s.line_ref(root, 1).unwrap();
        assert!(s.toggle_bookmark(lr));
        let child = s.fork_view(root, "level=error").unwrap();
        assert_eq!(s.visible_len(child), 1);
        assert!(s.is_bookmarked(s.line_ref(child, 0).unwrap()));
        assert_eq!(s.bookmarked_rows(child), vec![0]);
        assert_eq!(s.bookmark_count(a), 1);
    }

    #[test]
    fn reload_keeps_or_drops_bookmarks_by_mode() {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "neolog-reload-{}-{}.log",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let mut s = session();
        let doc = s.open_path(&path).unwrap();
        let view = s.create_doc_view(doc).unwrap();
        assert!(s.toggle_bookmark(s.line_ref(view, 1).unwrap()));

        // Windows refuses to truncate a mapped file, so tail-like growth is
        // simulated with a real append.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"d\n").unwrap();
        drop(file);
        let report = s.reload_doc(doc, ReloadMode::Append).unwrap();
        assert_eq!(report.lines, 4);
        assert_eq!(report.dropped_bookmarks, 0);
        assert_eq!(s.bookmark_count(doc), 1);

        // Rename-style rotation keeps the old mapping writable on Windows.
        let backup = path.with_extension("old");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::write(&path, "only one line\n").unwrap();
        let report = s.reload_doc(doc, ReloadMode::Rewrite).unwrap();
        assert_eq!(report.lines, 1);
        assert_eq!(report.dropped_bookmarks, 1);
        assert_eq!(s.bookmark_count(doc), 0);
        assert_eq!(s.visible_len(view), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn updating_a_filter_recomputes_descendants() {
        let mut s = session();
        let a = s.open_text("a.log", LOG_A);
        let root = s.create_doc_view(a).unwrap();
        let child = s.fork_view(root, "level>=error").unwrap();
        let grandchild = s.fork_view(child, "camera").unwrap();
        assert_eq!(s.visible_len(grandchild), 1);
        s.update_view(child, "ready").unwrap();
        assert_eq!(s.visible_len(child), 1);
        assert_eq!(s.visible_len(grandchild), 0);
    }

    #[test]
    fn removing_view_removes_subtree() {
        let mut s = session();
        let a = s.open_text("a.log", LOG_A);
        let root = s.create_doc_view(a).unwrap();
        let child = s.fork_view(root, "error").unwrap();
        s.remove_view(child);
        assert!(s.view(child).is_none());
        assert_eq!(s.view(root).unwrap().children.len(), 0);
        assert_eq!(s.roots().len(), 1);
    }

    #[test]
    fn view_title_and_note_can_be_edited_or_reset() {
        let mut s = session();
        let a = s.open_text("a.log", LOG_A);
        let root = s.create_doc_view(a).unwrap();
        let child = s.fork_view(root, "level>=warn").unwrap();
        assert!(s.set_view_meta(child, " 告警与错误 ", " 排查 08:12 启动 "));
        assert_eq!(s.view(child).unwrap().title, "告警与错误");
        assert_eq!(s.view(child).unwrap().note, "排查 08:12 启动");

        assert!(s.set_view_meta(child, "   ", "  "));
        assert_eq!(s.view(child).unwrap().title, "level>=warn");
        assert_eq!(s.view(child).unwrap().note, "");

        assert!(s.set_view_meta(root, "", ""));
        assert_eq!(s.view(root).unwrap().title, "a.log");
        assert!(!s.set_view_meta(999, "x", "y"));
    }

    #[test]
    fn removing_a_filter_level_reparents_and_recomputes_children() {
        let mut s = session();
        let a = s.open_text("a.log", LOG_A);
        let root = s.create_doc_view(a).unwrap();
        let error_only = s.fork_view(root, "level=error").unwrap();
        let ready = s.fork_view(error_only, "ready").unwrap();
        let info = s.fork_view(ready, "info").unwrap();
        assert_eq!(s.visible_len(ready), 0);
        assert_eq!(s.visible_len(info), 0);

        assert!(s.remove_view_level(error_only));
        assert!(s.view(error_only).is_none());
        assert_eq!(s.view(ready).unwrap().parent, Some(root));
        assert_eq!(s.view(root).unwrap().children, vec![ready]);
        assert_eq!(s.view_path(info), vec![root, ready, info]);
        assert_eq!(s.visible_len(ready), 1);
        assert_eq!(s.visible_len(info), 1);

        assert!(!s.remove_view_level(root));
    }

    #[test]
    fn rules_respect_doc_scope() {
        let mut s = session();
        let a = s.open_text("a.log", LOG_A);
        let b = s.open_text("b.log", LOG_B);
        let rule = HighlightRule::plain(
            "timeout",
            crate::style::RuleScope::Doc(a),
            "timeout",
            true,
            crate::style::TextStyle::default(),
            Some(crate::style::Rgb::new(30, 30, 0)),
        );
        s.add_rule(rule);
        assert_eq!(s.rules_for(LineRef::new(a, 0)).count(), 1);
        assert_eq!(s.rules_for(LineRef::new(b, 0)).count(), 0);
    }

    const STACK: &str = "\
2026-10-31 10:00:00.000 INFO boot
2026-10-31 10:00:02.000 ERROR camera timeout
Traceback (most recent call last):
  File \"camera.py\", line 1
2026-10-31 10:00:04.000 INFO ready
";

    #[test]
    fn continuation_lines_stay_with_their_event() {
        let mut s = session();
        let doc = s.open_text("stack.log", STACK);
        let root = s.create_doc_view(doc).unwrap();
        assert_eq!(s.visible_len(root), 5);
        let hit = s.fork_view(root, "timeout").unwrap();
        assert_eq!(s.visible_len(hit), 3);
        let file_hit = s.fork_view(root, "File").unwrap();
        assert_eq!(s.visible_len(file_hit), 3);
        let around = s.fork_view(root, "timeout around:1").unwrap();
        assert_eq!(s.visible_len(around), 4);
        assert!(s.line_text(s.line_ref(around, 0).unwrap()).contains("boot"));

        let traceback = s.line_ref(root, 2).unwrap();
        assert!(s.toggle_bookmark(traceback));
        assert_eq!(s.bookmark_count(doc), 3);
        assert!(s.event_text(traceback).contains("Traceback"));
        let bookmarked = s.fork_view(root, "bm:").unwrap();
        assert_eq!(s.visible_len(bookmarked), 3);
        assert!(!s.toggle_bookmark(traceback));
        assert_eq!(s.bookmark_count(doc), 0);
        assert_eq!(s.visible_len(bookmarked), 0);

        let plain = s.open_text("plain.log", "no stamp\nstill none\n");
        let plain_view = s.create_doc_view(plain).unwrap();
        let one = s.fork_view(plain_view, "stamp").unwrap();
        assert_eq!(s.visible_len(one), 1);
    }

    #[test]
    fn correlation_view_is_a_time_ordered_timeline() {
        let mut s = session();
        let a = s.open_text(
            "a.log",
            "\
2026-10-31 10:00:00.000 INFO trace_id=abc-123 boot
2026-10-31 10:00:04.000 INFO other
",
        );
        let b = s.open_text(
            "b.log",
            "\
2026-10-31 10:00:01.000 ERROR trace_id=abc-123 camera timeout
Traceback (most recent call last):
  File \"camera.py\", line 1
2026-10-31 10:00:05.000 INFO trace_id=zzz done
",
        );
        let merged = s.create_merged_view(&[a, b], "abc-123").unwrap();
        s.update_view(merged, "id:abc-123").unwrap();
        assert_eq!(s.visible_len(merged), 4);
        let first = s.line_text(s.line_ref(merged, 0).unwrap());
        assert!(first.contains("boot"));
        assert!(s
            .line_text(s.line_ref(merged, 1).unwrap())
            .contains("timeout"));
        assert!(s
            .line_text(s.line_ref(merged, 2).unwrap())
            .contains("Traceback"));
    }
}
