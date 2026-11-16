# agentmeter-usage

Pure-Rust data layer for AgentMeter. For each known AI coding agent it reports
how much of the **current usage-limit window is left** (100 = fully available,
0 = limit reached). No Tauri dependency; the app links it as a path dependency.

## API

```rust
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsage {
    pub id: String,                // "claude-code" | "codex" | "gemini" | "cursor" | "opencode" | "aider"
    pub name: String,
    pub installed: bool,
    pub percent_left: Option<f64>, // 0..=100, None = unknown
    pub resets_at: Option<String>, // RFC3339
    pub window: Option<String>,    // e.g. "5h"
    pub active: bool,              // newest log write within ~2 min
    pub exact: bool,               // false = estimate
    pub note: Option<String>,
}

pub fn poll_all() -> Vec<AgentUsage>;          // real data; sync; never panics
pub fn mock_all(t: f64) -> Vec<AgentUsage>;    // deterministic demo data
pub fn poll_all_at(home: &Path) -> Vec<AgentUsage>; // file sources only, custom $HOME (no Keychain/network)
```

`poll_all` always returns one row per known agent, in a fixed order, whether or
not it is installed. Adapter errors become a `note` with `percent_left: None`.
Panics inside an adapter are caught.

`poll_all` blocks the calling thread. Most calls finish in well under 100 ms.
The exception is a Claude Code cache miss (about once every 60 s), which makes
an HTTP call with a 5 s timeout and may also scan logs. From Tauri, call it
from a background thread or `spawn_blocking`, not from the main thread.

`mock_all(t)`: Claude Code drains from 100% to 10% over 90 s and then resets to
100%. Codex oscillates around 60%. Gemini stays low (6–13%). Cursor is
installed but unknown. opencode and aider are not installed.

```sh
cargo test
cargo run --example poll              # real poll, JSON (contains no secrets)
cargo run --example poll -- --offline # file-based sources only
cargo run --example poll -- --mock 42 # mock data at t = 42 s
```

## Per-agent sources

| Agent | Data | Source |
|---|---|---|
| Claude Code | **exact** (estimate as fallback) | Anthropic OAuth usage API; fallback: `~/.claude/projects/**/*.jsonl` |
| Codex | **exact** | `~/.codex/sessions/YYYY/MM/DD/*.jsonl` `token_count` events |
| Gemini CLI | unknown | none available locally |
| Cursor | unknown | none available locally |
| opencode, Aider | unknown | uses your own API keys, so there is no single usage-limit window |

### Codex

Each turn, Codex appends an `event_msg` with
`payload.type == "token_count"` and
`payload.rate_limits.{primary,secondary}.{used_percent, window_minutes, resets_at}`.
`primary` is the 5-hour window and `secondary` is the weekly window.
`resets_at` is epoch seconds. Older CLIs used `resets_in_seconds`, relative
to the event timestamp, and both forms are supported.

The adapter reads the last such event in the newest session files (up to the
20 newest).

- `percent_left = 100 - primary.used_percent`, and `exact = true`.
- If `resets_at` is already in the past, the window has reset since the last
  reading. It then reports 100% and says so in the note.
- The weekly window's remainder is shown in the note.

Limitation: the value is only as fresh as the last Codex turn.

### Claude Code (5-hour session limit)

1. **Exact source.** `GET https://api.anthropic.com/api/oauth/usage` with
   `anthropic-beta: oauth-2025-04-20`. This is the endpoint behind Claude
   Code's `/usage` screen. The response has
   `five_hour.utilization` (percent used) and `five_hour.resets_at`.
   - Auth uses the OAuth access token Claude Code keeps in the macOS Keychain
     (generic password, service `Claude Code-credentials`), read with
     `/usr/bin/security find-generic-password -w`. On Linux it uses
     `~/.claude/.credentials.json` instead.
   - The token is read-only. It is never refreshed or rewritten, never logged,
     and never sent anywhere except api.anthropic.com. The secret is wrapped
     in a type whose `Debug` output is redacted, and error messages contain
     only HTTP status codes.
   - If the stored token has expired, the adapter does not refresh it. It
     falls back to the estimate, and the note asks you to open Claude Code.
   - Results are cached for 60 s and use a 5 s HTTP timeout.
2. **Estimate fallback.** This runs when the API is unavailable (not logged in
   with a subscription, offline, HTTP error), and also in `poll_all_at`.
   - It rebuilds 5-hour session blocks from assistant `message.usage` records
     in `~/.claude/projects/**/*.jsonl` from the last 8 days, deduplicated by
     message id and request id.
   - A block starts at the minute of its first message. Tokens counted are
     input + output + cache-creation; cache reads are excluded.
   - The limit comes from one of two places:
     - **Calibrated**: a token budget derived in-process from the last
       successful exact reading (tokens used ÷ fraction used).
     - Otherwise, **peak proxy**: the busiest completed block in the history.
   - If the current block is busier than the peak, it returns unknown rather
     than a made-up 0%.
   - Always `exact = false`, with a note explaining what was compared.

   Limitations:
   - Anthropic doesn't publish token limits.
   - Usage from claude.ai or other devices shares the same limit but never
     appears in local logs.
   - Window boundaries can differ by a few minutes from the server's.

### Gemini CLI

`~/.gemini` contains settings and per-project chat logs (`tmp/`). Gemini
CLI's quota is only available from Google's servers through its
authenticated session. Nothing on disk records remaining quota, so the
adapter reports `installed`, `active` (from `~/.gemini/tmp` mtimes), and
`percent_left: None` with a note.

### Cursor

Detected by `~/.cursor` or `/Applications/Cursor.app`. Activity comes from
`~/.cursor/projects` and `~/.cursor/ai-tracking` mtimes. Cursor's usage and
limits live only on cursor.com behind the web-session cookie, and none of
the local state files record quota. Reading that cookie and sending it to
cursor.com would go beyond this crate's read-only, local scope, so Cursor
reports unknown.

### opencode / Aider

Detected by `~/.local/share/opencode` or `~/.config/opencode`, and by
`~/.aider.conf.yml` or `~/.aider`. Both run on your own provider API keys,
so there's no single usage-limit window to show.

## Design

- `src/adapters/mod.rs` defines the `Adapter` trait (`id`, `name`,
  `installed`, `poll`) and the registry.
- Every adapter receives a `Ctx { home, now, live }`. Tests use temporary
  home directories with synthetic fixtures and `live = false`, so they never
  touch the Keychain or the network.
- Dependencies: `serde`, `serde_json`, `chrono`, `dirs`, `ureq` (rustls).
  Dev-only: `tempfile`.
