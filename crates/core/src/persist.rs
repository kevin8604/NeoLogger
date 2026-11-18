use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::matcher::Needle;
use crate::session::{Base, Session, ViewId};
use crate::source::{DocId, TextEncoding};
use crate::style::{HighlightRule, Matcher, Rgb, RuleScope, TextStyle};

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionSnapshot {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub docs: Vec<DocSnapshot>,
    #[serde(default)]
    pub views: Vec<ViewSnapshot>,
    #[serde(default)]
    pub roots: Vec<ViewId>,
    #[serde(default)]
    pub bookmarks: Vec<DocBookmarks>,
    #[serde(default)]
    pub rules: Vec<RuleSnapshot>,
    /// App-owned cursor, restored by the UI layer.
    #[serde(default)]
    pub active: Vec<ViewId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocSnapshot {
    pub path: PathBuf,
    pub plugin_id: String,
    pub day_base: i64,
    #[serde(default)]
    pub encoding: String,
    #[serde(default)]
    pub missing: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RestoreReport {
    pub missing: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocBookmarks {
    pub doc: DocId,
    pub lines: Vec<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewSnapshot {
    pub id: ViewId,
    pub title: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub query: String,
    pub base: BaseSnapshot,
    #[serde(default)]
    pub parent: Option<ViewId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BaseSnapshot {
    Doc(DocId),
    Merged(Vec<DocId>),
    View(ViewId),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleSnapshot {
    pub id: u64,
    pub label: String,
    pub scope: RuleScope,
    pub matcher: MatcherSnapshot,
    pub style: TextStyle,
    #[serde(default)]
    pub row_bg: Option<Rgb>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MatcherSnapshot {
    Plain { text: String, case_sensitive: bool },
    Regex { pattern: String },
}

impl SessionSnapshot {
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|error| error.to_string())
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let snapshot: SessionSnapshot =
            serde_json::from_str(text).map_err(|error| format!("会话文件格式错误: {error}"))?;
        if snapshot.version > VERSION {
            return Err(format!(
                "会话版本 {} 高于当前支持的 {VERSION}",
                snapshot.version
            ));
        }
        Ok(snapshot)
    }
}

impl Session {
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            version: VERSION,
            docs: self
                .docs()
                .iter()
                .map(|doc| DocSnapshot {
                    path: doc.path.clone(),
                    plugin_id: doc.plugin_id.clone(),
                    day_base: doc.day_base,
                    encoding: doc.encoding.id().to_string(),
                    missing: doc.missing,
                })
                .collect(),
            views: self
                .views()
                .map(|view| ViewSnapshot {
                    id: view.id,
                    title: view.title.clone(),
                    note: view.note.clone(),
                    query: view.query.clone(),
                    base: match &view.base {
                        Base::Doc(doc) => BaseSnapshot::Doc(*doc),
                        Base::Merged(docs) => BaseSnapshot::Merged(docs.clone()),
                        Base::View(parent) => BaseSnapshot::View(*parent),
                    },
                    parent: view.parent,
                })
                .collect(),
            roots: self.roots().to_vec(),
            bookmarks: self
                .doc_ids()
                .map(|doc| DocBookmarks {
                    doc,
                    lines: self.bookmark_list(doc),
                })
                .filter(|entry| !entry.lines.is_empty())
                .collect(),
            rules: self
                .rules()
                .iter()
                .map(|rule| RuleSnapshot {
                    id: rule.id,
                    label: rule.label.clone(),
                    scope: rule.scope.clone(),
                    matcher: MatcherSnapshot::from(&rule.matcher),
                    style: rule.style,
                    row_bg: rule.row_bg,
                })
                .collect(),
            active: Vec::new(),
        }
    }

    /// Replaces the session with the snapshot. A missing file becomes an
    /// unavailable document and the rest of the views are kept. Only a broken
    /// snapshot returns an error.
    pub fn restore(&mut self, snapshot: &SessionSnapshot) -> Result<RestoreReport, String> {
        let mut fresh = Session::new(self.plugins().clone());
        let missing = fresh.restore_docs(snapshot)?;
        fresh.restore_views(snapshot)?;
        fresh.restore_bookmarks(snapshot);
        fresh.recompute_bookmark_views();
        fresh.restore_rules(snapshot)?;
        *self = fresh;
        Ok(RestoreReport { missing })
    }

    fn restore_docs(&mut self, snapshot: &SessionSnapshot) -> Result<usize, String> {
        let mut missing = 0usize;
        for saved in &snapshot.docs {
            let encoding = TextEncoding::parse(&saved.encoding);
            match self.open_path_with(&saved.path, encoding) {
                Ok(id) => {
                    if let Some(slot) = self.docs.get_mut(id as usize) {
                        slot.day_base = saved.day_base;
                    }
                    if !self.set_doc_plugin(id, &saved.plugin_id) {
                        self.refresh_doc_times(id);
                    }
                }
                Err(_) => {
                    self.push_unavailable(&saved.path, &saved.plugin_id, saved.day_base, encoding);
                    missing += 1;
                }
            }
        }
        Ok(missing)
    }

    fn restore_views(&mut self, snapshot: &SessionSnapshot) -> Result<(), String> {
        let mut views = snapshot.views.clone();
        views.sort_by_key(|view| view.id);
        let max_id = views.iter().map(|view| view.id).max().unwrap_or(0);
        if max_id > 100_000 {
            return Err("会话中的视图编号异常".to_string());
        }
        self.views.resize_with(max_id as usize + 1, || None);

        for view in &views {
            let base = match &view.base {
                BaseSnapshot::Doc(doc) => {
                    if self.doc(*doc).is_none() {
                        return Err(format!("视图 {} 引用了不存在的文档 {doc}", view.id));
                    }
                    Base::Doc(*doc)
                }
                BaseSnapshot::Merged(docs) => {
                    if docs.iter().any(|doc| self.doc(*doc).is_none()) {
                        return Err(format!("合并视图 {} 引用了不存在的文档", view.id));
                    }
                    Base::Merged(docs.clone())
                }
                BaseSnapshot::View(parent) => {
                    if *parent >= view.id || self.view(*parent).is_none() {
                        return Err(format!("视图 {} 的父视图不存在", view.id));
                    }
                    Base::View(*parent)
                }
            };
            let filters = crate::filter::parse_query(&view.query)
                .map_err(|error| format!("视图 {} 的过滤条件无效: {error}", view.id))?;
            let rows = self.compute_rows(&base, &filters);
            let node = crate::session::ViewNode {
                id: view.id,
                title: view.title.clone(),
                note: view.note.clone(),
                query: view.query.clone(),
                base,
                filters,
                rows,
                parent: view.parent,
                children: Vec::new(),
            };
            self.views[view.id as usize] = Some(node);
        }

        for view in &views {
            if let Some(parent) = view.parent {
                if let Some(node) = self.views.get_mut(parent as usize).and_then(Option::as_mut) {
                    node.children.push(view.id);
                }
            }
        }

        let mut roots: Vec<ViewId> = snapshot
            .roots
            .iter()
            .copied()
            .filter(|id| self.view(*id).is_some_and(|view| view.parent.is_none()))
            .collect();
        if roots.is_empty() {
            roots = views
                .iter()
                .filter(|view| view.parent.is_none() && self.view(view.id).is_some())
                .map(|view| view.id)
                .collect();
        }
        self.roots = roots;
        self.next_view = self.views.len() as ViewId;
        Ok(())
    }

    fn restore_bookmarks(&mut self, snapshot: &SessionSnapshot) {
        for entry in &snapshot.bookmarks {
            let Some(doc) = self.doc(entry.doc) else {
                continue;
            };
            let length = doc.len();
            let set = self.bookmarks.entry(entry.doc).or_default();
            for line in &entry.lines {
                if (*line as usize) < length {
                    set.insert(*line);
                }
            }
        }
    }

    fn restore_rules(&mut self, snapshot: &SessionSnapshot) -> Result<(), String> {
        for saved in &snapshot.rules {
            let matcher = match &saved.matcher {
                MatcherSnapshot::Plain {
                    text,
                    case_sensitive,
                } => Matcher::Plain(Needle::new(text.clone(), *case_sensitive)),
                MatcherSnapshot::Regex { pattern } => Matcher::Regex(
                    regex::RegexBuilder::new(pattern)
                        .case_insensitive(true)
                        .build()
                        .map_err(|error| format!("规则 {} 的正则无效: {error}", saved.id))?,
                ),
            };
            self.rules.push(HighlightRule {
                id: saved.id,
                label: saved.label.clone(),
                scope: saved.scope.clone(),
                matcher,
                style: saved.style,
                row_bg: saved.row_bg,
            });
            self.next_rule = self.next_rule.max(saved.id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::plugin::PluginRegistry;

    const LOG_A: &str = "\
2026-10-31 10:00:00.000 INFO boot
2026-10-31 10:00:02.000 ERROR camera timeout
2026-10-31 10:00:04.000 INFO ready
";
    const LOG_B: &str = "\
2026-10-31 10:00:01.000 WARN battery low
2026-10-31 10:00:03.000 ERROR lidar lost
";

    fn temp_file(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("neolog-persist-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn registry() -> PluginRegistry {
        PluginRegistry::new(vec![])
    }

    #[test]
    fn roundtrip_keeps_views_bookmarks_and_rules() {
        let a = temp_file("a.log", LOG_A);
        let b = temp_file("b.log", LOG_B);
        let mut session = Session::new(registry());
        let da = session.open_path(&a).unwrap();
        let db = session.open_path(&b).unwrap();
        let doc_view = session.create_doc_view(da).unwrap();
        let merged = session.create_merged_view(&[da, db], "merged").unwrap();
        let child = session.fork_view(merged, "level>=warn").unwrap();
        assert!(session.set_view_meta(child, "告警与错误", "排查 08:12 启动"));
        let lr = session.line_ref(doc_view, 1).unwrap();
        session.toggle_bookmark(lr);
        session.add_rule(HighlightRule::plain(
            "camera",
            RuleScope::Doc(da),
            "camera",
            false,
            TextStyle::default(),
            Some(Rgb(60, 40, 0)),
        ));

        let json = session.snapshot().to_json().unwrap();
        let snapshot = SessionSnapshot::from_json(&json).unwrap();
        let mut restored = Session::new(registry());
        restored.restore(&snapshot).unwrap();

        assert_eq!(restored.roots(), &[doc_view, merged]);
        assert_eq!(restored.visible_len(doc_view), 3);
        assert_eq!(restored.visible_len(merged), 5);
        assert_eq!(restored.visible_len(child), 3);
        assert_eq!(restored.view_path(child), vec![merged, child]);
        assert_eq!(restored.view(child).unwrap().title, "告警与错误");
        assert_eq!(restored.view(child).unwrap().note, "排查 08:12 启动");
        assert!(restored.is_bookmarked(lr));
        assert_eq!(restored.bookmarked_rows(doc_view), vec![1]);
        assert_eq!(restored.rules().len(), 1);
        assert_eq!(restored.rules()[0].scope, RuleScope::Doc(da));
        assert!(restored.rules()[0].is_match("camera timeout"));

        let _ = fs::remove_file(&a);
        let _ = fs::remove_file(&b);
    }

    #[test]
    fn missing_file_keeps_the_rest_of_the_session() {
        let a = temp_file("kept.log", LOG_A);
        let mut session = Session::new(registry());
        let snapshot = SessionSnapshot {
            version: VERSION,
            docs: vec![
                DocSnapshot {
                    path: a.clone(),
                    plugin_id: "plain".to_string(),
                    day_base: 0,
                    encoding: "utf-8".to_string(),
                    missing: false,
                },
                DocSnapshot {
                    path: PathBuf::from("definitely-missing-neolog.log"),
                    plugin_id: "plain".to_string(),
                    day_base: 0,
                    encoding: String::new(),
                    missing: false,
                },
            ],
            views: vec![ViewSnapshot {
                id: 0,
                title: "kept.log".to_string(),
                note: String::new(),
                query: String::new(),
                base: BaseSnapshot::Doc(0),
                parent: None,
            }],
            roots: vec![0],
            ..Default::default()
        };
        let report = session.restore(&snapshot).unwrap();
        assert_eq!(report.missing, 1);
        assert_eq!(session.docs().len(), 2);
        assert!(!session.docs()[0].missing);
        assert!(session.docs()[1].missing);
        assert_eq!(session.visible_len(0), 3);
        let _ = fs::remove_file(&a);
    }

    #[test]
    fn missing_file_keeps_an_unavailable_document() {
        let mut session = Session::new(registry());
        let snapshot = SessionSnapshot {
            version: VERSION,
            docs: vec![DocSnapshot {
                path: PathBuf::from("definitely-missing-neolog.log"),
                plugin_id: "plain".to_string(),
                day_base: 0,
                encoding: String::new(),
                missing: false,
            }],
            ..Default::default()
        };
        let report = session.restore(&snapshot).unwrap();
        assert_eq!(report.missing, 1);
        assert!(session.docs()[0].missing);
        assert_eq!(session.docs().len(), 1);
    }

    #[test]
    fn rejects_newer_versions() {
        let json = r#"{"version":999}"#;
        assert!(SessionSnapshot::from_json(json).is_err());
    }
}

impl From<&Matcher> for MatcherSnapshot {
    fn from(matcher: &Matcher) -> Self {
        match matcher {
            Matcher::Plain(needle) => MatcherSnapshot::Plain {
                text: needle.raw().to_string(),
                case_sensitive: needle.case_sensitive(),
            },
            Matcher::Regex(regex) => MatcherSnapshot::Regex {
                pattern: regex.as_str().to_string(),
            },
        }
    }
}
