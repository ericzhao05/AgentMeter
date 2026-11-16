mod settings;

use agentmeter_usage as usage;

use settings::{DockEdge, Settings};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Monitor, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use usage::AgentUsage;

const SIDEBAR: &str = "sidebar";
const SETTINGS: &str = "settings";
const REAL_POLL: Duration = Duration::from_secs(45);
const DEMO_POLL: Duration = Duration::from_secs(2);

// Sidebar geometry in logical pixels; must match the layout in ui/sidebar.css.
const COL_W: f64 = 86.0; // one tube column (70 px tube + label room)
const PAD_X: f64 = 10.0;
const SIDEBAR_H: f64 = 372.0; // toolbar 26 + tube 280 + label 56 + bottom 10
const EDGE_MARGIN: f64 = 6.0;

/// Settings plus the first-run flag behind ONE mutex. Every settings write holds it across
/// update + persist + emit, so there is a single persistence owner and writes can't interleave.
struct SettingsStore {
    settings: Settings,
    /// True until the first real poll seeds `agents` (no usable list was on disk).
    needs_default_agents: bool,
    /// An invalid settings file couldn't be preserved: refuse writes until it can be (or Reset).
    blocked: Option<String>,
}

/// Latest usage snapshot. `gen` bumps whenever demo mode is toggled; a poll that started under
/// an older generation is dropped instead of published.
struct UsageCache {
    gen: u64,
    data: Vec<AgentUsage>,
    /// Last real (non-demo) poll, used to show a "refreshing" state when demo mode is turned off.
    last_real: Vec<AgentUsage>,
}

struct AppState {
    store: Mutex<SettingsStore>,
    usage: Mutex<UsageCache>,
    wake_poller: Mutex<Sender<()>>,
    move_gen: AtomicU64,
    /// Last user-drag activity (drag start, or a Move event while a drag is live).
    drag_seen: Mutex<Option<Instant>>,
}

const DRAG_QUIET: Duration = Duration::from_millis(600);

impl AppState {
    fn dragging(&self) -> bool {
        self.drag_seen.lock().unwrap().is_some_and(|t| t.elapsed() < DRAG_QUIET)
    }
}

fn launch_secs() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn sidebar_width(tubes: u32) -> f64 {
    PAD_X * 2.0 + COL_W * tubes.max(1) as f64
}

// ---------- commands ----------

#[tauri::command]
fn get_settings(state: tauri::State<'_, Arc<AppState>>) -> Settings {
    state.store.lock().unwrap().settings.clone()
}

/// Async so the file write + fsync never runs on the main thread; persistence itself is
/// serialized by the settings mutex on a blocking worker.
#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let settings = settings.normalized();
    let state = app.state::<Arc<AppState>>().inner().clone();
    let (a, st, s) = (app.clone(), state.clone(), settings.clone());
    let old = tauri::async_runtime::spawn_blocking(move || persist(&a, &st, s)).await.map_err(|e| e.to_string())??;
    if settings.dock_edge != old.dock_edge {
        let (a, edge) = (app.clone(), settings.dock_edge);
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = a.get_webview_window(SIDEBAR) {
                dock(&w, edge, None);
            }
        });
    }
    if settings.demo_mode != old.demo_mode {
        switch_mode(&app, &state, &settings);
    }
    Ok(())
}

/// Write settings (single owner: holds the settings mutex across check, write and emit).
/// Returns the previous settings. Refuses while an unpreserved invalid file is on disk.
fn persist(app: &AppHandle, state: &AppState, settings: Settings) -> Result<Settings, String> {
    let mut store = state.store.lock().unwrap();
    if let Some(msg) = store.blocked.clone() {
        match settings::preserve_existing(app) {
            Ok(kept) => {
                eprintln!("[agentmeter] settings file preserved at {kept:?}; writes unblocked");
                store.blocked = None;
            }
            Err(_) => return Err(msg),
        }
    }
    let old = store.settings.clone();
    settings::save(app, &settings)?;
    store.settings = settings.clone();
    store.needs_default_agents = false;
    app.emit("settings-changed", &settings).map_err(|e| e.to_string())?;
    Ok(old)
}

/// Explicit user reset (tray item, offered only when settings writes are blocked): overwrite
/// the unpreservable file with defaults and re-seed the agent list from the next real poll.
fn reset_settings(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut store = state.store.lock().unwrap();
        let fresh = Settings::default();
        match settings::save(&app, &fresh) {
            Ok(()) => {
                store.blocked = None;
                store.needs_default_agents = true;
                store.settings = fresh;
                let _ = app.emit("settings-changed", &store.settings);
                let _ = state.wake_poller.lock().unwrap().send(());
            }
            Err(e) => eprintln!("[agentmeter] reset failed: {e}"),
        }
    });
}

/// Demo mode toggled: invalidate in-flight polls and publish the new mode's state right away
/// (mock data when turning demo on; a "refreshing" placeholder until the next real poll when off).
fn switch_mode(app: &AppHandle, state: &AppState, settings: &Settings) {
    {
        let mut cache = state.usage.lock().unwrap();
        cache.gen += 1;
        cache.data = if settings.demo_mode {
            usage::mock_all(launch_secs())
        } else {
            refreshing(&cache.last_real, &settings.agents)
        };
        let _ = app.emit("usage-updated", &cache.data);
    }
    let _ = state.wake_poller.lock().unwrap().send(());
}

/// Placeholder rows (usage unknown) shown while the first real poll after demo mode is running.
fn refreshing(last_real: &[AgentUsage], enabled: &[String]) -> Vec<AgentUsage> {
    let mut rows: Vec<AgentUsage> = last_real
        .iter()
        .map(|a| AgentUsage { percent_left: None, active: false, exact: false, note: Some("Refreshing…".into()), ..a.clone() })
        .collect();
    for id in enabled {
        if !rows.iter().any(|a| &a.id == id) {
            rows.push(AgentUsage {
                id: id.clone(),
                name: id.clone(),
                installed: true,
                percent_left: None,
                resets_at: None,
                window: None,
                active: false,
                exact: false,
                note: Some("Refreshing…".into()),
            });
        }
    }
    rows
}

#[tauri::command]
fn get_usage(state: tauri::State<'_, Arc<AppState>>) -> Vec<AgentUsage> {
    state.usage.lock().unwrap().data.clone()
}

#[tauri::command]
fn open_settings(app: AppHandle) -> Result<(), String> {
    show_settings(&app).map_err(|e| e.to_string())
}

/// The sidebar asks for a width that fits `tubes` columns. Only a real size change does anything:
/// the docked edge stays put and the result is kept on screen. Never repositions mid-drag.
#[tauri::command]
fn resize_sidebar(window: WebviewWindow, state: tauri::State<'_, Arc<AppState>>, tubes: u32) -> Result<(), String> {
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let old_size = window.outer_size().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
    let w = sidebar_width(tubes.min(settings::MAX_TUBES as u32));
    if (old_size.width - w).abs() < 0.5 && (old_size.height - SIDEBAR_H).abs() < 0.5 {
        return Ok(()); // unchanged: leave the position alone (it may span monitors on purpose)
    }
    let pos = window.outer_position().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
    window.set_size(LogicalSize::new(w, SIDEBAR_H)).map_err(|e| e.to_string())?;
    if state.dragging() {
        return Ok(());
    }
    let edge = state.store.lock().unwrap().settings.dock_edge;
    let x = if edge == DockEdge::Right { pos.x + old_size.width - w } else { pos.x };
    keep_on_screen(&window, edge, x, pos.y, w);
    Ok(())
}

/// The sidebar is about to start a native window drag (mousedown on the panel).
#[tauri::command]
fn drag_began(state: tauri::State<'_, Arc<AppState>>) {
    *state.drag_seen.lock().unwrap() = Some(Instant::now());
}

// ---------- windows ----------

fn monitor_for(w: &WebviewWindow) -> Option<Monitor> {
    w.current_monitor().ok().flatten().or_else(|| w.primary_monitor().ok().flatten())
}

/// Logical work area (x, y, w, h) of a monitor: the screen minus menu bar and Dock.
fn work_area(m: &Monitor) -> (f64, f64, f64, f64) {
    let s = m.scale_factor();
    let a = m.work_area();
    (a.position.x as f64 / s, a.position.y as f64 / s, a.size.width as f64 / s, a.size.height as f64 / s)
}

/// Move the sidebar flush against the left or right edge, vertically centred.
fn dock(w: &WebviewWindow, edge: DockEdge, width: Option<f64>) {
    let Some(m) = monitor_for(w) else { return };
    let (ax, ay, aw, ah) = work_area(&m);
    let s = w.scale_factor().unwrap_or(1.0);
    let width = width.unwrap_or_else(|| w.outer_size().map(|z| z.to_logical::<f64>(s).width).unwrap_or(COL_W));
    let x = match edge {
        DockEdge::Right => ax + aw - width - EDGE_MARGIN,
        DockEdge::Left => ax + EDGE_MARGIN,
    };
    let y = ay + ((ah - SIDEBAR_H) / 2.0).max(0.0);
    let _ = w.set_position(LogicalPosition::new(x, y));
}

/// Clamp a logical window rect fully inside the work area of the screen it overlaps most.
/// Returns None when it overlaps no connected screen at all.
fn clamp_to_screen(app: &AppHandle, x: f64, y: f64, w: f64, h: f64) -> Option<(f64, f64)> {
    let areas: Vec<_> = app.available_monitors().unwrap_or_default().iter().map(work_area).collect();
    clamp_into(&areas, x, y, w, h)
}

/// Put the sidebar at (x, y) clamped fully onto a screen; if that rect is on no screen at all
/// (e.g. its display was disconnected), dock it on the current/primary monitor instead.
fn keep_on_screen(w: &WebviewWindow, edge: DockEdge, x: f64, y: f64, width: f64) {
    match clamp_to_screen(w.app_handle(), x, y, width, SIDEBAR_H) {
        Some((cx, cy)) => {
            let cur = w.outer_position().ok().zip(w.scale_factor().ok()).map(|(p, s)| p.to_logical::<f64>(s));
            if cur.is_none_or(|c| (c.x - cx).abs() >= 0.5 || (c.y - cy).abs() >= 0.5) {
                let _ = w.set_position(LogicalPosition::new(cx, cy));
            }
        }
        None => dock(w, edge, Some(width)),
    }
}

/// Pure part of `clamp_to_screen`: `areas` are logical (x, y, w, h) work areas.
fn clamp_into(areas: &[(f64, f64, f64, f64)], x: f64, y: f64, w: f64, h: f64) -> Option<(f64, f64)> {
    let overlap = |&(ax, ay, aw, ah): &(f64, f64, f64, f64)| {
        let ox = (x + w).min(ax + aw) - x.max(ax);
        let oy = (y + h).min(ay + ah) - y.max(ay);
        if ox > 0.0 && oy > 0.0 { ox * oy } else { 0.0 }
    };
    let &(ax, ay, aw, ah) = areas.iter().max_by(|a, b| overlap(a).total_cmp(&overlap(b))).filter(|a| overlap(a) > 0.0)?;
    Some((x.min(ax + aw - w).max(ax), y.min(ay + ah - h).max(ay)))
}

fn create_sidebar(app: &AppHandle, state: &Arc<AppState>) -> tauri::Result<WebviewWindow> {
    let s = state.store.lock().unwrap().settings.clone();
    let width = sidebar_width(s.tube_count.min(s.agents.len().max(1) as u8) as u32);
    let w = WebviewWindowBuilder::new(app, SIDEBAR, WebviewUrl::App("sidebar.html".into()))
        .title("AgentMeter")
        .inner_size(width, SIDEBAR_H)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .visible_on_all_workspaces(true)
        .visible(false)
        .build()?;

    match settings::load_position(app) {
        Some((x, y)) => keep_on_screen(&w, s.dock_edge, x, y, width),
        None => dock(&w, s.dock_edge, Some(width)),
    }
    w.show()?;

    // Remember where the user drags it (debounced: write once the window stops moving).
    let handle = app.clone();
    let st = state.clone();
    w.on_window_event(move |e| {
        // Monitor / scale change (e.g. moved to another display, resolution changed): re-fit.
        if let WindowEvent::ScaleFactorChanged { .. } = e {
            if let Some(w) = handle.get_webview_window(SIDEBAR) {
                if let (Ok(p), Ok(sz), Ok(sf)) = (w.outer_position(), w.outer_size(), w.scale_factor()) {
                    let (p, sz) = (p.to_logical::<f64>(sf), sz.to_logical::<f64>(sf));
                    let edge = st.store.lock().unwrap().settings.dock_edge;
                    keep_on_screen(&w, edge, p.x, p.y, sz.width);
                }
            }
        }
        if let WindowEvent::Moved(_) = e {
            {
                let mut drag = st.drag_seen.lock().unwrap();
                if drag.is_some_and(|t| t.elapsed() < DRAG_QUIET) {
                    *drag = Some(Instant::now()); // still dragging
                }
            }
            let gen = st.move_gen.fetch_add(1, Ordering::SeqCst) + 1;
            let (handle, st) = (handle.clone(), st.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(600));
                if st.move_gen.load(Ordering::SeqCst) != gen {
                    return;
                }
                if let Some(w) = handle.get_webview_window(SIDEBAR) {
                    if let (Ok(p), Ok(sf)) = (w.outer_position(), w.scale_factor()) {
                        let p = p.to_logical::<f64>(sf);
                        settings::save_position(&handle, p.x, p.y);
                    }
                }
            });
        }
    });
    Ok(w)
}

fn show_settings(app: &AppHandle) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window(SETTINGS) {
        w.show()?;
        w.unminimize()?;
        return w.set_focus();
    }
    let w = WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("settings.html".into()))
        .title("AgentMeter Settings")
        .inner_size(420.0, 560.0)
        .resizable(false)
        .maximizable(false)
        .center()
        .focused(true)
        .build()?;
    w.set_focus()
}

fn toggle_sidebar(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(SIDEBAR) {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else {
            let _ = w.show();
        }
        #[cfg(debug_assertions)]
        eprintln!("[agentmeter] sidebar visible={:?}", w.is_visible());
    }
}

fn build_tray(app: &AppHandle, settings_blocked: bool) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "Show/Hide Sidebar", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit AgentMeter", true, Some("CmdOrCtrl+Q"))?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&toggle, &settings, &sep, &quit])?;
    if settings_blocked {
        // Only offered when the settings file is unreadable and couldn't be backed up.
        let reset = MenuItem::with_id(app, "reset", "Reset Settings (settings file unreadable)", true, None::<&str>)?;
        menu.insert(&reset, 2)?;
    }
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("AgentMeter")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, e| match e.id.as_ref() {
            "toggle" => toggle_sidebar(app),
            "settings" => {
                let _ = show_settings(app);
            }
            "reset" => reset_settings(app),
            "quit" => app.exit(0),
            _ => {}
        });
    // Menu-bar glyph: a black+alpha template image that macOS tints for light/dark menu bars.
    match tauri::image::Image::from_bytes(include_bytes!("../../assets/icons/tray-icon@2x.png")) {
        Ok(icon) => tray = tray.icon(icon).icon_as_template(true),
        Err(e) => {
            eprintln!("[agentmeter] tray icon unreadable ({e}); using the app icon");
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
        }
    }
    tray.build(app)?;
    Ok(())
}

// ---------- polling ----------

fn spawn_poller(app: AppHandle, state: Arc<AppState>, wake: mpsc::Receiver<()>) {
    std::thread::spawn(move || loop {
        while wake.try_recv().is_ok() {} // wakes sent before this poll starts are covered by it
        // Read the generation BEFORE the mode, so a toggle in between always invalidates this poll.
        let gen = state.usage.lock().unwrap().gen;
        let demo = state.store.lock().unwrap().settings.demo_mode;
        let data = if demo { usage::mock_all(launch_secs()) } else { usage::poll_all() };

        // First run (no usable agent list on disk): enable every installed agent, in detection order.
        if !demo {
            let mut store = state.store.lock().unwrap();
            if store.needs_default_agents {
                store.needs_default_agents = false;
                store.settings.agents = data.iter().filter(|a| a.installed).map(|a| a.id.clone()).collect();
                // While blocked, show the defaults but never write over the unpreserved file.
                if store.blocked.is_none() {
                    if let Err(e) = settings::save(&app, &store.settings) {
                        eprintln!("[agentmeter] could not save default agents: {e}");
                    }
                }
                let _ = app.emit("settings-changed", &store.settings);
            }
        }

        #[cfg(debug_assertions)]
        eprintln!(
            "[agentmeter] {} poll: {}",
            if demo { "demo" } else { "real" },
            data.iter()
                .map(|a| format!(
                    "{}(installed={} left={:?} exact={} active={} resets={:?} note={:?})",
                    a.id, a.installed, a.percent_left, a.exact, a.active, a.resets_at, a.note
                ))
                .collect::<Vec<_>>()
                .join(" ")
        );

        {
            let mut cache = state.usage.lock().unwrap();
            if cache.gen != gen {
                continue; // demo mode was toggled while polling: drop this result, poll again now
            }
            if !demo {
                cache.last_real = data.clone();
            }
            cache.data = data;
            let _ = app.emit("usage-updated", &cache.data); // under the lock: no stale emit can follow a newer one
        }

        // Sleep until the next poll, or until settings wake us (demo mode toggled).
        match wake.recv_timeout(if demo { DEMO_POLL } else { REAL_POLL }) {
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            _ => {}
        }
    });
}

// ---------- entry point ----------

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_usage,
            open_settings,
            resize_sidebar,
            drag_began
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory); // menu-bar app: no Dock icon

            let handle = app.handle().clone();
            let loaded = settings::load(&handle);
            let (tx, rx) = mpsc::channel();
            let state = Arc::new(AppState {
                store: Mutex::new(SettingsStore {
                    settings: loaded.settings,
                    needs_default_agents: loaded.needs_default_agents,
                    blocked: loaded.blocked.clone(),
                }),
                usage: Mutex::new(UsageCache { gen: 0, data: Vec::new(), last_real: Vec::new() }),
                wake_poller: Mutex::new(tx),
                move_gen: AtomicU64::new(0),
                drag_seen: Mutex::new(None),
            });
            app.manage(state.clone());

            build_tray(&handle, loaded.blocked.is_some())?;
            create_sidebar(&handle, &state)?;
            spawn_poller(handle, state, rx);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building AgentMeter")
        .run(|_app, event| {
            // Closing the settings window must not quit the menu-bar app; only "Quit" does.
            if let tauri::RunEvent::ExitRequested { code: None, api, .. } = event {
                api.prevent_exit();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_pulls_whole_sidebar_back_on_screen() {
        let screen = [(0.0, 25.0, 1440.0, 875.0)];
        // 708 px wide sidebar restored at x=1300 after a resolution drop: right edge must fit.
        assert_eq!(clamp_into(&screen, 1300.0, 600.0, 708.0, SIDEBAR_H), Some((732.0, 528.0)));
        assert_eq!(clamp_into(&screen, -50.0, 0.0, 200.0, SIDEBAR_H), Some((0.0, 25.0)));
        assert_eq!(clamp_into(&screen, 100.0, 100.0, 200.0, SIDEBAR_H), Some((100.0, 100.0)));
        assert_eq!(clamp_into(&screen, 5000.0, 100.0, 200.0, SIDEBAR_H), None, "on no screen → caller docks");
    }

    #[test]
    fn clamp_prefers_screen_with_most_overlap() {
        let screens = [(0.0, 0.0, 1440.0, 900.0), (1440.0, 0.0, 1920.0, 1080.0)];
        assert_eq!(clamp_into(&screens, 1400.0, 100.0, 364.0, SIDEBAR_H), Some((1440.0, 100.0)));
    }

    #[test]
    fn refreshing_rows_are_unknown_and_cover_enabled_ids() {
        let real = vec![AgentUsage {
            id: "codex".into(), name: "Codex".into(), installed: true, percent_left: Some(40.0),
            resets_at: None, window: Some("5h".into()), active: true, exact: true, note: None,
        }];
        let rows = refreshing(&real, &["codex".into(), "gemini".into()]);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|a| a.percent_left.is_none() && !a.active && a.installed));
        assert_eq!(rows[0].name, "Codex");
        assert_eq!(rows[1].id, "gemini");
    }
}
