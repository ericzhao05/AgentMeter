//! Claude Code: 5-hour session limit.
//!
//! 1. **Exact** (live mode only): `GET https://api.anthropic.com/api/oauth/usage`
//!    with header `anthropic-beta: oauth-2025-04-20`, the same endpoint Claude
//!    Code's `/usage` screen uses. Authenticated with the OAuth access token Claude
//!    Code stores in the macOS Keychain (generic password, service
//!    `Claude Code-credentials`), or `~/.claude/.credentials.json` on Linux.
//!    Strictly read-only: the token is never refreshed, rewritten, logged or sent
//!    anywhere except api.anthropic.com.
//! 2. **Estimate** fallback: rebuild 5-hour session blocks from
//!    `~/.claude/projects/**/*.jsonl` assistant `usage` records and compare the
//!    current block's tokens with the busiest completed block seen in the last
//!    few days (a data-derived proxy for the limit; Anthropic does not publish
//!    a token figure).
//!
//! Live access is rate-limit aware and persisted; see the adapter section below.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use chrono::{DateTime, Duration, DurationRound, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Adapter;
use crate::util::{clamp_pct, is_jsonl, is_recent, rfc3339, round1, run_with_deadline, walk_files};
use crate::{AgentUsage, Ctx};

pub struct ClaudeCode;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(5);
/// Upper bound for the Keychain lookup (it can block waiting for user approval).
const KEYCHAIN_TIMEOUT: StdDuration = StdDuration::from_secs(3);
const BLOCK_HOURS: i64 = 5;
/// History scanned for the estimate (files older than this are skipped).
const HISTORY_DAYS: i64 = 8;

/// Normalised reading independent of source.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub percent_left: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub exact: bool,
    pub note: String,
}

// ---------------------------------------------------------------- live source

/// Access token wrapper whose Debug never reveals the secret.
struct Token(String);
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Extract the access token from Claude Code's credential JSON.
/// Returns Err (without the secret) if missing or expired.
fn token_from_credentials(json: &str, now: DateTime<Utc>) -> Result<Token, String> {
    let v: Value = serde_json::from_str(json.trim()).map_err(|_| "credentials are not valid JSON")?;
    let oauth = v.get("claudeAiOauth").ok_or("no claudeAiOauth entry in credentials")?;
    let tok = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("no OAuth access token in credentials")?;
    if let Some(exp_ms) = oauth.get("expiresAt").and_then(Value::as_i64) {
        if exp_ms <= now.timestamp_millis() {
            return Err("stored OAuth token has expired (open Claude Code to refresh it)".into());
        }
    }
    Ok(Token(tok.to_string()))
}

/// Why the live source failed. `hard` errors are reported as-is (unknown)
/// instead of falling back to the estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveError {
    pub msg: String,
    pub hard: bool,
    /// HTTP status, if the API answered.
    pub status: Option<u16>,
    /// Raw `Retry-After` header value, if any.
    pub retry_after: Option<String>,
}

impl LiveError {
    fn soft(msg: impl Into<String>) -> Self {
        LiveError { msg: msg.into(), hard: false, status: None, retry_after: None }
    }
}

impl From<String> for LiveError {
    fn from(msg: String) -> Self {
        LiveError::soft(msg)
    }
}

impl From<&str> for LiveError {
    fn from(msg: &str) -> Self {
        LiveError::soft(msg)
    }
}

fn read_credentials_json(home: &Path) -> Result<String, LiveError> {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("/usr/bin/security");
        cmd.args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"]);
        match run_with_deadline(&mut cmd, KEYCHAIN_TIMEOUT) {
            Ok(out) => {
                if let Ok(s) = String::from_utf8(out) {
                    if !s.trim().is_empty() {
                        return Ok(s);
                    }
                }
            }
            Err(e) if e.contains("timed out") => {
                return Err(LiveError {
                    msg: format!(
                        "Keychain lookup did not finish within {}s (waiting for access approval?)",
                        KEYCHAIN_TIMEOUT.as_secs()
                    ),
                    hard: true,
                    status: None,
                    retry_after: None,
                });
            }
            Err(_) => {} // item missing / access denied: try the file below
        }
    }
    fs::read_to_string(home.join(".claude/.credentials.json")).map_err(|_| {
        LiveError::soft("no Claude Code OAuth credentials found (not logged in with a Claude subscription?)")
    })
}

/// Parse the `/api/oauth/usage` response body.
/// Shape: `{"five_hour":{"utilization":23.0,"resets_at":"…"},"seven_day":{…},…}`
/// where `utilization` is percent used (0..=100).
pub fn parse_oauth_usage(body: &str) -> Result<Reading, String> {
    let v: Value = serde_json::from_str(body).map_err(|_| "usage response is not JSON")?;
    let fh = v
        .get("five_hour")
        .filter(|x| !x.is_null())
        .ok_or("usage response has no five_hour window")?;
    let used = fh
        .get("utilization")
        .and_then(Value::as_f64)
        .ok_or("five_hour.utilization missing")?;
    let resets_at = fh
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let mut note = "Live from Anthropic usage API (same as /usage)".to_string();
    if let Some(sd) = v.get("seven_day").and_then(|x| x.get("utilization")).and_then(Value::as_f64) {
        note.push_str(&format!("; 7d window: {}% left", round1(clamp_pct(100.0 - sd))));
    }
    Ok(Reading {
        percent_left: Some(round1(clamp_pct(100.0 - used))),
        resets_at,
        exact: true,
        note,
    })
}

fn fetch_live(home: &Path, now: DateTime<Utc>) -> Result<Reading, LiveError> {
    let creds = read_credentials_json(home)?;
    let token = token_from_credentials(&creds, now)?;
    drop(creds);
    let agent = ureq::AgentBuilder::new().timeout(HTTP_TIMEOUT).build();
    let resp = agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {}", token.0))
        .set("anthropic-beta", OAUTH_BETA)
        .set("Accept", "application/json")
        .set("User-Agent", concat!("agentmeter-usage/", env!("CARGO_PKG_VERSION")))
        .call();
    drop(token);
    match resp {
        Ok(r) => {
            let body = r.into_string().map_err(|_| "could not read usage response")?;
            Ok(parse_oauth_usage(&body)?)
        }
        // Only the status code is reported: never echo headers/body.
        Err(ureq::Error::Status(code, resp)) => {
            let msg = if code == 429 {
                "usage API rate-limited (HTTP 429)".to_string()
            } else {
                format!("usage API returned HTTP {code}")
            };
            Err(LiveError {
                msg,
                hard: false,
                status: Some(code),
                retry_after: resp.header("retry-after").map(str::to_string),
            })
        }
        Err(ureq::Error::Transport(t)) => Err(format!("usage API unreachable ({})", t.kind()).into()),
    }
}

// ----------------------------------------------------------- estimate source

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    pub at: DateTime<Utc>,
    pub tokens: u64,
}

/// Parse one projects JSONL line into a usage entry. Also returns a dedupe key
/// (streamed responses repeat the same message id / request id).
pub fn parse_entry(line: &str) -> Option<(Entry, Option<String>)> {
    if !line.contains("\"usage\"") {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let at = DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?).ok()?.with_timezone(&Utc);
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;
    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    // Cache reads are excluded: they are cheap and would swamp everything else.
    let tokens = n("input_tokens") + n("output_tokens") + n("cache_creation_input_tokens");
    let key = msg
        .get("id")
        .and_then(Value::as_str)
        .map(|id| format!("{id}:{}", v.get("requestId").and_then(Value::as_str).unwrap_or("")));
    Some((Entry { at, tokens }, key))
}

pub fn collect_entries(projects: &Path, now: DateTime<Utc>) -> Vec<Entry> {
    let cutoff: SystemTime = (now - Duration::days(HISTORY_DAYS)).into();
    let files = walk_files(projects, 5, &is_jsonl);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (p, m) in files {
        if m < cutoff {
            continue;
        }
        let Ok(c) = fs::read_to_string(&p) else { continue };
        for line in c.lines() {
            if let Some((e, key)) = parse_entry(line) {
                if key.map_or(true, |k| seen.insert(k)) {
                    out.push(e);
                }
            }
        }
    }
    out.sort_by_key(|e| e.at);
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Block {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub last: DateTime<Utc>,
    pub tokens: u64,
}

/// Group sorted entries into 5-hour session blocks: a block starts at the minute
/// of its first message and covers the following 5 hours (this matches the reset
/// times the usage API reports).
pub fn blocks(entries: &[Entry]) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    for e in entries {
        match out.last_mut() {
            Some(b) if e.at < b.end => {
                b.tokens += e.tokens;
                b.last = e.at;
            }
            _ => {
                let start = e.at.duration_trunc(Duration::minutes(1)).unwrap_or(e.at);
                out.push(Block {
                    start,
                    end: start + Duration::hours(BLOCK_HOURS),
                    last: e.at,
                    tokens: e.tokens,
                });
            }
        }
    }
    out
}

/// The block containing `now`, if any.
pub fn current_block(bl: &[Block], now: DateTime<Utc>) -> Option<Block> {
    bl.iter().copied().find(|b| b.start <= now && now < b.end)
}

/// Estimate the 5h window from local logs.
///
/// `calibrated_limit` is a token budget derived from an earlier exact reading
/// (see [`calibrate`]); without it, the busiest completed block in the scanned
/// history is used as a proxy for the limit.
pub fn estimate(entries: &[Entry], now: DateTime<Utc>, calibrated_limit: Option<u64>) -> Reading {
    let base = "Estimate from local Claude Code logs (exact usage API unavailable)";
    if entries.is_empty() {
        return Reading {
            percent_left: None,
            resets_at: None,
            exact: false,
            note: format!("{base}: no usable usage records in ~/.claude/projects, so remaining usage is unknown"),
        };
    }
    let bl = blocks(entries);
    let Some(cur) = current_block(&bl, now) else {
        return Reading {
            percent_left: Some(100.0),
            resets_at: None,
            exact: false,
            note: format!("{base}: no usage in the current 5h window"),
        };
    };
    let used = cur.tokens;
    let resets_at = Some(cur.end);
    let unknown = |why: String| Reading { percent_left: None, resets_at, exact: false, note: format!("{base}: {why}") };

    if let Some(limit) = calibrated_limit.filter(|l| *l > 0) {
        let pct = round1(clamp_pct(100.0 - used as f64 / limit as f64 * 100.0));
        return Reading {
            percent_left: Some(pct),
            resets_at,
            exact: false,
            note: format!("{base}: {used} tokens this 5h window vs. ~{limit} limit calibrated from an earlier exact reading"),
        };
    }
    let peak = bl.iter().filter(|b| b.end <= now).map(|b| b.tokens).max().unwrap_or(0);
    if peak == 0 {
        return unknown(format!("{used} tokens this window, but no past windows to compare against"));
    }
    if used > peak {
        return unknown(format!(
            "{used} tokens this window exceeds your busiest recent window ({peak}); the limit cannot be inferred"
        ));
    }
    let pct = round1(clamp_pct(100.0 - used as f64 / peak as f64 * 100.0));
    Reading {
        percent_left: Some(pct),
        resets_at,
        exact: false,
        note: format!("{base}: {used} tokens this 5h window vs. {peak} in your busiest recent window (rough proxy for the limit)"),
    }
}

/// Derive a token budget from an exact reading: if `used_tokens` corresponds to
/// `used_percent` of the window, the full window is `used_tokens / used_percent`.
/// Returns None when the reading is too small to be meaningful.
pub fn calibrate(used_tokens: u64, used_percent: f64) -> Option<u64> {
    if used_percent < 5.0 || used_tokens == 0 {
        return None;
    }
    Some((used_tokens as f64 / (used_percent / 100.0)).round() as u64)
}

// ------------------------------------------------------------------- adapter
//
// The live endpoint is shared with Claude Code itself and rate-limits
// aggressively, so access goes through a small persisted state machine:
// * a successful reading is reused for 5 min (60 s shortly after a window reset);
// * 429/5xx (and any other failure) back off, honouring `Retry-After`, else
//   exponential 60 s -> 2 m -> 4 m ... capped at 15 min, with jitter;
// * the API is never called during backoff, and only one fetch runs at a time;
// * the last good reading is served (exact=false, with its age) while the API
//   is unavailable;
// * state is persisted to `<cache_dir>/claude-usage.json` (never the token) so
//   app restarts neither re-hit the API nor lose the reading.

const SUCCESS_TTL_SECS: i64 = 300;
const POST_RESET_TTL_SECS: i64 = 60;
/// How long after a window reset the short TTL applies.
const POST_RESET_PERIOD_SECS: i64 = 600;
const BACKOFF_BASE_SECS: i64 = 60;
const BACKOFF_CAP_SECS: i64 = 900;
const RETRY_AFTER_MAX_SECS: i64 = 3600;
/// A last-good reading without a reset time is ignored after one full window.
const LAST_GOOD_MAX_AGE_SECS: i64 = BLOCK_HOURS * 3600;
const ESTIMATE_TTL: StdDuration = StdDuration::from_secs(60);
pub const STATE_FILE: &str = "claude-usage.json";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GoodReading {
    pub percent_left: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub fetched_at: DateTime<Utc>,
    pub note: String,
}

/// Persisted live-source state. Contains no credentials.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct LiveState {
    pub last_good: Option<GoodReading>,
    pub backoff_until: Option<DateTime<Utc>>,
    pub failures: u32,
    pub last_error: Option<String>,
    pub last_error_hard: bool,
    pub reset_seen_at: Option<DateTime<Utc>>,
    pub calibrated_limit: Option<u64>,
}

/// Parse a `Retry-After` value: delta-seconds or an HTTP-date.
pub fn retry_after_secs(v: &str, now: DateTime<Utc>) -> Option<i64> {
    let v = v.trim();
    if let Ok(n) = v.parse::<i64>() {
        return Some(n.max(0));
    }
    DateTime::parse_from_rfc2822(v).ok().map(|d| (d.with_timezone(&Utc) - now).num_seconds().max(0))
}

/// Exponential backoff for the n-th consecutive failure (n >= 1), with up to
/// +20% jitter (`jitter01` in 0..=1), capped at 15 min.
pub fn backoff_secs(failures: u32, jitter01: f64) -> i64 {
    let exp = BACKOFF_BASE_SECS.saturating_mul(1i64 << failures.saturating_sub(1).min(16));
    let jitter = (exp as f64 * 0.2 * jitter01.clamp(0.0, 1.0)) as i64;
    exp.saturating_add(jitter).min(BACKOFF_CAP_SECS)
}

/// Allowed clock skew before a `fetched_at` in the future is distrusted.
const FUTURE_SKEW_SECS: i64 = 60;

/// Bound wall-clock values from disk or from before a clock change: no backoff
/// can legitimately end more than `RETRY_AFTER_MAX_SECS` from now. Applied on
/// every load and every poll, so a corrupt/forged file or a backwards clock
/// jump can delay fetching by at most that cap.
fn sanitize(st: &mut LiveState, now: DateTime<Utc>) {
    let max = now + Duration::seconds(RETRY_AFTER_MAX_SECS);
    if st.backoff_until.is_some_and(|b| b > max) {
        st.backoff_until = Some(max);
    }
}

fn note_reset(st: &mut LiveState, now: DateTime<Utc>) {
    if let Some(r) = st.last_good.as_ref().and_then(|g| g.resets_at).filter(|r| *r <= now) {
        if st.reset_seen_at.map_or(true, |seen| seen < r) {
            st.reset_seen_at = Some(r);
        }
    }
}

/// Whether the live API may be called now.
pub fn should_fetch(st: &LiveState, now: DateTime<Utc>) -> bool {
    if st.backoff_until.is_some_and(|b| now < b) {
        return false;
    }
    let Some(g) = &st.last_good else { return true };
    if (g.fetched_at - now).num_seconds() > FUTURE_SKEW_SECS {
        return true; // fetched "in the future": clock changed or bad file; don't trust its freshness
    }
    if g.resets_at.is_some_and(|r| r <= now) {
        return true; // window reset since the reading: refresh promptly
    }
    let just_reset = st.reset_seen_at.is_some_and(|r| (now - r).num_seconds() < POST_RESET_PERIOD_SECS);
    let ttl = if just_reset { POST_RESET_TTL_SECS } else { SUCCESS_TTL_SECS };
    (now - g.fetched_at).num_seconds() >= ttl || st.last_error.is_some()
}

fn record_success(st: &mut LiveState, r: &Reading, now: DateTime<Utc>) {
    if let Some(p) = r.percent_left {
        st.last_good = Some(GoodReading { percent_left: p, resets_at: r.resets_at, fetched_at: now, note: r.note.clone() });
    }
    st.failures = 0;
    st.backoff_until = None;
    st.last_error = None;
    st.last_error_hard = false;
}

fn record_failure(st: &mut LiveState, e: &LiveError, now: DateTime<Utc>, jitter01: f64) {
    st.failures = st.failures.saturating_add(1);
    let wait = e
        .retry_after
        .as_deref()
        .and_then(|v| retry_after_secs(v, now))
        .map(|s| s.clamp(1, RETRY_AFTER_MAX_SECS))
        .unwrap_or_else(|| backoff_secs(st.failures, jitter01));
    st.backoff_until = Some(now + Duration::seconds(wait));
    st.last_error = Some(e.msg.clone());
    st.last_error_hard = e.hard;
}

pub fn human_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", (s + 30) / 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

fn retry_hint(st: &LiveState, now: DateTime<Utc>) -> String {
    match st.backoff_until {
        Some(b) if b > now => format!(", retrying in {}", human_duration((b - now).num_seconds())),
        _ => String::new(),
    }
}

/// Turn state into a reading. `est` lazily produces the log-based estimate.
fn render(st: &LiveState, now: DateTime<Utc>, est: &mut dyn FnMut(Option<u64>) -> Reading) -> Reading {
    let good = st
        .last_good
        .as_ref()
        .filter(|g| g.resets_at.is_some() || (now - g.fetched_at).num_seconds() < LAST_GOOD_MAX_AGE_SECS);
    if let Some(g) = good {
        let reset_passed = g.resets_at.is_some_and(|r| r <= now);
        if reset_passed {
            // Window reset since the last exact reading: prefer local evidence,
            // else show full (apply() marks it exact=false).
            let r = est(st.calibrated_limit);
            if r.percent_left.is_some() && r.resets_at.is_some() {
                return r;
            }
        }
        let age = human_duration((now - g.fetched_at).num_seconds());
        return match &st.last_error {
            None => Reading { percent_left: Some(g.percent_left), resets_at: g.resets_at, exact: true, note: g.note.clone() },
            Some(err) => Reading {
                percent_left: Some(g.percent_left),
                resets_at: g.resets_at,
                exact: false,
                note: format!("Last exact reading {age} ago; {err}{}", retry_hint(st, now)),
            },
        };
    }
    if let (true, Some(err)) = (st.last_error_hard, &st.last_error) {
        return Reading { percent_left: None, resets_at: None, exact: false, note: format!("error: {err}{}", retry_hint(st, now)) };
    }
    let mut r = est(st.calibrated_limit);
    if let Some(err) = &st.last_error {
        r.note = format!("{} [live: {err}{}]", r.note, retry_hint(st, now));
    }
    r
}

fn load_state(path: Option<&Path>) -> LiveState {
    let mut st: LiveState = path
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    sanitize(&mut st, Utc::now());
    st
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically replace the state file: write a temp file unique to this
/// process and write (pid + counter, `create_new`) in the same directory,
/// fsync it, then rename over the target. Concurrent writers (threads or app
/// instances) never share a temp file, so the published file is always one
/// complete write.
fn save_state(path: Option<&Path>, st: &LiveState) {
    use std::io::Write;
    let Some(p) = path else { return };
    let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else { return };
    let _ = fs::create_dir_all(dir);
    let Ok(body) = serde_json::to_string_pretty(st) else { return };
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.{}.{}.tmp", name.to_string_lossy(), std::process::id(), n));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(body.as_bytes())?;
            f.sync_all()
        });
    if written.is_err() || fs::rename(&tmp, p).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// Process-wide live-source state (+ single-flight flag + estimate cache).
pub(crate) struct Store {
    state: Mutex<Option<(Option<PathBuf>, LiveState)>>,
    in_flight: AtomicBool,
    est: Mutex<Option<(Instant, PathBuf, Option<u64>, Reading)>>,
    /// Test hook run between the first eligibility check and taking flight ownership.
    #[cfg(test)]
    before_acquire: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Store {
    pub const fn new() -> Self {
        Store {
            state: Mutex::new(None),
            in_flight: AtomicBool::new(false),
            est: Mutex::new(None),
            #[cfg(test)]
            before_acquire: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn new_with(st: LiveState) -> Self {
        let s = Store::new();
        *s.state.lock().unwrap() = Some((None, st));
        s
    }

    /// Current state, loading it from disk on first use (or when the path changes).
    /// Test-only: production reads go through `current`, which also clamps.
    #[cfg(test)]
    fn snapshot(&self, path: Option<&Path>) -> LiveState {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if g.as_ref().map_or(true, |(p, _)| p.as_deref() != path) {
            *g = Some((path.map(Path::to_path_buf), load_state(path)));
        }
        g.as_ref().map(|(_, s)| s.clone()).unwrap_or_default()
    }

    /// Current state with wall-clock bounds applied to the *authoritative*
    /// copy (under the mutex), persisting the correction if anything changed.
    fn current(&self, path: Option<&Path>, now: DateTime<Utc>) -> LiveState {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if g.as_ref().map_or(true, |(p, _)| p.as_deref() != path) {
            *g = Some((path.map(Path::to_path_buf), load_state(path)));
        }
        let st = &mut g.as_mut().expect("initialised above").1;
        let before = st.backoff_until;
        sanitize(st, now);
        if st.backoff_until != before {
            save_state(path, st);
        }
        st.clone()
    }

    fn update(&self, path: Option<&Path>, f: impl FnOnce(&mut LiveState)) -> LiveState {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if g.as_ref().map_or(true, |(p, _)| p.as_deref() != path) {
            *g = Some((path.map(Path::to_path_buf), load_state(path)));
        }
        let st = &mut g.as_mut().expect("initialised above").1;
        f(st);
        save_state(path, st);
        st.clone()
    }

    fn estimate(&self, ctx: &Ctx, limit: Option<u64>) -> Reading {
        let mut g = self.est.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, home, l, r)) = g.as_ref() {
            if at.elapsed() < ESTIMATE_TTL && *home == ctx.home && *l == limit {
                return r.clone();
            }
        }
        let r = estimate(&collect_entries(&ctx.path(".claude/projects"), ctx.now), ctx.now, limit);
        *g = Some((Instant::now(), ctx.home.clone(), limit, r.clone()));
        r
    }
}

static STORE: Store = Store::new();

type LiveFetch<'a> = &'a dyn Fn(&Ctx) -> Result<Reading, LiveError>;

fn real_fetch(ctx: &Ctx) -> Result<Reading, LiveError> {
    fetch_live(&ctx.home, ctx.now)
}

fn real_jitter() -> f64 {
    let n = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    (n % 1000) as f64 / 1000.0
}

/// Clears the single-flight flag even if the fetch panics.
struct InFlight<'a>(&'a AtomicBool);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn cached_reading(ctx: &Ctx) -> Reading {
    if !ctx.live {
        return estimate(&collect_entries(&ctx.path(".claude/projects"), ctx.now), ctx.now, None);
    }
    cached_reading_with(&STORE, ctx, &real_fetch, real_jitter())
}

/// One poll: maybe fetch (outside backoff, single-flight, no lock held during
/// the call), update + persist state, then render.
fn cached_reading_with(store: &Store, ctx: &Ctx, fetch: LiveFetch, jitter01: f64) -> Reading {
    let now = ctx.now;
    let path = ctx.cache_dir.as_ref().map(|d| d.join(STATE_FILE));
    let path = path.as_deref();
    let current = |store: &Store| {
        let mut st = store.current(path, now);
        note_reset(&mut st, now);
        st
    };
    let mut st = current(store);
    #[cfg(test)]
    if should_fetch(&st, now) {
        let hook = store.before_acquire.lock().unwrap().take();
        if let Some(h) = hook {
            h();
        }
    }
    if should_fetch(&st, now)
        && store.in_flight.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    {
        let _flight = InFlight(&store.in_flight);
        // Another caller may have fetched (and e.g. started a backoff) between
        // our first check and taking ownership: re-read and re-check.
        st = current(store);
        if !should_fetch(&st, now) {
            return render(&st, now, &mut |limit| store.estimate(ctx, limit));
        }
        let res = fetch(ctx);
        // Calibrate the estimate's token budget from the exact reading.
        let limit = match &res {
            Ok(r) => r.percent_left.and_then(|left| {
                let entries = collect_entries(&ctx.path(".claude/projects"), now);
                let used = current_block(&blocks(&entries), now).map_or(0, |b| b.tokens);
                calibrate(used, 100.0 - left)
            }),
            Err(_) => None,
        };
        st = store.update(path, |s| {
            note_reset(s, now);
            match &res {
                Ok(r) => {
                    record_success(s, r, now);
                    if limit.is_some() {
                        s.calibrated_limit = limit;
                    }
                }
                Err(e) => record_failure(s, e, now, jitter01),
            }
        });
    }
    render(&st, now, &mut |limit| store.estimate(ctx, limit))
}

/// Render a reading into the row, handling a window that has reset since the reading.
pub fn apply(u: &mut AgentUsage, r: &Reading, now: DateTime<Utc>) {
    u.window = Some("5h".into());
    u.exact = r.exact;
    match r.resets_at {
        Some(reset) if reset <= now => {
            // Usage since the reset has not been observed: assume full, flag as estimate.
            u.percent_left = Some(100.0);
            u.resets_at = None;
            u.exact = false;
            u.note = Some(format!(
                "Window reset at {}; no newer reading, assuming full. Previous: {}",
                rfc3339(reset),
                r.note
            ));
        }
        reset => {
            u.percent_left = r.percent_left;
            u.resets_at = reset.map(rfc3339);
            u.note = Some(r.note.clone());
        }
    }
}

impl Adapter for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn name(&self) -> &'static str {
        "Claude Code"
    }
    fn installed(&self, ctx: &Ctx) -> bool {
        ctx.path(".claude").is_dir()
    }
    fn poll(&self, ctx: &Ctx) -> Result<AgentUsage, String> {
        let mut u = AgentUsage::new(self.id(), self.name());
        u.installed = self.installed(ctx);
        if !u.installed {
            return Ok(u);
        }
        let newest = walk_files(&ctx.path(".claude/projects"), 5, &is_jsonl)
            .into_iter()
            .map(|(_, m)| m)
            .max();
        u.active = is_recent(newest, ctx.now);
        let r = cached_reading(ctx);
        apply(&mut u, &r, ctx.now);
        Ok(u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn assistant(ts: &str, id: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","requestId":"req_{id}","message":{{"id":"msg_{id}","role":"assistant","content":[],"usage":{{"input_tokens":{input},"cache_creation_input_tokens":0,"cache_read_input_tokens":99999,"output_tokens":{output}}}}}}}"#
        )
    }

    #[test]
    fn parses_oauth_usage() {
        let body = r#"{"five_hour":{"utilization":23.0,"resets_at":"2026-11-15T20:00:00+00:00"},"seven_day":{"utilization":40,"resets_at":null},"seven_day_opus":null}"#;
        let r = parse_oauth_usage(body).unwrap();
        assert_eq!(r.percent_left, Some(77.0));
        assert_eq!(rfc3339(r.resets_at.unwrap()), "2026-11-15T20:00:00Z");
        assert!(r.exact);
        assert!(r.note.contains("7d window: 60% left"));
    }

    #[test]
    fn oauth_usage_malformed() {
        assert!(parse_oauth_usage("<html>").is_err());
        assert!(parse_oauth_usage(r#"{"five_hour":null}"#).is_err());
        assert!(parse_oauth_usage(r#"{"five_hour":{"resets_at":null}}"#).is_err());
        let r = parse_oauth_usage(r#"{"five_hour":{"utilization":120,"resets_at":null}}"#).unwrap();
        assert_eq!(r.percent_left, Some(0.0));
        assert!(r.resets_at.is_none());
    }

    #[test]
    fn credentials_parsing_never_leaks_token() {
        let now = t("2026-11-15T12:00:00Z");
        let future = now.timestamp_millis() + 3_600_000;
        let ok = format!(r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat-SECRET","expiresAt":{future}}}}}"#);
        let tok = token_from_credentials(&ok, now).unwrap();
        assert!(!format!("{tok:?}").contains("SECRET"));

        let expired = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-SECRET","expiresAt":1}}"#;
        let e = token_from_credentials(expired, now).unwrap_err();
        assert!(e.contains("expired") && !e.contains("SECRET"));
        assert!(token_from_credentials("garbage", now).is_err());
        assert!(token_from_credentials(r#"{"claudeAiOauth":{}}"#, now).is_err());
    }

    #[test]
    fn parse_entry_filters_and_sums() {
        let (e, k) = parse_entry(&assistant("2026-11-15T10:15:00Z", "a", 100, 50)).unwrap();
        assert_eq!(e.tokens, 150); // cache reads excluded
        assert_eq!(k.as_deref(), Some("msg_a:req_a"));
        assert!(parse_entry(r#"{"type":"user","timestamp":"2026-11-15T10:00:00Z","usage":{}}"#).is_none());
        assert!(parse_entry("{\"usage\": broken").is_none());
        assert!(parse_entry("").is_none());
    }

    #[test]
    fn blocks_and_estimate() {
        let es = vec![
            // past block 1: 1000 tokens
            Entry { at: t("2026-11-14T08:10:00Z"), tokens: 600 },
            Entry { at: t("2026-11-14T11:00:00Z"), tokens: 400 },
            // past block 2: 200
            Entry { at: t("2026-11-14T20:00:00Z"), tokens: 200 },
            // current block starting 10:00
            Entry { at: t("2026-11-15T10:30:00Z"), tokens: 250 },
        ];
        let b = blocks(&es);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0].start, t("2026-11-14T08:10:00Z"));
        assert_eq!(b[0].tokens, 1000);
        let now = t("2026-11-15T11:00:00Z");
        let r = estimate(&es, now, None);
        assert_eq!(r.percent_left, Some(75.0));
        assert_eq!(r.resets_at, Some(t("2026-11-15T15:30:00Z")));
        assert!(!r.exact);

        // calibrated limit wins over the peak proxy
        let r = estimate(&es, now, Some(500));
        assert_eq!(r.percent_left, Some(50.0));
        assert!(r.note.contains("calibrated"));

        // current window busier than any past one -> unknown, not 0
        let mut busy = es.clone();
        busy.push(Entry { at: t("2026-11-15T10:45:00Z"), tokens: 5000 });
        assert_eq!(estimate(&busy, now, None).percent_left, None);

        // no current block
        assert_eq!(estimate(&es, t("2026-11-15T16:00:00Z"), None).percent_left, Some(100.0));
        // only a current block, no history -> unknown
        assert_eq!(estimate(&es[3..], now, None).percent_left, None);
        // nothing at all -> no evidence -> unknown
        assert_eq!(estimate(&[], now, None).percent_left, None);
    }

    #[test]
    fn calibration() {
        assert_eq!(calibrate(400, 40.0), Some(1000));
        assert_eq!(calibrate(400, 2.0), None);
        assert_eq!(calibrate(0, 50.0), None);
    }

    #[test]
    fn adapter_estimates_from_fixture_tree_with_dedupe() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join(".claude/projects/-tmp-proj");
        fs::create_dir_all(proj.join("sess/subagents")).unwrap();
        let now = Utc::now();
        let fmt = |dt: DateTime<Utc>| rfc3339(dt);
        let cur = now - Duration::minutes(1);
        let past = now - Duration::hours(30);
        let content = [
            assistant(&fmt(past), "p1", 1000, 0),
            assistant(&fmt(past), "p1", 1000, 0), // duplicate (streamed) -> ignored
            assistant(&fmt(cur), "c1", 300, 0),
            "not json at all".to_string(),
        ]
        .join("\n");
        fs::write(proj.join("sess.jsonl"), content).unwrap();
        fs::write(proj.join("sess/subagents/agent-1.jsonl"), assistant(&fmt(cur), "c2", 200, 0)).unwrap();

        let ctx = Ctx { home: d.path().to_path_buf(), now, live: false, cache_dir: None };
        let u = ClaudeCode.poll(&ctx).unwrap();
        assert!(u.installed && u.active && !u.exact);
        assert_eq!(u.percent_left, Some(50.0));
        assert_eq!(u.window.as_deref(), Some("5h"));
        assert!(u.note.unwrap().contains("Estimate"));
    }

    #[test]
    fn adapter_installed_without_projects() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        let ctx = Ctx { home: d.path().to_path_buf(), now: Utc::now(), live: false, cache_dir: None };
        let u = ClaudeCode.poll(&ctx).unwrap();
        assert!(u.installed && !u.active);
        assert_eq!(u.percent_left, None);
    }

    fn failing(_: &Ctx) -> Result<Reading, LiveError> {
        Err(LiveError::soft("usage API returned HTTP 500"))
    }

    fn never(_: &Ctx) -> Result<Reading, LiveError> {
        panic!("live API must not be called here")
    }

    fn rate_limited(retry_after: Option<&str>) -> impl Fn(&Ctx) -> Result<Reading, LiveError> + '_ {
        move |_| {
            Err(LiveError {
                msg: "usage API rate-limited (HTTP 429)".into(),
                hard: false,
                status: Some(429),
                retry_after: retry_after.map(str::to_string),
            })
        }
    }

    fn ok_reading(left: f64, resets_at: Option<DateTime<Utc>>) -> impl Fn(&Ctx) -> Result<Reading, LiveError> {
        move |_| Ok(Reading { percent_left: Some(left), resets_at, exact: true, note: "Live".into() })
    }

    fn live_ctx(home: &Path, now: DateTime<Utc>, cache: Option<&Path>) -> Ctx {
        Ctx { home: home.to_path_buf(), now, live: true, cache_dir: cache.map(Path::to_path_buf) }
    }

    fn home_with_claude() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        d
    }

    #[test]
    fn api_failure_and_missing_logs_is_unknown() {
        let d = home_with_claude();
        let r = cached_reading_with(&Store::new(), &live_ctx(d.path(), Utc::now(), None), &failing, 0.0);
        assert_eq!(r.percent_left, None);
        assert!(!r.exact);
        assert!(r.note.contains("HTTP 500") && r.note.contains("unknown"));
    }

    #[test]
    fn api_failure_and_corrupt_logs_is_unknown() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join(".claude/projects/-p");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("s.jsonl"), "{\"usage\": nope\n\u{0}\u{1}garbage\n{\"type\":\"assistant\"}").unwrap();
        fs::write(proj.join("bin.jsonl"), [0xffu8, 0xfe, 0x00, 0x9f]).unwrap();
        let r = cached_reading_with(&Store::new(), &live_ctx(d.path(), Utc::now(), None), &failing, 0.0);
        assert_eq!(r.percent_left, None);
    }

    #[test]
    fn keychain_timeout_is_hard_error_without_estimate() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join(".claude/projects/-p");
        fs::create_dir_all(&proj).unwrap();
        let now = Utc::now();
        fs::write(proj.join("s.jsonl"), assistant(&rfc3339(now - Duration::hours(30)), "a", 10, 0)).unwrap();
        let hung = |_: &Ctx| -> Result<Reading, LiveError> {
            Err(LiveError { msg: "Keychain lookup did not finish within 3s".into(), hard: true, status: None, retry_after: None })
        };
        let r = cached_reading_with(&Store::new(), &live_ctx(d.path(), now, None), &hung, 0.0);
        assert_eq!(r.percent_left, None);
        assert!(r.note.contains("Keychain"));
    }

    #[test]
    fn retry_after_parsing() {
        let now = t("2026-11-15T12:00:00Z");
        assert_eq!(retry_after_secs("120", now), Some(120));
        assert_eq!(retry_after_secs(" 0 ", now), Some(0));
        assert_eq!(retry_after_secs("Sun, 15 Nov 2026 12:05:00 GMT", now), Some(300));
        assert_eq!(retry_after_secs("Sun, 15 Nov 2026 11:00:00 GMT", now), Some(0));
        assert_eq!(retry_after_secs("soon", now), None);
    }

    #[test]
    fn rate_limited_with_retry_after_seconds_blocks_calls() {
        let d = home_with_claude();
        let store = Store::new();
        let now = t("2026-11-15T12:00:00Z");
        let r = cached_reading_with(&store, &live_ctx(d.path(), now, None), &rate_limited(Some("120")), 0.0);
        assert!(r.note.contains("429") && r.note.contains("retrying in 2m"));
        let st = store.snapshot(None);
        assert_eq!(st.backoff_until, Some(now + Duration::seconds(120)));
        // Within the backoff: no call.
        cached_reading_with(&store, &live_ctx(d.path(), now + Duration::seconds(119), None), &never, 0.0);
        // After it: allowed again.
        let r = cached_reading_with(&store, &live_ctx(d.path(), now + Duration::seconds(121), None), &ok_reading(70.0, None), 0.0);
        assert_eq!(r.percent_left, Some(70.0));
        assert!(r.exact);
        assert_eq!(store.snapshot(None).failures, 0);
    }

    #[test]
    fn rate_limited_with_retry_after_date() {
        let d = home_with_claude();
        let store = Store::new();
        let now = t("2026-11-15T12:00:00Z");
        cached_reading_with(&store, &live_ctx(d.path(), now, None), &rate_limited(Some("Sun, 15 Nov 2026 12:07:00 GMT")), 0.0);
        assert_eq!(store.snapshot(None).backoff_until, Some(t("2026-11-15T12:07:00Z")));
        cached_reading_with(&store, &live_ctx(d.path(), t("2026-11-15T12:06:59Z"), None), &never, 0.0);
    }

    #[test]
    fn backoff_without_header_grows_and_caps() {
        let secs: Vec<i64> = (1..=8).map(|n| backoff_secs(n, 0.0)).collect();
        assert_eq!(secs, [60, 120, 240, 480, 900, 900, 900, 900]);
        assert!((1..=40).all(|n| backoff_secs(n, 1.0) <= BACKOFF_CAP_SECS));
        assert_eq!(backoff_secs(1, 1.0), 72); // +20% jitter
        assert_eq!(backoff_secs(u32::MAX, 0.5), BACKOFF_CAP_SECS);

        // Through the state machine: each retry after the backoff expires waits longer.
        let d = home_with_claude();
        let store = Store::new();
        let mut now = t("2026-11-15T12:00:00Z");
        let mut waits = Vec::new();
        for _ in 0..6 {
            cached_reading_with(&store, &live_ctx(d.path(), now, None), &rate_limited(None), 0.0);
            let until = store.snapshot(None).backoff_until.unwrap();
            waits.push((until - now).num_seconds());
            now = until;
        }
        assert_eq!(waits, [60, 120, 240, 480, 900, 900]);
    }

    #[test]
    fn serves_last_good_during_backoff() {
        let d = home_with_claude();
        let store = Store::new();
        let now = t("2026-11-15T12:00:00Z");
        let reset = Some(t("2026-11-15T15:00:00Z"));
        let r = cached_reading_with(&store, &live_ctx(d.path(), now, None), &ok_reading(40.0, reset), 0.0);
        assert!(r.exact && r.percent_left == Some(40.0));
        // Within the 5-minute success TTL: served from state, no call.
        let r = cached_reading_with(&store, &live_ctx(d.path(), now + Duration::seconds(299), None), &never, 0.0);
        assert!(r.exact && r.percent_left == Some(40.0));
        // TTL expired, API rate-limited: last good, flagged, with age and retry hint.
        let later = now + Duration::minutes(6);
        let r = cached_reading_with(&store, &live_ctx(d.path(), later, None), &rate_limited(Some("120")), 0.0);
        assert_eq!(r.percent_left, Some(40.0));
        assert_eq!(r.resets_at, reset);
        assert!(!r.exact);
        assert!(r.note.contains("Last exact reading 6m ago"), "{}", r.note);
        assert!(r.note.contains("rate-limited") && r.note.contains("retrying in 2m"), "{}", r.note);
    }

    #[test]
    fn persisted_state_loaded_on_startup() {
        let d = home_with_claude();
        let cache = tempfile::tempdir().unwrap();
        let now = t("2026-11-15T12:00:00Z");
        let reset = Some(t("2026-11-15T15:00:00Z"));
        cached_reading_with(&Store::new(), &live_ctx(d.path(), now, Some(cache.path())), &ok_reading(33.0, reset), 0.0);
        let saved = fs::read_to_string(cache.path().join(STATE_FILE)).unwrap();
        assert!(saved.contains("33.0") && !saved.to_lowercase().contains("token\""));

        // "Restart": fresh store, same cache dir -> reading served without calling the API.
        let r = cached_reading_with(&Store::new(), &live_ctx(d.path(), now + Duration::minutes(1), Some(cache.path())), &never, 0.0);
        assert_eq!(r.percent_left, Some(33.0));
        assert!(r.exact);

        // A persisted backoff is honoured after restart too.
        let later = now + Duration::minutes(10);
        cached_reading_with(&Store::new(), &live_ctx(d.path(), later, Some(cache.path())), &rate_limited(Some("600")), 0.0);
        let r = cached_reading_with(&Store::new(), &live_ctx(d.path(), later + Duration::minutes(5), Some(cache.path())), &never, 0.0);
        assert_eq!(r.percent_left, Some(33.0));
        assert!(!r.exact);

        // Corrupt state file -> defaults, no panic.
        fs::write(cache.path().join(STATE_FILE), "{not json").unwrap();
        let r = cached_reading_with(&Store::new(), &live_ctx(d.path(), later, Some(cache.path())), &ok_reading(80.0, reset), 0.0);
        assert_eq!(r.percent_left, Some(80.0));
    }

    #[test]
    fn expired_last_good_shows_full_or_estimate() {
        let d = home_with_claude();
        let store = Store::new();
        let now = t("2026-11-15T12:00:00Z");
        cached_reading_with(&store, &live_ctx(d.path(), now, None), &ok_reading(5.0, Some(t("2026-11-15T13:00:00Z"))), 0.0);
        // After the reset the API is rate-limited and there are no logs: full, not exact.
        let after = t("2026-11-15T13:30:00Z");
        let r = cached_reading_with(&store, &live_ctx(d.path(), after, None), &rate_limited(None), 0.0);
        let mut u = AgentUsage::new("claude-code", "Claude Code");
        apply(&mut u, &r, after);
        assert_eq!(u.percent_left, Some(100.0));
        assert!(!u.exact);

        // With logs showing usage in the new window, the estimate is used instead.
        let proj = d.path().join(".claude/projects/-p");
        fs::create_dir_all(&proj).unwrap();
        let lines = [
            assistant(&rfc3339(after - Duration::hours(30)), "old", 1000, 0),
            assistant(&rfc3339(after - Duration::minutes(10)), "new", 250, 0),
        ];
        fs::write(proj.join("s.jsonl"), lines.join("\n")).unwrap();
        let r = cached_reading_with(&Store::new_with(store.snapshot(None)), &live_ctx(d.path(), after, None), &never, 0.0);
        assert_eq!(r.percent_left, Some(75.0));
        assert!(!r.exact && r.note.contains("Estimate"));
    }

    #[test]
    fn single_flight_no_concurrent_calls() {
        use std::sync::mpsc;
        let d = home_with_claude();
        let store = Store::new();
        let now = t("2026-11-15T12:00:00Z");
        let ctx = live_ctx(d.path(), now, None);
        let (started_tx, started_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        std::thread::scope(|s| {
            let a = s.spawn(|| {
                let slow = move |_: &Ctx| -> Result<Reading, LiveError> {
                    started_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                    Ok(Reading { percent_left: Some(42.0), resets_at: None, exact: true, note: "Live".into() })
                };
                cached_reading_with(&store, &ctx, &slow, 0.0)
            });
            started_rx.recv().unwrap();
            // While A's fetch is in flight, B must not call the API.
            cached_reading_with(&store, &ctx, &never, 0.0);
            go_tx.send(()).unwrap();
            assert_eq!(a.join().unwrap().percent_left, Some(42.0));
        });
        let r = cached_reading_with(&store, &ctx, &never, 0.0);
        assert_eq!(r.percent_left, Some(42.0));
    }

    #[test]
    fn far_future_backoff_is_clamped() {
        let d = home_with_claude();
        let cache = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let st = LiveState { backoff_until: Some(now + Duration::days(3650)), failures: 3, ..Default::default() };
        fs::write(cache.path().join(STATE_FILE), serde_json::to_string(&st).unwrap()).unwrap();
        let store = Store::new();
        let loaded = store.snapshot(Some(&cache.path().join(STATE_FILE)));
        let until = loaded.backoff_until.unwrap();
        assert!(until <= Utc::now() + Duration::seconds(RETRY_AFTER_MAX_SECS));
        // So fetching resumes after at most the cap, not in ten years.
        let r = cached_reading_with(
            &store,
            &live_ctx(d.path(), now + Duration::seconds(RETRY_AFTER_MAX_SECS + 5), Some(cache.path())),
            &ok_reading(64.0, None),
            0.0,
        );
        assert_eq!(r.percent_left, Some(64.0));
        assert!(r.exact);

        // Also bounded per poll (e.g. after the clock jumps backwards).
        let mut st = LiveState { backoff_until: Some(now + Duration::days(1)), ..Default::default() };
        sanitize(&mut st, now);
        assert_eq!(st.backoff_until, Some(now + Duration::seconds(RETRY_AFTER_MAX_SECS)));
    }

    #[test]
    fn backward_clock_jump_unblocks_within_cap() {
        let d = home_with_claude();
        let cache = tempfile::tempdir().unwrap();
        let store = Store::new();
        let t0 = t("2026-11-15T12:00:00Z");
        // Backoff starts (Retry-After 10 min) ...
        cached_reading_with(&store, &live_ctx(d.path(), t0, Some(cache.path())), &rate_limited(Some("600")), 0.0);
        // ... then the clock jumps back 24h.
        let jumped = t0 - Duration::hours(24);
        let ctx_at = |secs: i64| live_ctx(d.path(), jumped + Duration::seconds(secs), Some(cache.path()));
        // Poll every 45 s; the API must not be hit until the 1h cap, and must be hit right after.
        let mut secs = 0;
        while secs < RETRY_AFTER_MAX_SECS {
            cached_reading_with(&store, &ctx_at(secs), &never, 0.0);
            secs += 45;
        }
        let r = cached_reading_with(&store, &ctx_at(RETRY_AFTER_MAX_SECS), &ok_reading(61.0, None), 0.0);
        assert_eq!(r.percent_left, Some(61.0));
        assert!(r.exact);
        // The corrected deadline was persisted to the authoritative state/file.
        let st = store.snapshot(Some(&cache.path().join(STATE_FILE)));
        assert!(st.backoff_until.is_none() && st.failures == 0);
    }

    #[test]
    fn clamp_is_applied_to_stored_state_and_persisted() {
        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join(STATE_FILE);
        let now = t("2026-11-15T12:00:00Z");
        let store = Store::new_with(LiveState { backoff_until: Some(now + Duration::hours(24)), ..Default::default() });
        // new_with stores path None; use None for the in-memory check.
        store.current(None, now);
        assert_eq!(store.snapshot(None).backoff_until, Some(now + Duration::seconds(RETRY_AFTER_MAX_SECS)));
        // With a file: the corrected deadline is written back.
        fs::write(&path, serde_json::to_string(&LiveState { backoff_until: Some(Utc::now() + Duration::hours(24)), ..Default::default() }).unwrap()).unwrap();
        let s2 = Store::new();
        let n = Utc::now();
        s2.current(Some(&path), n);
        let on_disk: LiveState = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(on_disk.backoff_until.unwrap() <= n + Duration::seconds(RETRY_AFTER_MAX_SECS));
    }

    #[test]
    fn future_fetched_at_is_stale() {
        let now = t("2026-11-15T12:00:00Z");
        let g = |fetched: DateTime<Utc>| GoodReading { percent_left: 50.0, resets_at: Some(t("2026-11-15T15:00:00Z")), fetched_at: fetched, note: String::new() };
        let st = LiveState { last_good: Some(g(now + Duration::days(30))), ..Default::default() };
        assert!(should_fetch(&st, now));
        let st = LiveState { last_good: Some(g(now + Duration::seconds(30))), ..Default::default() };
        assert!(!should_fetch(&st, now)); // small skew tolerated

        let d = home_with_claude();
        let store = Store::new_with(LiveState { last_good: Some(g(now + Duration::days(30))), ..Default::default() });
        let r = cached_reading_with(&store, &live_ctx(d.path(), now, None), &ok_reading(20.0, None), 0.0);
        assert_eq!(r.percent_left, Some(20.0));
    }

    #[test]
    fn backoff_set_by_concurrent_caller_is_rechecked_after_acquire() {
        use std::sync::Arc;
        let d = home_with_claude();
        let store = Arc::new(Store::new());
        let now = t("2026-11-15T12:00:00Z");
        let ctx = live_ctx(d.path(), now, None);
        // Caller A passes the first check; before it takes ownership, caller B
        // runs a full poll that gets a 429 and starts a backoff.
        let (s2, c2) = (Arc::clone(&store), ctx.clone());
        *store.before_acquire.lock().unwrap() = Some(Box::new(move || {
            cached_reading_with(&s2, &c2, &rate_limited(Some("300")), 0.0);
        }));
        let r = cached_reading_with(&store, &ctx, &never, 0.0); // A must not fetch
        assert!(r.note.contains("rate-limited"), "{}", r.note);
        assert_eq!(store.snapshot(None).failures, 1);
    }

    #[test]
    fn concurrent_saves_never_corrupt_or_leak_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        std::thread::scope(|s| {
            for i in 0..8u32 {
                let path = &path;
                s.spawn(move || {
                    for j in 0..25u32 {
                        let st = LiveState { failures: i * 1000 + j, last_error: Some("x".repeat(2000)), ..Default::default() };
                        save_state(Some(path), &st);
                    }
                });
            }
        });
        let parsed: LiveState = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.last_error.unwrap().len(), 2000);
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [std::ffi::OsString::from(STATE_FILE)]);
    }

    #[test]
    fn short_ttl_right_after_reset() {
        let now = t("2026-11-15T12:00:00Z");
        let g = |fetched: DateTime<Utc>| GoodReading { percent_left: 90.0, resets_at: Some(t("2026-11-15T17:00:00Z")), fetched_at: fetched, note: String::new() };
        let mut st = LiveState { last_good: Some(g(now - Duration::seconds(90))), ..Default::default() };
        assert!(!should_fetch(&st, now)); // normal: 5 min TTL
        st.reset_seen_at = Some(now - Duration::minutes(3));
        assert!(should_fetch(&st, now)); // just after a reset: 60 s TTL
        st.backoff_until = Some(now + Duration::seconds(1));
        assert!(!should_fetch(&st, now)); // never during backoff
    }

    #[test]
    fn reset_since_reading_shows_full() {
        let now = t("2026-11-15T12:00:00Z");
        let r = Reading { percent_left: Some(5.0), resets_at: Some(t("2026-11-15T11:00:00Z")), exact: true, note: "x".into() };
        let mut u = AgentUsage::new("claude-code", "Claude Code");
        apply(&mut u, &r, now);
        assert_eq!(u.percent_left, Some(100.0));
        assert!(u.resets_at.is_none());
        assert!(!u.exact, "post-reset usage is unobserved, so not exact");
        assert!(u.note.unwrap().contains("no newer reading"));
    }
}
