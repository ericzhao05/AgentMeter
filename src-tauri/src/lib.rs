mod settings;
mod usage_stub;

// The one line to change when the real crate lands: `use agentmeter_usage as usage;`
use usage_stub as usage;

use settings::{DockEdge, Settings};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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

struct AppState {
    settings: Mutex<Settings>,
    usage: Mutex<Vec<AgentUsage>>,
    /// True until the first real poll seeds `agents` (only when no settings file existed).
    needs_default_agents: Mutex<bool>,
    wake_poller: Mutex<Sender<()>>,
    move_gen: AtomicU64,
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

fn sidebar_width(tubes: u32) -> f64 {
    PAD_X * 2.0 + COL_W * tubes.max(1) as f64
}

// ---------- commands ----------

#[tauri::command]
fn get_settings(state: tauri::State<'_, Arc<AppState>>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(app: AppHandle, state: tauri::State<'_, Arc<AppState>>, settings: Settings) -> Result<(), String> {
    let settings = settings.normalized();
    let old = state.settings.lock().unwrap().clone();
    settings::save(&app, &settings)?;
    *state.settings.lock().unwrap() = settings.clone();
    *state.needs_default_agents.lock().unwrap() = false;
    if settings.dock_edge != old.dock_edge {
        if let Some(w) = app.get_webview_window(SIDEBAR) {
            dock(&w, settings.dock_edge, None);
        }
    }
    if settings.demo_mode != old.demo_mode {
        let _ = state.wake_poller.lock().unwrap().send(());
    }
    app.emit("settings-changed", &settings).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_usage(state: tauri::State<'_, Arc<AppState>>) -> Vec<AgentUsage> {
    state.usage.lock().unwrap().clone()
}

#[tauri::command]
fn open_settings(app: AppHandle) -> Result<(), String> {
    show_settings(&app).map_err(|e| e.to_string())
}

/// The sidebar asks for a width that fits `tubes` columns; the docked edge stays put.
#[tauri::command]
fn resize_sidebar(window: WebviewWindow, state: tauri::State<'_, Arc<AppState>>, tubes: u32) -> Result<(), String> {
    let edge = state.settings.lock().unwrap().dock_edge;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let old_size = window.outer_size().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
    let pos = window.outer_position().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
    let w = sidebar_width(tubes);
    if (old_size.width - w).abs() < 0.5 && (old_size.height - SIDEBAR_H).abs() < 0.5 {
        return Ok(());
    }
    window.set_size(LogicalSize::new(w, SIDEBAR_H)).map_err(|e| e.to_string())?;
    if edge == DockEdge::Right {
        let x = pos.x + old_size.width - w;
        window.set_position(LogicalPosition::new(x, pos.y)).map_err(|e| e.to_string())?;
    }
    Ok(())
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

/// Is a saved logical position still on one of the connected screens?
fn on_some_screen(app: &AppHandle, x: f64, y: f64) -> bool {
    app.available_monitors().unwrap_or_default().iter().any(|m| {
        let (ax, ay, aw, ah) = work_area(m);
        x >= ax - 40.0 && x < ax + aw - 20.0 && y >= ay - 40.0 && y < ay + ah - 20.0
    })
}

fn create_sidebar(app: &AppHandle, state: &Arc<AppState>) -> tauri::Result<WebviewWindow> {
    let s = state.settings.lock().unwrap().clone();
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
        Some((x, y)) if on_some_screen(app, x, y) => {
            let _ = w.set_position(LogicalPosition::new(x, y));
        }
        _ => dock(&w, s.dock_edge, Some(width)),
    }
    w.show()?;

    // Remember where the user drags it (debounced: write once the window stops moving).
    let handle = app.clone();
    let st = state.clone();
    w.on_window_event(move |e| {
        if let WindowEvent::Moved(_) = e {
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
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "Show/Hide Sidebar", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit AgentMeter", true, Some("CmdOrCtrl+Q"))?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&toggle, &settings, &sep, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("AgentMeter")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, e| match e.id.as_ref() {
            "toggle" => toggle_sidebar(app),
            "settings" => {
                let _ = show_settings(app);
            }
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

// ---------- polling ----------

fn spawn_poller(app: AppHandle, state: Arc<AppState>, wake: mpsc::Receiver<()>) {
    std::thread::spawn(move || loop {
        let demo = state.settings.lock().unwrap().demo_mode;
        let data = if demo { usage::mock_all(now_secs()) } else { usage::poll_all() };

        // First run without a settings file: enable every installed agent, in detection order.
        if !demo {
            let mut needs = state.needs_default_agents.lock().unwrap();
            if *needs {
                *needs = false;
                let mut s = state.settings.lock().unwrap();
                s.agents = data.iter().filter(|a| a.installed).map(|a| a.id.clone()).collect();
                let s = s.clone();
                let _ = settings::save(&app, &s);
                let _ = app.emit("settings-changed", &s);
            }
        }

        *state.usage.lock().unwrap() = data.clone();
        let _ = app.emit("usage-updated", &data);

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
            resize_sidebar
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory); // menu-bar app: no Dock icon

            let handle = app.handle().clone();
            let (loaded, existed) = settings::load(&handle);
            let (tx, rx) = mpsc::channel();
            let state = Arc::new(AppState {
                settings: Mutex::new(loaded),
                usage: Mutex::new(Vec::new()),
                needs_default_agents: Mutex::new(!existed),
                wake_poller: Mutex::new(tx),
                move_gen: AtomicU64::new(0),
            });
            app.manage(state.clone());

            build_tray(&handle)?;
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
