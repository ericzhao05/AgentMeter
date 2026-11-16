mod settings;

use agentmeter_usage as usage;

use serde::Serialize;
use settings::{DockEdge, Settings};
use std::sync::atomic::{AtomicBool, Ordering};
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
const SIDEBAR_H: f64 = 382.0; // toolbar 26 + tube 280 + label 66 + bottom 10

// Auto-hide: the sidebar lives flush against the right edge of its screen's work area and slides in
// when the cursor touches that edge, out again once the cursor has left it for HIDE_AFTER.
const EDGE_POLL: Duration = Duration::from_millis(90); // cursor check interval (cheap)
const EDGE_HIT: f64 = 3.0; // logical px from the right edge that count as "touching" it
const EDGE_SLACK_Y: f64 = 40.0; // vertical slack above/below the sidebar for the edge hit
const HIDE_AFTER: Duration = Duration::from_millis(600);
const LAUNCH_REVEAL: Duration = Duration::from_secs(2); // brief reveal on launch / tray Show
const GLIDE: Duration = Duration::from_millis(220); // drop → edge glide (ease-out)
const GLIDE_STEP: Duration = Duration::from_millis(8); // ~120 Hz position steps

/// Settings plus the first-run flag behind ONE mutex. Every settings write holds it across
/// update + persist + emit, so there is a single persistence owner and writes can't interleave.
struct SettingsStore {
    settings: Settings,
    /// True until the first real poll seeds `agents` (no usable list was on disk).
    needs_default_agents: bool,
    /// An invalid settings file couldn't be preserved: refuse writes until it can be (or Reset).
    blocked: Option<String>,
    /// The file's own (legacy) demoMode. In memory demo_mode mirrors AGENTMETER_DEMO; on disk we
    /// keep whatever the file had, so a dev demo run never rewrites it.
    file_demo_mode: bool,
}

impl SettingsStore {
    fn write(&self, app: &AppHandle, s: &Settings) -> Result<(), String> {
        settings::save(app, &Settings { demo_mode: self.file_demo_mode, ..s.clone() })
    }
}

/// Latest usage snapshot (published under its lock so emits stay in order).
struct UsageCache {
    data: Vec<AgentUsage>,
}

struct AppState {
    /// Demo mode, fixed at launch by the AGENTMETER_DEMO env var (development only; no UI toggle).
    demo: bool,
    store: Mutex<SettingsStore>,
    usage: Mutex<UsageCache>,
    wake_poller: Mutex<Sender<()>>,
    /// Tray menu + its "Reset Settings" item, present only while settings writes are blocked.
    reset_item: Mutex<Option<(Menu<tauri::Wry>, MenuItem<tauri::Wry>)>>,
    /// Last window movement (every Move event, plus the sidebar's drag start).
    drag_seen: Mutex<Option<Instant>>,
    /// A scale/monitor change arrived mid-drag; a deferred re-fit is scheduled.
    refit_after_drag: AtomicBool,
    /// Auto-hide state of the sidebar (see `spawn_edge_watcher`).
    vis: Mutex<Visibility>,
    /// The window is gliding to its edge after a drop (its own Move events are not a new drag).
    gliding: AtomicBool,
}

/// What the sidebar UI needs to know: slid in or out, and whether the cursor is over it.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SidebarState {
    shown: bool,
    hover: bool,
    /// Which screen edge it is docked to (slide direction, header layout).
    edge: DockEdge,
}

struct Visibility {
    /// false = disabled from the tray ("Hide"): no edge reveal at all.
    enabled: bool,
    ui: SidebarState,
    /// Last time the cursor was over the (shown) sidebar.
    last_inside: Instant,
    /// Stay shown at least until then (launch / tray Show reveal).
    reveal_until: Option<Instant>,
}

const DRAG_QUIET: Duration = Duration::from_millis(600);

impl AppState {
    fn recently_moved(&self) -> bool {
        self.drag_seen.lock().unwrap().is_some_and(|t| t.elapsed() < DRAG_QUIET)
    }

    /// A user window drag may be in progress. On macOS a drag always holds a mouse button, which
    /// also covers pausing mid-drag; elsewhere fall back to "moved within the quiet window".
    fn user_dragging(&self) -> bool {
        if cfg!(target_os = "macos") { mouse_button_down() } else { self.recently_moved() }
    }
}

/// Is any mouse button currently held (system-wide)? Cheap NSEvent class query.
#[cfg(target_os = "macos")]
fn mouse_button_down() -> bool {
    objc2_app_kit::NSEvent::pressedMouseButtons() != 0
}
#[cfg(not(target_os = "macos"))]
fn mouse_button_down() -> bool {
    false
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

/// AGENTMETER_DEMO=1 (or true/yes) runs on mock data instead of real usage.
fn demo_from_env() -> bool {
    std::env::var("AGENTMETER_DEMO").is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// Async so the file write + fsync never runs on the main thread; persistence itself is
/// serialized by the settings mutex on a blocking worker.
#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    // demoMode is legacy: in memory it mirrors the env var (the file keeps its own value).
    let settings = Settings { demo_mode: state.demo, ..settings.normalized() };
    let (a, st, s) = (app.clone(), state.clone(), settings.clone());
    tauri::async_runtime::spawn_blocking(move || persist(&a, &st, s)).await.map_err(|e| e.to_string())??;
    Ok(())
}

/// Write settings (single owner: holds the settings mutex across check, write and emit).
/// Refuses while an unpreserved invalid file is on disk.
fn persist(app: &AppHandle, state: &AppState, settings: Settings) -> Result<(), String> {
    let mut store = state.store.lock().unwrap();
    if let Some(msg) = store.blocked.clone() {
        match settings::preserve_existing(app) {
            Ok(kept) => {
                eprintln!("[agentmeter] settings file preserved at {kept:?}; writes unblocked");
                store.blocked = None;
                remove_reset_item(app, state);
            }
            Err(_) => return Err(msg),
        }
    }
    // Demo run on a first-ever launch: the list started from mock agents, so keep changes in memory
    // only and leave first-run detection pending for the next real launch.
    if !(state.demo && store.needs_default_agents) {
        store.write(app, &settings)?;
        store.needs_default_agents = false;
    }
    store.settings = settings.clone();
    app.emit("settings-changed", &settings).map_err(|e| e.to_string())
}

/// Explicit user reset (tray item, offered only when settings writes are blocked): overwrite
/// the unpreservable file with defaults and re-seed the agent list from the next real poll.
/// A no-op unless writes are still blocked (a stale menu item can't wipe a working setup).
fn reset_settings(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut store = state.store.lock().unwrap();
        if store.blocked.is_none() {
            remove_reset_item(&app, &state);
            return;
        }
        let fresh = Settings { demo_mode: state.demo, ..Settings::default() };
        let old_file_demo = store.file_demo_mode;
        store.file_demo_mode = false;
        match store.write(&app, &fresh) {
            Ok(()) => {
                store.blocked = None;
                store.needs_default_agents = true;
                store.settings = fresh;
                let _ = app.emit("settings-changed", &store.settings);
                let _ = state.wake_poller.lock().unwrap().send(());
                // Demo mode is env-only and the sidebar is always on the right edge: nothing else
                // to re-apply.
                remove_reset_item(&app, &state);
            }
            Err(e) => {
                store.file_demo_mode = old_file_demo;
                eprintln!("[agentmeter] reset failed: {e}");
            }
        }
    });
}

/// Take the "Reset Settings" item out of the tray menu (once writes are unblocked).
fn remove_reset_item(app: &AppHandle, state: &AppState) {
    if let Some((menu, item)) = state.reset_item.lock().unwrap().take() {
        let _ = app.run_on_main_thread(move || {
            let _ = menu.remove(&item);
        });
    }
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
/// it stays flush against the right edge. Never repositions mid-drag.
#[tauri::command]
fn resize_sidebar(window: WebviewWindow, state: tauri::State<'_, Arc<AppState>>, tubes: u32) -> Result<(), String> {
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let old_size = window.outer_size().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
    let w = sidebar_width(tubes.min(settings::MAX_TUBES as u32));
    if (old_size.width - w).abs() < 0.5 && (old_size.height - SIDEBAR_H).abs() < 0.5 {
        return Ok(()); // unchanged: leave the position alone (it may span monitors on purpose)
    }
    window.set_size(LogicalSize::new(w, SIDEBAR_H)).map_err(|e| e.to_string())?;
    if state.user_dragging() {
        // A button is held (maybe a drag): don't move it now, re-fit once it's released.
        if !state.refit_after_drag.swap(true, Ordering::SeqCst) {
            schedule_refit(window.app_handle().clone(), state.inner().clone());
        }
        return Ok(());
    }
    let edge = state.vis.lock().unwrap().ui.edge;
    snap_to_edge(&window, None, edge);
    Ok(())
}

/// The sidebar is about to start a native window drag (mousedown on the panel).
#[tauri::command]
fn drag_began(state: tauri::State<'_, Arc<AppState>>) {
    *state.drag_seen.lock().unwrap() = Some(Instant::now());
}

// ---------- windows ----------

/// Debug builds: lets the sidebar report measurements (e.g. slide frame timing) to stderr.
#[tauri::command]
fn dev_log(msg: String) {
    #[cfg(debug_assertions)]
    eprintln!("[agentmeter/ui] {msg}");
    let _ = msg;
}

#[tauri::command]
fn get_sidebar_state(state: tauri::State<'_, Arc<AppState>>) -> SidebarState {
    state.vis.lock().unwrap().ui
}

fn monitor_for(w: &WebviewWindow) -> Option<Monitor> {
    w.current_monitor().ok().flatten().or_else(|| w.primary_monitor().ok().flatten())
}

/// Logical work area (x, y, w, h) of a monitor: the screen minus menu bar and Dock.
fn work_area(m: &Monitor) -> (f64, f64, f64, f64) {
    let s = m.scale_factor();
    let a = m.work_area();
    (a.position.x as f64 / s, a.position.y as f64 / s, a.size.width as f64 / s, a.size.height as f64 / s)
}

/// Docked position: flush with the left/right edge of `area`, `y` clamped into it (None = centred).
fn docked_pos((ax, ay, aw, ah): (f64, f64, f64, f64), width: f64, y: Option<f64>, edge: DockEdge) -> (f64, f64) {
    let y = y.unwrap_or(ay + (ah - SIDEBAR_H) / 2.0);
    let x = match edge {
        DockEdge::Right => ax + aw - width,
        DockEdge::Left => ax,
    };
    (x, y.min(ay + ah - SIDEBAR_H).max(ay))
}

/// Which edge a sidebar dropped at x (width w) belongs to: the half of the screen its centre is in.
fn edge_for_drop((ax, _, aw, _): (f64, f64, f64, f64), x: f64, w: f64) -> DockEdge {
    if x + w / 2.0 < ax + aw / 2.0 { DockEdge::Left } else { DockEdge::Right }
}

/// Put the sidebar flush against an edge of the screen it is on, keeping its vertical position
/// (or `y`), clamped to the work area. A window on no screen lands on the primary one.
fn snap_to_edge(w: &WebviewWindow, y: Option<f64>, edge: DockEdge) {
    let Some(m) = monitor_for(w) else { return };
    let s = w.scale_factor().unwrap_or(1.0);
    let cur = w.outer_position().ok().map(|p| p.to_logical::<f64>(s));
    let width = w.outer_size().map(|z| z.to_logical::<f64>(s).width).unwrap_or(COL_W);
    let (x, y) = docked_pos(work_area(&m), width, y.or(cur.map(|c| c.y)), edge);
    if cur.is_none_or(|c| (c.x - x).abs() >= 0.5 || (c.y - y).abs() >= 0.5) {
        let _ = w.set_position(LogicalPosition::new(x, y));
    }
}

fn create_sidebar(app: &AppHandle, state: &Arc<AppState>) -> tauri::Result<WebviewWindow> {
    let s = state.store.lock().unwrap().settings.clone();
    let width = sidebar_width(s.agents.len().clamp(1, settings::MAX_TUBES as usize) as u32);
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
        .accept_first_mouse(true) // the gear works on the first click even though we never take focus
        .visible(false)
        .build()?;

    // The edge comes from settings (dockEdge); only the vertical position is used from window.json.
    snap_to_edge(&w, settings::load_position(app).map(|(_, y)| y), s.dock_edge);
    w.show()?;

    // One long-lived thread, fed a tick per Move event. As soon as the button is released (checked
    // every 40 ms, so the release is caught quickly) the sidebar glides to the nearer edge.
    let (move_tx, move_rx) = mpsc::channel::<()>();
    let saver = app.clone();
    let st_saver = state.clone();
    std::thread::spawn(move || loop {
        if move_rx.recv().is_err() {
            return;
        }
        loop {
            match move_rx.recv_timeout(Duration::from_millis(40)) {
                Ok(()) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) if mouse_button_down() => continue, // still dragging
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        finish_drag(&saver, &st_saver);
        while move_rx.try_recv().is_ok() {} // the glide's own Move events
    });
    let handle = app.clone();
    let st = state.clone();
    w.on_window_event(move |e| {
        // Monitor / scale change (e.g. moved to another display, resolution changed): re-dock.
        // Never during a user drag: remember it and re-dock once the drag ends.
        if let WindowEvent::ScaleFactorChanged { .. } = e {
            if st.recently_moved() || st.user_dragging() {
                if !st.refit_after_drag.swap(true, Ordering::SeqCst) {
                    schedule_refit(handle.clone(), st.clone());
                }
            } else {
                refit_sidebar(&handle);
            }
        }
        if let WindowEvent::Moved(_) = e {
            if !st.gliding.load(Ordering::SeqCst) {
                *st.drag_seen.lock().unwrap() = Some(Instant::now()); // every move renews the marker
                let _ = move_tx.send(());
            }
        }
    });
    Ok(w)
}

/// Deferred re-dock after a mid-drag scale change: wait out the quiet window, then on the main
/// thread re-check that the window is still and no button is held; otherwise try again later.
fn schedule_refit(app: AppHandle, st: Arc<AppState>) {
    std::thread::spawn(move || {
        std::thread::sleep(DRAG_QUIET + Duration::from_millis(50));
        let (h, s2) = (app.clone(), st.clone());
        let _ = app.run_on_main_thread(move || {
            if s2.recently_moved() || s2.user_dragging() {
                schedule_refit(h, s2);
            } else {
                s2.refit_after_drag.store(false, Ordering::SeqCst);
                refit_sidebar(&h);
            }
        });
    });
}

/// Re-dock the sidebar on the right edge of its current screen (after a scale/monitor change).
fn refit_sidebar(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(SIDEBAR) {
        let edge = app.state::<Arc<AppState>>().vis.lock().unwrap().ui.edge;
        snap_to_edge(&w, None, edge);
    }
}

/// Ease-out cubic.
fn ease_out(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// Drag released: pick the edge from the half of the screen the sidebar was dropped in, glide the
/// window there in one continuous ease-out motion (instead of an instant jump), remember edge +
/// height, and let it slide out right away if the cursor isn't over it.
fn finish_drag(app: &AppHandle, st: &Arc<AppState>) {
    let Some(w) = app.get_webview_window(SIDEBAR) else { return };
    let (Some(m), Ok(sf), Ok(p), Ok(sz)) = (monitor_for(&w), w.scale_factor(), w.outer_position(), w.outer_size()) else {
        return;
    };
    let (p, sz) = (p.to_logical::<f64>(sf), sz.to_logical::<f64>(sf));
    let area = work_area(&m);
    let edge = edge_for_drop(area, p.x, sz.width);
    let (tx, ty) = docked_pos(area, sz.width, Some(p.y), edge);
    let (dx, dy) = (tx - p.x, ty - p.y);
    if dx.abs() >= 0.5 || dy.abs() >= 0.5 {
        st.gliding.store(true, Ordering::SeqCst);
        let t0 = Instant::now();
        let (mut last, mut max_gap, mut steps) = (t0, Duration::ZERO, 0u32);
        loop {
            let t = t0.elapsed().as_secs_f64() / GLIDE.as_secs_f64();
            let e = ease_out(t);
            let _ = w.set_position(LogicalPosition::new(p.x + dx * e, p.y + dy * e));
            steps += 1;
            max_gap = max_gap.max(last.elapsed());
            last = Instant::now();
            if t >= 1.0 {
                break;
            }
            std::thread::sleep(GLIDE_STEP);
        }
        std::thread::sleep(Duration::from_millis(30)); // let the last Move events arrive
        st.gliding.store(false, Ordering::SeqCst);
        #[cfg(debug_assertions)]
        eprintln!("[agentmeter] glide: {steps} steps in {:?}, max step gap {max_gap:?}, edge={edge:?}", t0.elapsed());
    }
    settings::save_position(app, tx, ty);
    {
        let mut vis = st.vis.lock().unwrap();
        vis.last_inside = Instant::now() - HIDE_AFTER; // hide on the next tick unless the cursor is over it
        let ui = SidebarState { edge, ..vis.ui };
        set_ui(app, &w, &mut vis, ui);
    }
    let current = st.store.lock().unwrap().settings.clone();
    if current.dock_edge != edge {
        if let Err(e) = persist(app, st, Settings { dock_edge: edge, ..current }) {
            eprintln!("[agentmeter] could not save dock edge: {e}");
        }
    }
}

fn show_settings(app: &AppHandle) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window(SETTINGS) {
        w.show()?;
        w.unminimize()?;
        return w.set_focus();
    }
    let w = WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("settings.html".into()))
        .title("AgentMeter Settings")
        .inner_size(360.0, 420.0)
        .resizable(false)
        .maximizable(false)
        .center()
        .focused(true)
        .build()?;
    w.set_focus()
}

/// Apply a new show/hover state: hidden windows ignore the cursor (clicks reach the apps below).
fn set_ui(app: &AppHandle, w: &WebviewWindow, vis: &mut Visibility, ui: SidebarState) {
    if vis.ui == ui {
        return;
    }
    if vis.ui.shown != ui.shown {
        let _ = w.set_ignore_cursor_events(!ui.shown);
    }
    vis.ui = ui;
    #[cfg(debug_assertions)]
    eprintln!("[agentmeter] sidebar shown={} hover={}", ui.shown, ui.hover);
    let _ = app.emit("sidebar-state", ui);
}

/// Tray "Show/Hide Sidebar": Hide disables the sidebar entirely (no edge reveal); Show re-enables
/// auto-hide with a brief reveal so it's easy to find.
fn toggle_sidebar(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let Some(w) = app.get_webview_window(SIDEBAR) else { return };
    let mut vis = state.vis.lock().unwrap();
    if vis.enabled {
        vis.enabled = false;
        let edge = vis.ui.edge;
        set_ui(app, &w, &mut vis, SidebarState { shown: false, hover: false, edge });
        let _ = w.hide();
    } else {
        vis.enabled = true;
        vis.reveal_until = Some(Instant::now() + LAUNCH_REVEAL);
        vis.last_inside = Instant::now();
        let _ = w.show();
        let edge = vis.ui.edge;
        set_ui(app, &w, &mut vis, SidebarState { shown: true, hover: false, edge });
    }
    #[cfg(debug_assertions)]
    eprintln!("[agentmeter] sidebar enabled={}", vis.enabled);
}

/// Cursor geometry for one edge-watcher tick, all in the sidebar monitor's logical points.
struct EdgeInput {
    cursor: (f64, f64),
    /// Sidebar window rect (x, y, w, h).
    win: (f64, f64, f64, f64),
    /// Left / right edges of the sidebar's screen work area (where the sidebar docks).
    work_left: f64,
    work_right: f64,
    /// Left / right edges of the whole screen (beyond the work area if the Dock is on that side).
    screen_left: f64,
    screen_right: f64,
}

/// Is the cursor touching the docked screen edge, level with the sidebar (some vertical slack)?
fn at_reveal_edge(i: &EdgeInput, edge: DockEdge) -> bool {
    let (cx, cy) = i.cursor;
    let (_, wy, _, wh) = i.win;
    let level = cy >= wy - EDGE_SLACK_Y && cy <= wy + wh + EDGE_SLACK_Y;
    let at_edge = match edge {
        DockEdge::Right => cx >= i.work_right - EDGE_HIT && cx <= i.screen_right.max(i.work_right) + 1.0,
        DockEdge::Left => cx <= i.work_left + EDGE_HIT && cx >= i.screen_left.min(i.work_left) - 1.0,
    };
    level && at_edge
}

fn cursor_inside(i: &EdgeInput) -> bool {
    let (cx, cy) = i.cursor;
    let (wx, wy, ww, wh) = i.win;
    cx >= wx - 2.0 && cx <= wx + ww + 2.0 && cy >= wy - 2.0 && cy <= wy + wh + 2.0
}

/// Background thread: ~11 cheap cursor checks per second drive reveal / hide / hover.
fn spawn_edge_watcher(app: AppHandle, state: Arc<AppState>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(EDGE_POLL);
        let Some(w) = app.get_webview_window(SIDEBAR) else { continue };
        if !state.vis.lock().unwrap().enabled {
            continue;
        }
        let (Ok(cur), Ok(pos), Ok(size), Some(m)) = (app.cursor_position(), w.outer_position(), w.outer_size(), monitor_for(&w))
        else {
            continue;
        };
        // Physical → this monitor's logical points (cursor and window share the desktop space).
        let sf = m.scale_factor();
        let (ax, _, aw, _) = work_area(&m);
        let input = EdgeInput {
            cursor: (cur.x / sf, cur.y / sf),
            win: (pos.x as f64 / sf, pos.y as f64 / sf, size.width as f64 / sf, size.height as f64 / sf),
            work_left: ax,
            work_right: ax + aw,
            screen_left: m.position().x as f64 / sf,
            screen_right: (m.position().x as f64 + m.size().width as f64) / sf,
        };
        let settings_open = app.get_webview_window(SETTINGS).is_some_and(|s| s.is_visible().unwrap_or(false));
        let now = Instant::now();
        let mut vis = state.vis.lock().unwrap();
        if !vis.enabled {
            continue;
        }
        let inside = cursor_inside(&input);
        let mut ui = vis.ui;
        if !ui.shown && !state.gliding.load(Ordering::SeqCst) && at_reveal_edge(&input, ui.edge) {
            ui.shown = true;
            vis.last_inside = now;
        }
        if ui.shown {
            if inside {
                vis.last_inside = now;
            }
            let held = vis.reveal_until.is_some_and(|t| now < t)
                || now.duration_since(vis.last_inside) < HIDE_AFTER
                || mouse_button_down() // mid-drag (or clicking): never hide
                || state.gliding.load(Ordering::SeqCst)
                || settings_open;
            if !held {
                ui.shown = false;
                vis.reveal_until = None;
            }
        }
        ui.hover = ui.shown && inside;
        set_ui(&app, &w, &mut vis, ui);
    });
}

/// Returns the menu + "Reset Settings" item when that item was added (settings writes blocked).
fn build_tray(app: &AppHandle, settings_blocked: bool) -> tauri::Result<Option<(Menu<tauri::Wry>, MenuItem<tauri::Wry>)>> {
    let toggle = MenuItem::with_id(app, "toggle", "Show/Hide Sidebar", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit AgentMeter", true, Some("CmdOrCtrl+Q"))?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&toggle, &settings, &sep, &quit])?;
    let mut reset_handles = None;
    if settings_blocked {
        // Only offered while the settings file is unreadable and couldn't be backed up.
        let reset = MenuItem::with_id(app, "reset", "Reset Settings (settings file unreadable)", true, None::<&str>)?;
        menu.insert(&reset, 2)?;
        reset_handles = Some((menu.clone(), reset));
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
    Ok(reset_handles)
}

// ---------- polling ----------

fn spawn_poller(app: AppHandle, state: Arc<AppState>, wake: mpsc::Receiver<()>) {
    std::thread::spawn(move || loop {
        while wake.try_recv().is_ok() {} // wakes sent before this poll starts are covered by it
        let demo = state.demo;
        let data = if demo { usage::mock_all(launch_secs()) } else { usage::poll_all() };

        // Demo run with no agent list yet: show the mock agents in memory. While first-run detection
        // is pending, `persist` keeps even Settings toggles in memory, so demo data never becomes the
        // real first-run defaults.
        if demo {
            let mut store = state.store.lock().unwrap();
            if store.needs_default_agents && store.settings.agents.is_empty() {
                store.settings.agents = data.iter().filter(|a| a.installed).map(|a| a.id.clone()).collect();
                let _ = app.emit("settings-changed", &store.settings);
            }
        }
        // First run (no usable agent list on disk): enable every installed agent, in detection order.
        if !demo {
            let mut store = state.store.lock().unwrap();
            if store.needs_default_agents {
                store.needs_default_agents = false;
                store.settings.agents = data.iter().filter(|a| a.installed).map(|a| a.id.clone()).collect();
                // While blocked, show the defaults but never write over the unpreserved file.
                if store.blocked.is_none() {
                    if let Err(e) = store.write(&app, &store.settings) {
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
            cache.data = data;
            let _ = app.emit("usage-updated", &cache.data); // under the lock: no stale emit can follow a newer one
        }

        // Sleep until the next poll, or until woken (settings reset re-seeds the agent list).
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
            drag_began,
            get_sidebar_state,
            dev_log
        ])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory); // menu-bar app: no Dock icon

            let handle = app.handle().clone();
            let demo = demo_from_env();
            let mut loaded = settings::load(&handle);
            let dock_edge = loaded.settings.dock_edge;
            let file_demo_mode = loaded.settings.demo_mode;
            loaded.settings.demo_mode = demo; // legacy field: in memory it reflects the env var
            let (tx, rx) = mpsc::channel();
            let state = Arc::new(AppState {
                demo,
                store: Mutex::new(SettingsStore {
                    settings: loaded.settings,
                    needs_default_agents: loaded.needs_default_agents,
                    blocked: loaded.blocked.clone(),
                    file_demo_mode,
                }),
                usage: Mutex::new(UsageCache { data: Vec::new() }),
                wake_poller: Mutex::new(tx),
                reset_item: Mutex::new(None),
                drag_seen: Mutex::new(None),
                refit_after_drag: AtomicBool::new(false),
                gliding: AtomicBool::new(false),
                vis: Mutex::new(Visibility {
                    enabled: true,
                    ui: SidebarState { shown: true, hover: false, edge: dock_edge },
                    last_inside: Instant::now(),
                    reveal_until: Some(Instant::now() + LAUNCH_REVEAL),
                }),
            });
            app.manage(state.clone());

            *state.reset_item.lock().unwrap() = build_tray(&handle, loaded.blocked.is_some())?;
            create_sidebar(&handle, &state)?;
            spawn_edge_watcher(handle.clone(), state.clone());
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
    fn docks_flush_right_and_clamps_vertically() {
        let area = (0.0, 25.0, 1440.0, 875.0);
        let r = DockEdge::Right;
        assert_eq!(docked_pos(area, 364.0, Some(300.0), r), (1076.0, 300.0));
        assert_eq!(docked_pos(area, 364.0, Some(800.0), r), (1076.0, 25.0 + 875.0 - SIDEBAR_H), "kept above the bottom");
        assert_eq!(docked_pos(area, 364.0, Some(-50.0), r), (1076.0, 25.0), "kept below the menu bar");
        assert_eq!(docked_pos(area, 364.0, None, r).1, 25.0 + (875.0 - SIDEBAR_H) / 2.0, "centred by default");
        assert_eq!(docked_pos(area, 364.0, Some(300.0), DockEdge::Left), (0.0, 300.0));
        // Second screen to the right: docks to that screen's edges.
        assert_eq!(docked_pos((1440.0, 0.0, 1920.0, 1080.0), 278.0, Some(100.0), r), (3082.0, 100.0));
        assert_eq!(docked_pos((1440.0, 0.0, 1920.0, 1080.0), 278.0, Some(100.0), DockEdge::Left), (1440.0, 100.0));
    }

    #[test]
    fn drop_side_picks_the_edge() {
        let area = (0.0, 25.0, 1920.0, 1055.0);
        assert_eq!(edge_for_drop(area, 100.0, 278.0), DockEdge::Left);
        assert_eq!(edge_for_drop(area, 820.0, 278.0), DockEdge::Left, "centre at 959 < 960");
        assert_eq!(edge_for_drop(area, 822.0, 278.0), DockEdge::Right);
        assert_eq!(edge_for_drop((1920.0, 0.0, 1728.0, 1117.0), 2000.0, 278.0), DockEdge::Left, "left half of a second screen");
    }

    #[test]
    fn glide_eases_out() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert!(ease_out(0.5) > 0.8, "most of the distance early");
        assert_eq!(ease_out(2.0), 1.0, "clamped");
    }

    fn input(cx: f64, cy: f64) -> EdgeInput {
        EdgeInput { cursor: (cx, cy), win: (1556.0, 300.0, 364.0, SIDEBAR_H), work_left: 0.0, work_right: 1920.0, screen_left: 0.0, screen_right: 1920.0 }
    }

    #[test]
    fn reveal_needs_the_right_edge_level_with_the_sidebar() {
        let r = DockEdge::Right;
        assert!(at_reveal_edge(&input(1919.0, 400.0), r));
        assert!(at_reveal_edge(&input(1917.5, 300.0 - 30.0), r), "small vertical slack");
        assert!(!at_reveal_edge(&input(1900.0, 400.0), r), "not at the edge");
        assert!(!at_reveal_edge(&input(1919.0, 900.0), r), "edge, but far below the sidebar");
        assert!(!at_reveal_edge(&input(1950.0, 400.0), r), "on the next screen to the right");
        assert!(!at_reveal_edge(&input(1919.0, 400.0), DockEdge::Left), "wrong edge for a left dock");
        assert!(at_reveal_edge(&input(0.5, 400.0), DockEdge::Left));
        assert!(!at_reveal_edge(&input(-30.0, 400.0), DockEdge::Left), "on a screen to the left");
        // Dock on the right: the screen edge is past the work area; pushing against it still reveals.
        let docked = EdgeInput { cursor: (1999.0, 400.0), win: (1500.0, 300.0, 364.0, SIDEBAR_H), work_left: 0.0, work_right: 1864.0, screen_left: 0.0, screen_right: 2000.0 };
        assert!(at_reveal_edge(&docked, r));
    }

    #[test]
    fn hover_is_the_window_rect() {
        assert!(cursor_inside(&input(1600.0, 400.0)));
        assert!(!cursor_inside(&input(1500.0, 400.0)));
        assert!(!cursor_inside(&input(1600.0, 300.0 + SIDEBAR_H + 10.0)));
    }
}
