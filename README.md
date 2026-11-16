# AgentMeter

A macOS desktop sidebar that shows, for each AI coding agent installed on your computer,
how much of its current usage limit is **left**, drawn as an animated glass tube.
Full tube = 100% of the limit available; empty = the current session/window limit is reached.

- `~62%` — an estimate (the agent doesn't expose an exact figure)
- `?` with a dimmed, empty tube — usage couldn't be read; AgentMeter never invents numbers
- Bubbling liquid — the agent is working right now
- Pouring refill — the limit window just reset

## Layout

| Path | What |
| --- | --- |
| `src-tauri/` | Tauri v2 shell (Rust): sidebar + settings windows, menu-bar tray, polling thread, settings persistence |
| `ui/` | Static frontend (no bundler): `sidebar.html`, `tube.js` (the tube component), settings screen |
| `crates/agentmeter-usage/` | Pure Rust library that detects agents and reads their usage (`poll_all()`, `mock_all(t)`) |

## Run

Requires Rust and the Tauri CLI (`cargo install tauri-cli --version '^2' --locked`).

```sh
cd src-tauri
cargo tauri dev        # run in development
cargo tauri build      # produce AgentMeter.app / .dmg
```

AgentMeter runs as a menu-bar app (no Dock icon). Use the tray menu for **Show/Hide Sidebar**,
**Settings…** and **Quit**. Drag the sidebar anywhere; its position is remembered.

## Settings

Stored as JSON in the app config dir (`~/Library/Application Support/com.agentmeter.app/settings.json`):

```json
{ "tubeCount": 4, "agents": ["claude", "codex"], "demoMode": false, "dockEdge": "right" }
```

Changes apply live: the sidebar resizes and reorders as soon as settings are saved.

## Frontend ↔ backend contract

- `invoke('get_usage')` → `AgentUsage[]` (latest cached poll, or demo data in demo mode)
- `invoke('get_settings')` → `Settings`; `invoke('save_settings', { settings })` persists and emits `settings-changed`
- `invoke('open_settings')`, `invoke('resize_sidebar', { tubes })` (sidebar internals)
- events: `usage-updated` (`AgentUsage[]`, every ~45 s, every 2 s in demo mode), `settings-changed` (`Settings`)
