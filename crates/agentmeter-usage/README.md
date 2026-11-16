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
pub fn mock_all(t: f64) -> Vec<AgentUsage>;    // demo data (percentages deterministic in t)
pub fn mock_at(t: f64, now: DateTime<Utc>) -> Vec<AgentUsage>; // fully deterministic demo data
pub fn poll_all_at(home: &Path) -> Vec<AgentUsage>; // file sources only, custom $HOME (no Keychain/network)
```

`poll_all` always returns one row per known agent, in a fixed order, whether or
not it is installed. Adapter errors become a `note` with `percent_left: None`.
Panics inside an adapter are caught.

`poll_all` blocks the calling thread. Most calls finish in well under 100 ms.
The exception is a Claude Code refresh (at most once every 5 minutes, and
never during backoff). That runs a Keychain lookup (3 s deadline) and an HTTP
call (5 s timeout), and may also scan logs. From Tauri, call it from a
background thread or `spawn_blocking`, not from the main thread.

`poll_all` persists a small Claude state file at
`dirs::cache_dir()/agentmeter/claude-usage.json` (on macOS,
`~/Library/Caches/agentmeter/`). It holds the last good reading, the backoff
state, and the calibrated token budget. It never contains credentials.

`mock_all(t)`: Claude Code drains from 100% to 10% over 90 s and then resets to
100%. Percentages depend only on `t`. `resetsAt` is relative to the current
wall clock, so countdown labels look realistic. Claude's demo cycle maps onto
a 5-hour window. `mock_at(t, now)` is the fully deterministic variant. Codex oscillates around 60%. Gemini stays low (6–13%). Cursor is
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

The adapter takes the 20 most recently modified session files as candidates.
From those, it picks the reading with the **latest event timestamp**. File
mtime alone isn't trusted, because an old session touched by a non-quota event
would otherwise win with stale numbers.

- `percent_left = 100 - primary.used_percent`, and `exact = true`.
- If `resets_at` is already in the past, the window has reset since the last
  reading. It then reports 100% with **`exact = false`**, because usage since
  the reset hasn't been observed, and the note says so.
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
     shows the last good reading (`exact = false`) if one is available, or
     else the log estimate. The note asks you to open Claude Code.
   - The Keychain lookup runs as a subprocess with a 3 s deadline. If it hangs
     (for example, waiting for access approval), it is killed, so other
     agents are still polled. The row shows the last good reading
     (`exact = false`) if one is available. Otherwise it is unknown, with an
     error note.
   - The HTTP call has a 5 s timeout.
   - **Gentle on the endpoint.** Claude Code itself also calls this endpoint,
     and it rate-limits.
     - A good reading is reused for **5 min**. Within 10 minutes of a window
       reset, that drops to 60 s.
     - Any failure starts a backoff of **at least 60 s**. The adapter honours
       `Retry-After` (seconds or HTTP-date) but floors it at 60 s, so
       `Retry-After: 0` doesn't mean "retry next poll", and caps it at 1 h. Without it, it waits
       60 s → 2 m → 4 m → 8 m → 15 m (cap), with up to +20% jitter.
     - **The API is never called during backoff.** Only one fetch runs at a
       time, and no lock is held during the Keychain or HTTP call.
   - **Serving during errors.** While the API is unavailable, the last good
     exact reading is still shown with the same percentage, `exact = false`,
     and a note like
     `Last exact reading 6m ago; usage API rate-limited (HTTP 429), retrying in 2m`.
   - **Reset since the last reading.** If the last reading's `resets_at` has
     passed, the API is refetched promptly (unless in backoff).
     - Meanwhile, the log estimate is used if it has evidence for the new
       window.
     - Otherwise the row shows 100% with `exact = false`.
     - A last good reading without a reset time is dropped after 5 h.
   - **Persistence.** The last good reading, failure count, backoff deadline
     and calibrated budget are saved in the cache-dir JSON above. An app
     restart therefore neither re-hits the API early nor loses the reading.
     A corrupt file is ignored.
     - Wall-clock values are bounded on every load and poll. A backoff more
       than 1 h ahead is clamped to 1 h, and a reading dated more than 60 s
       in the future counts as stale. A bad file or a clock jump can delay
       fetching by at most 1 h.
     - Saves write a temp file unique to the process and the write, fsync it,
       then rename it into place. Two app instances never interleave writes.
   - After a caller takes fetch ownership, it re-reads the state and re-checks
     the backoff and TTL. A backoff started by a concurrent caller is never
     bypassed.
2. **Estimate fallback.** This runs when the API is unavailable (not logged in
   with a subscription, offline, HTTP error) and there is no usable last good
   reading. It is also used in `poll_all_at`, and is cached in memory for 60 s.
   - It rebuilds 5-hour session blocks from assistant `message.usage` records
     in `~/.claude/projects/**/*.jsonl` from the last 8 days, deduplicated by
     message id and request id.
   - A block starts at the minute of its first message. Tokens counted are
     input + output + cache-creation; cache reads are excluded.
   - The limit comes from one of two places:
     - **Calibrated**: a token budget derived from the last successful exact
       reading (tokens used ÷ fraction used), persisted with the state.
     - Otherwise, **peak proxy**: the busiest completed block in the history.
   - If the current block is busier than the peak, it returns unknown rather
     than a made-up 0%.
   - If no usable records exist (logs missing, unreadable, or malformed), it
     returns unknown rather than a full tube.
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
