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
//! Readings are cached for 60 s.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use chrono::{DateTime, Duration, DurationRound, Utc};
use serde_json::Value;

use super::Adapter;
use crate::util::{clamp_pct, is_jsonl, is_recent, rfc3339, round1, walk_files};
use crate::{AgentUsage, Ctx};

pub struct ClaudeCode;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const CACHE_TTL: StdDuration = StdDuration::from_secs(60);
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(5);
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

fn read_credentials_json(home: &Path) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
            .stderr(std::process::Stdio::null())
            .output();
        if let Ok(o) = out {
            if o.status.success() {
                if let Ok(s) = String::from_utf8(o.stdout) {
                    if !s.trim().is_empty() {
                        return Ok(s);
                    }
                }
            }
        }
    }
    fs::read_to_string(home.join(".claude/.credentials.json"))
        .map_err(|_| "no Claude Code OAuth credentials found (not logged in with a Claude subscription?)".into())
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

fn fetch_live(home: &Path, now: DateTime<Utc>) -> Result<Reading, String> {
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
            parse_oauth_usage(&body)
        }
        // Only the status code is reported: never echo headers/body.
        Err(ureq::Error::Status(code, _)) => Err(format!("usage API returned HTTP {code}")),
        Err(ureq::Error::Transport(t)) => Err(format!("usage API unreachable ({})", t.kind())),
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
    let bl = blocks(entries);
    let base = "Estimate from local Claude Code logs (exact usage API unavailable)";
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

static CACHE: Mutex<Option<(Instant, PathBuf, Reading)>> = Mutex::new(None);
/// Token budget learned from the last successful exact reading (process-local).
static CALIBRATED_LIMIT: Mutex<Option<u64>> = Mutex::new(None);

fn compute(ctx: &Ctx) -> Reading {
    let projects = ctx.path(".claude/projects");
    let live_err = if ctx.live {
        match fetch_live(&ctx.home, ctx.now) {
            Ok(r) => {
                if let Some(left) = r.percent_left {
                    let entries = collect_entries(&projects, ctx.now);
                    let used = current_block(&blocks(&entries), ctx.now).map_or(0, |b| b.tokens);
                    if let Some(limit) = calibrate(used, 100.0 - left) {
                        *CALIBRATED_LIMIT.lock().unwrap_or_else(|p| p.into_inner()) = Some(limit);
                    }
                }
                return r;
            }
            Err(e) => Some(e),
        }
    } else {
        None
    };
    let limit = if ctx.live { *CALIBRATED_LIMIT.lock().unwrap_or_else(|p| p.into_inner()) } else { None };
    let mut r = estimate(&collect_entries(&projects, ctx.now), ctx.now, limit);
    if let Some(e) = live_err {
        r.note = format!("{} [live: {e}]", r.note);
    }
    r
}

fn cached_reading(ctx: &Ctx) -> Reading {
    if !ctx.live {
        return compute(ctx);
    }
    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, home, r)) = guard.as_ref() {
        if at.elapsed() < CACHE_TTL && *home == ctx.home {
            return r.clone();
        }
    }
    let r = compute(ctx);
    *guard = Some((Instant::now(), ctx.home.clone(), r.clone()));
    r
}

/// Render a reading into the row, handling a window that has reset since the reading.
pub fn apply(u: &mut AgentUsage, r: &Reading, now: DateTime<Utc>) {
    u.window = Some("5h".into());
    u.exact = r.exact;
    match r.resets_at {
        Some(reset) if reset <= now => {
            u.percent_left = Some(100.0);
            u.resets_at = None;
            u.note = Some(format!("{}; window reset at {}", r.note, rfc3339(reset)));
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
        // nothing at all
        assert_eq!(estimate(&[], now, None).percent_left, Some(100.0));
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

        let ctx = Ctx { home: d.path().to_path_buf(), now, live: false };
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
        let ctx = Ctx { home: d.path().to_path_buf(), now: Utc::now(), live: false };
        let u = ClaudeCode.poll(&ctx).unwrap();
        assert!(u.installed && !u.active);
        assert_eq!(u.percent_left, Some(100.0));
    }

    #[test]
    fn reset_since_reading_shows_full() {
        let now = t("2026-11-15T12:00:00Z");
        let r = Reading { percent_left: Some(5.0), resets_at: Some(t("2026-11-15T11:00:00Z")), exact: true, note: "x".into() };
        let mut u = AgentUsage::new("claude-code", "Claude Code");
        apply(&mut u, &r, now);
        assert_eq!(u.percent_left, Some(100.0));
        assert!(u.resets_at.is_none());
    }
}
