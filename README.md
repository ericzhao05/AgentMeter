# AgentMeter

A macOS desktop sidebar that shows, for each AI coding agent installed on your computer,
how much of its current usage limit is **left**, drawn as an animated glass tube.
Full tube = 100% of the limit available; empty = the current session/window limit is reached.

Under each tube: the agent name, the percentage left and, when known, when the limit resets
(e.g. `Limit resets in 2h 15min`).

- `~62%` — an estimate (the agent doesn't expose an exact figure)
- Big `?` inside a dimmed, empty tube, labelled "Usage unavailable" — usage couldn't be read;
  AgentMeter never invents numbers (hover the tube for the reason)
- Bubbling liquid — the agent is working right now
- Pouring refill — the limit window just reset

## Layout

| Path | What |
| --- | --- |
| `src-tauri/` | Tauri v2 shell (Rust): sidebar + settings windows, menu-bar tray, polling thread, settings persistence |
| `ui/` | Static frontend (no bundler): `sidebar.html`, `tube.js` (the tube component), settings screen |
| `crates/agentmeter-usage/` | Pure Rust library that detects agents and reads their usage (`poll_all()`, `mock_all(t)`) |
| `assets/icons/` | App icon and menu-bar (template) icon artwork |

## Run

Requires Rust and the Tauri CLI (`cargo install tauri-cli --version '^2' --locked`).

```sh
cd src-tauri
cargo tauri dev        # run in development
cargo tauri build      # produce AgentMeter.app / .dmg
```

AgentMeter runs as a menu-bar app (no Dock icon). The tray menu has **Show/Hide Sidebar**,
**Settings…** and **Quit**.

### Using the sidebar

- **Reveal:** the tubes stay hidden just off the right edge of the screen. Push the mouse against the
  rightmost edge of the screen, level with the sidebar, and they slide in. Move away and they slide
  back out after about half a second. On launch (and after tray **Show**) they appear briefly so you
  know where they are.
- **Hover:** the sidebar has no background, just tubes and labels. While the mouse is over it, a light
  grey panel and the ⚙︎ settings button appear.
- **Move:** drag the sidebar up or down; it snaps back to the right edge and remembers its height
  across restarts. With several displays it docks to the right edge of the display you drop it on.
- **Off entirely:** tray **Show/Hide Sidebar** turns edge reveal off; choose it again to turn it back on.
- While hidden, the sidebar lets clicks through to the apps underneath and pauses its animation.
  With "Reduce motion" on, it appears and disappears without sliding.

### Demo mode (development)

Run on simulated usage instead of real data (no network or Keychain access) by setting an env var:

```sh
AGENTMETER_DEMO=1 cargo tauri dev
```

Demo data refreshes every 2 s and shows a small `demo` badge on the sidebar. There is no UI toggle;
without the variable AgentMeter always shows real usage (polled every ~45 s).

## Settings

Settings only choose **which agents are shown**: tick agents on or off. The sidebar shows one tube
per enabled agent (up to 8), in the saved order, and resizes live when you change the selection.
With nothing selected it shows "No agents selected — open Settings". On first run every installed
agent is enabled.

Stored as JSON in the app config dir (`~/Library/Application Support/com.agentmeter.app/settings.json`):

```json
{ "agents": ["claude-code", "codex"], "tubeCount": 4, "demoMode": false, "dockEdge": "right" }
```

`tubeCount`, `demoMode` and `dockEdge` are legacy fields kept for compatibility and ignored (demo mode
comes from `AGENTMETER_DEMO`; the sidebar always docks to the right edge). If the file is unreadable it is kept as `settings.json.corrupt-…`
before anything is written; if even that fails, saving is refused and the menu-bar icon offers
**Reset Settings**.

## Frontend ↔ backend contract

- `invoke('get_usage')` → `AgentUsage[]` (latest cached poll, or demo data in demo mode)
- `invoke('get_settings')` → `Settings`; `invoke('save_settings', { settings })` persists and emits `settings-changed`
- `invoke('open_settings')`, `invoke('resize_sidebar', { tubes })`, `invoke('drag_began')`, `invoke('get_sidebar_state')` (sidebar internals)
- events: `usage-updated` (`AgentUsage[]`, every ~45 s, every 2 s in demo mode), `settings-changed` (`Settings`), `sidebar-state` (`{ shown, hover }`, auto-hide)
