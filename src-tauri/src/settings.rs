//! User settings and the remembered sidebar position, stored as JSON in the app config dir.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DockEdge {
    Left,
    #[default]
    Right,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// How many tubes the sidebar shows (1..=8).
    pub tube_count: u8,
    /// Enabled agent ids, in display order.
    pub agents: Vec<String>,
    pub demo_mode: bool,
    pub dock_edge: DockEdge,
}

impl Default for Settings {
    fn default() -> Self {
        Self { tube_count: 4, agents: Vec::new(), demo_mode: false, dock_edge: DockEdge::Right }
    }
}

impl Settings {
    pub fn normalized(mut self) -> Self {
        self.tube_count = self.tube_count.clamp(1, 8);
        let mut seen = std::collections::HashSet::new();
        self.agents.retain(|id| !id.is_empty() && seen.insert(id.clone()));
        self
    }
}

fn config_file(app: &AppHandle, name: &str) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join(name))
}

fn write_json<T: Serialize>(app: &AppHandle, name: &str, value: &T) -> Result<(), String> {
    let path = config_file(app, name).ok_or("no app config dir")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Returns the settings and whether a settings file already existed.
pub fn load(app: &AppHandle) -> (Settings, bool) {
    let Some(path) = config_file(app, "settings.json") else { return (Settings::default(), false) };
    match fs::read_to_string(&path) {
        Ok(text) => (serde_json::from_str::<Settings>(&text).unwrap_or_default().normalized(), true),
        Err(_) => (Settings::default(), false),
    }
}

pub fn save(app: &AppHandle, s: &Settings) -> Result<(), String> {
    write_json(app, "settings.json", s)
}

#[derive(Serialize, Deserialize)]
struct Position {
    x: f64,
    y: f64,
}

pub fn load_position(app: &AppHandle) -> Option<(f64, f64)> {
    let text = fs::read_to_string(config_file(app, "window.json")?).ok()?;
    let p: Position = serde_json::from_str(&text).ok()?;
    Some((p.x, p.y))
}

pub fn save_position(app: &AppHandle, x: f64, y: f64) {
    let _ = write_json(app, "window.json", &Position { x, y });
}
