//! User settings and the remembered sidebar position, stored as JSON in the app config dir.
//!
//! Writes are atomic (unique temp file → fsync → rename). Callers serialize settings writes by
//! holding the settings mutex in lib.rs across update + persist, so there is one owner.
//! A settings file that can't be read or parsed is never silently overwritten: it is kept as
//! `settings.json.corrupt-<unix-ts>` first, and individually invalid fields fall back to defaults.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager};

pub const MAX_TUBES: u8 = 8;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DockEdge {
    Left,
    #[default]
    Right,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Legacy (older settings screens): ignored for rendering; kept so old files round-trip.
    pub tube_count: u8,
    /// Enabled agent ids, in display order. The sidebar shows one tube per entry (max 8).
    pub agents: Vec<String>,
    /// Legacy: demo mode now comes only from the AGENTMETER_DEMO env var; this mirrors it.
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
        self.tube_count = self.tube_count.clamp(1, MAX_TUBES);
        let mut seen = std::collections::HashSet::new();
        self.agents.retain(|id| !id.is_empty() && seen.insert(id.clone()));
        self
    }
}

/// Result of reading settings at startup.
pub struct Loaded {
    pub settings: Settings,
    /// No usable `agents` list was found (first run, or it was unreadable): seed installed agents.
    pub needs_default_agents: bool,
    /// Set when an invalid settings file could NOT be preserved: nothing may write settings.json
    /// until `preserve_existing` succeeds or the user explicitly resets.
    pub blocked: Option<String>,
}

fn config_file(app: &AppHandle, name: &str) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join(name))
}

fn unix_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Atomic write: unique temp file in the same dir, fsync, rename over the target, fsync the dir.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or("settings path has no parent dir")?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
        Ok::<_, std::io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("writing {}: {e}", path.display()))
}

fn write_json<T: Serialize>(app: &AppHandle, name: &str, value: &T) -> Result<(), String> {
    let path = config_file(app, name).ok_or("no app config dir")?;
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write_atomic(&path, json.as_bytes())
}

/// Collision-safe backup name: timestamp + pid + per-process counter.
fn corrupt_path(path: &Path) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    path.with_file_name(format!(
        "settings.json.corrupt-{}-{}-{}",
        unix_ts(),
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Copy an existing settings file aside (keeps the original in place). A missing file is fine.
fn preserve_at(path: &Path) -> Result<Option<PathBuf>, String> {
    match fs::metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        _ => {}
    }
    let aside = corrupt_path(path);
    fs::copy(path, &aside).map(|_| Some(aside)).map_err(|e| format!("cannot back up {}: {e}", path.display()))
}

/// Retry preserving the current settings file (used before writing while blocked).
pub fn preserve_existing(app: &AppHandle) -> Result<Option<PathBuf>, String> {
    preserve_at(&config_file(app, "settings.json").ok_or("no app config dir")?)
}

fn blocked_msg(path: &Path, why: &str) -> String {
    format!(
        "Your settings file {} could not be read or backed up ({why}), so AgentMeter won't overwrite it. \
         Fix or move that file, or choose \"Reset Settings\" in the menu-bar icon to start fresh.",
        path.display()
    )
}

/// Read settings. Missing file → defaults. Unreadable / unparsable file → moved aside, defaults.
/// Readable object with some bad fields → good fields kept, bad ones defaulted, original copied aside.
pub fn load(app: &AppHandle) -> Loaded {
    match config_file(app, "settings.json") {
        Some(path) => load_from(&path),
        None => Loaded { settings: Settings::default(), needs_default_agents: true, blocked: None },
    }
}

fn load_from(path: &Path) -> Loaded {
    let path = path.to_path_buf();
    let fresh = Loaded { settings: Settings::default(), needs_default_agents: true, blocked: None };
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return fresh,
        Err(e) => {
            let aside = corrupt_path(&path);
            let moved = fs::rename(&path, &aside);
            eprintln!(
                "[agentmeter] cannot read {}: {e}; {}; using defaults",
                path.display(),
                match &moved { Ok(()) => format!("moved it to {}", aside.display()), Err(m) => format!("could not move it aside ({m}): writes blocked") }
            );
            return Loaded {
                settings: Settings::default(),
                needs_default_agents: true,
                blocked: moved.err().map(|m| blocked_msg(&path, &format!("{e}; {m}"))),
            };
        }
    };
    let obj = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => m,
        other => {
            let why = other.err().map(|e| e.to_string()).unwrap_or_else(|| "not a JSON object".into());
            let aside = corrupt_path(&path);
            let moved = fs::rename(&path, &aside);
            eprintln!(
                "[agentmeter] {} is invalid ({why}); {}; using defaults",
                path.display(),
                match &moved { Ok(()) => format!("moved it to {}", aside.display()), Err(m) => format!("could not move it aside ({m}): writes blocked") }
            );
            return Loaded {
                settings: Settings::default(),
                needs_default_agents: true,
                blocked: moved.err().map(|m| blocked_msg(&path, &format!("{why}; {m}"))),
            };
        }
    };

    // Field by field: keep what is valid, default what isn't.
    let d = Settings::default();
    let mut bad = Vec::new();
    let mut field = |key: &str| {
        let v = obj.get(key).cloned();
        if v.is_none() {
            bad.push(format!("{key} (missing)"));
        }
        v
    };
    let tube_count = match field("tubeCount").and_then(|v| v.as_f64()) {
        Some(n) if n.is_finite() => n.round().clamp(1.0, MAX_TUBES as f64) as u8,
        _ => d.tube_count,
    };
    let agents_v = field("agents");
    let agents: Option<Vec<String>> = agents_v
        .as_ref()
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect());
    let demo_v = field("demoMode");
    let demo_mode = demo_v.as_ref().and_then(Value::as_bool);
    let dock_v = field("dockEdge");
    let dock_edge = dock_v.clone().and_then(|v| serde_json::from_value::<DockEdge>(v).ok());

    if obj.get("tubeCount").is_some_and(|v| !v.as_f64().is_some_and(|n| (1.0..=MAX_TUBES as f64).contains(&n))) {
        bad.push("tubeCount".into());
    }
    if agents_v.is_some() && agents.is_none() {
        bad.push("agents".into());
    }
    if demo_v.is_some() && demo_mode.is_none() {
        bad.push("demoMode".into());
    }
    if dock_v.is_some() && dock_edge.is_none() {
        bad.push("dockEdge".into());
    }
    let invalid: Vec<_> = bad.iter().filter(|b| !b.ends_with("(missing)")).collect();
    let mut blocked = None;
    if !invalid.is_empty() {
        let kept = preserve_at(&path);
        eprintln!(
            "[agentmeter] invalid settings fields {invalid:?} in {}; defaulted them; {}",
            path.display(),
            match &kept { Ok(p) => format!("original kept at {:?}", p), Err(m) => format!("{m}: writes blocked") }
        );
        blocked = kept.err().map(|m| blocked_msg(&path, &format!("invalid {invalid:?}; {m}")));
    }

    let needs_default_agents = agents.is_none();
    let settings = Settings {
        tube_count,
        agents: agents.unwrap_or_default(),
        demo_mode: demo_mode.unwrap_or(d.demo_mode),
        dock_edge: dock_edge.unwrap_or(d.dock_edge),
    }
    .normalized();
    Loaded { settings, needs_default_agents, blocked }
}

/// Persist settings. Callers must hold the settings mutex (single writer).
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
    (p.x.is_finite() && p.y.is_finite()).then_some((p.x, p.y))
}

pub fn save_position(app: &AppHandle, x: f64, y: f64) {
    if let Err(e) = write_json(app, "window.json", &Position { x, y }) {
        eprintln!("[agentmeter] {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentmeter-test-{tag}-{}-{}", std::process::id(), unix_ts()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn corrupt_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir).unwrap().filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.contains(".corrupt-")).collect()
    }

    #[test]
    fn missing_file_gives_defaults_and_seeds_agents() {
        let d = scratch("missing");
        let l = load_from(&d.join("settings.json"));
        assert_eq!(l.settings, Settings::default());
        assert!(l.needs_default_agents);
    }

    #[test]
    fn malformed_json_is_moved_aside_not_overwritten() {
        let d = scratch("malformed");
        let p = d.join("settings.json");
        fs::write(&p, "{ not json").unwrap();
        let l = load_from(&p);
        assert_eq!(l.settings, Settings::default());
        assert!(!p.exists(), "original must be moved aside before any write");
        let aside = corrupt_files(&d);
        assert_eq!(aside.len(), 1);
        assert_eq!(fs::read_to_string(d.join(&aside[0])).unwrap(), "{ not json");
    }

    #[test]
    fn invalid_fields_are_defaulted_individually_and_original_kept() {
        let d = scratch("fields");
        let p = d.join("settings.json");
        let orig = r#"{"tubeCount": 99, "agents": ["codex","claude-code","codex",""], "demoMode": "yes", "dockEdge": "left"}"#;
        fs::write(&p, orig).unwrap();
        let l = load_from(&p);
        assert_eq!(l.settings.tube_count, MAX_TUBES);
        assert_eq!(l.settings.agents, ["codex", "claude-code"]);
        assert!(!l.settings.demo_mode);
        assert_eq!(l.settings.dock_edge, DockEdge::Left);
        assert!(!l.needs_default_agents);
        assert_eq!(fs::read_to_string(&p).unwrap(), orig, "file untouched");
        assert_eq!(corrupt_files(&d).len(), 1, "a copy of the original is kept");
        assert!(l.blocked.is_none());
    }

    #[test]
    fn valid_file_round_trips_without_side_files() {
        let d = scratch("valid");
        let p = d.join("settings.json");
        let s = Settings { tube_count: 2, agents: vec!["codex".into()], demo_mode: true, dock_edge: DockEdge::Left };
        write_atomic(&p, serde_json::to_string(&s).unwrap().as_bytes()).unwrap();
        let l = load_from(&p);
        assert_eq!(l.settings, s);
        assert!(!l.needs_default_agents);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 1, "no temp or corrupt files left behind");
        assert!(l.blocked.is_none());
    }

    #[test]
    fn failed_backup_blocks_writes() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("blocked");
        let p = d.join("settings.json");
        fs::write(&p, "{ not json").unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o555)).unwrap(); // dir read-only: no rename/copy
        let l = load_from(&p);
        let retry = preserve_at(&p);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(l.blocked.is_some(), "unpreserved original must block writes");
        assert!(retry.is_err());
        assert_eq!(fs::read_to_string(&p).unwrap(), "{ not json", "original untouched");
        // Once the directory is writable again, preserving succeeds and unblocks.
        assert!(preserve_at(&p).unwrap().is_some());
    }

    #[test]
    fn backup_names_do_not_collide() {
        let p = Path::new("/x/settings.json");
        assert_ne!(corrupt_path(p), corrupt_path(p));
    }

    #[test]
    fn concurrent_atomic_writes_never_corrupt() {
        let d = scratch("concurrent");
        let p = d.join("settings.json");
        let handles: Vec<_> = (0..8u8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        let s = Settings { tube_count: i % 8 + 1, ..Settings::default() };
                        write_atomic(&p, serde_json::to_string(&s).unwrap().as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        handles.into_iter().for_each(|h| h.join().unwrap());
        let parsed: Settings = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert!((1..=8).contains(&parsed.tube_count));
        assert_eq!(fs::read_dir(&d).unwrap().count(), 1, "no temp files left behind");
    }
}
