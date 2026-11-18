use std::env;
use std::fs;
use std::path::PathBuf;

use neolog_core::SessionSnapshot;
use serde::{Deserialize, Serialize};

pub fn config_dir() -> PathBuf {
    if let Some(appdata) = env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("neo-log");
    }
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("neo-log");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("neo-log");
    }
    PathBuf::from(".neo-log")
}

pub fn session_path() -> PathBuf {
    config_dir().join("session.json")
}

pub fn plugins_dir() -> PathBuf {
    config_dir().join("plugins")
}

fn ui_path() -> PathBuf {
    config_dir().join("ui.conf")
}

fn user_path() -> PathBuf {
    config_dir().join("user.json")
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub query: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserState {
    #[serde(default)]
    pub find_history: Vec<String>,
    #[serde(default)]
    pub presets: Vec<Preset>,
}

pub fn load_user() -> UserState {
    let Ok(text) = fs::read_to_string(user_path()) else {
        return UserState::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save_user(state: &UserState) -> Result<(), String> {
    let path = user_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建配置目录失败: {error}"))?;
    }
    let json = serde_json::to_string_pretty(state).map_err(|error| error.to_string())?;
    fs::write(path, json).map_err(|error| format!("写入查找历史失败: {error}"))
}

/// UI preferences live in a tiny key=value file next to the session, so chrome
/// layout choices survive restarts without touching the session schema.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiPrefs {
    pub theme: Option<String>,
    pub sidebar_open: Option<bool>,
    pub stats_open: Option<bool>,
    pub filter_bar_open: Option<bool>,
}

pub fn load_ui() -> UiPrefs {
    fs::read_to_string(ui_path())
        .map(|text| parse_ui(&text))
        .unwrap_or_default()
}

fn parse_ui(text: &str) -> UiPrefs {
    let mut prefs = UiPrefs::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "theme" if !value.is_empty() => prefs.theme = Some(value.to_string()),
            "sidebar" => prefs.sidebar_open = parse_bool(value),
            "stats" => prefs.stats_open = parse_bool(value),
            "filter_bar" => prefs.filter_bar_open = parse_bool(value),
            _ => {}
        }
    }
    prefs
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" | "on" => Some(true),
        "0" | "false" | "off" => Some(false),
        _ => None,
    }
}

pub fn save_ui(prefs: &UiPrefs) -> Result<(), String> {
    let path = ui_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建配置目录失败: {error}"))?;
    }
    let mut text = String::new();
    if let Some(theme) = &prefs.theme {
        text.push_str(&format!("theme={theme}\n"));
    }
    if let Some(open) = prefs.sidebar_open {
        text.push_str(&format!("sidebar={}\n", open as u8));
    }
    if let Some(open) = prefs.stats_open {
        text.push_str(&format!("stats={}\n", open as u8));
    }
    if let Some(open) = prefs.filter_bar_open {
        text.push_str(&format!("filter_bar={}\n", open as u8));
    }
    fs::write(&path, text).map_err(|error| format!("写入界面配置失败: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_prefs_parse_known_keys_and_ignore_junk() {
        let prefs =
            parse_ui("theme=light\nsidebar=1\nstats=off\nfilter_bar=true\nunknown=3\nmalformed\n");
        assert_eq!(prefs.theme.as_deref(), Some("light"));
        assert_eq!(prefs.sidebar_open, Some(true));
        assert_eq!(prefs.stats_open, Some(false));
        assert_eq!(prefs.filter_bar_open, Some(true));
    }

    #[test]
    fn ui_prefs_missing_keys_stay_none() {
        let prefs = parse_ui("theme=\nsidebar=maybe\n");
        assert_eq!(prefs.theme, None);
        assert_eq!(prefs.sidebar_open, None);
        assert_eq!(prefs.stats_open, None);
    }
}

pub fn load_session() -> Result<Option<SessionSnapshot>, String> {
    let path = session_path();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("读取会话失败: {error}")),
    };
    SessionSnapshot::from_json(&text).map(Some)
}

pub fn save_session(snapshot: &SessionSnapshot) -> Result<(), String> {
    let path = session_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建配置目录失败: {error}"))?;
    }
    let json = snapshot.to_json()?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|error| format!("写入会话失败: {error}"))?;
    let bak = path.with_extension("json.bak");
    if path.exists() {
        if bak.exists() {
            let _ = fs::remove_file(&bak);
        }
        if let Err(error) = fs::rename(&path, &bak) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("写入会话失败: {error}"));
        }
    }
    if let Err(error) = fs::rename(&tmp, &path) {
        if bak.exists() {
            let _ = fs::rename(&bak, &path);
        }
        return Err(format!("写入会话失败: {error}"));
    }
    if bak.exists() {
        let _ = fs::remove_file(&bak);
    }
    Ok(())
}
