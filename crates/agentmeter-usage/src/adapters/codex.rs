//! Codex CLI: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Each turn appends an `event_msg` whose payload is
//! `{"type":"token_count","info":{..},"rate_limits":{"primary":{..},"secondary":{..}}}`.
//! `primary` is the 5-hour window, `secondary` the weekly one; each carries
//! `used_percent`, `window_minutes` and either `resets_at` (epoch seconds, current
//! CLI) or `resets_in_seconds` (relative to the event timestamp, older CLI).

use std::fs;
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use super::Adapter;
use crate::util::{clamp_pct, is_jsonl, is_recent, rfc3339, round1, walk_files, window_label};
use crate::{AgentUsage, Ctx};

pub struct Codex;

/// How many of the newest session files to inspect for a rate-limit reading.
const MAX_FILES: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub at: Option<DateTime<Utc>>,
    pub primary: Window,
    pub secondary: Option<Window>,
}

fn parse_window(v: &Value, event_at: Option<DateTime<Utc>>) -> Option<Window> {
    let used = v.get("used_percent")?.as_f64()?;
    let window_minutes = v.get("window_minutes").and_then(Value::as_i64);
    let resets_at = if let Some(ts) = v.get("resets_at").and_then(Value::as_i64) {
        Utc.timestamp_opt(ts, 0).single()
    } else if let (Some(secs), Some(at)) =
        (v.get("resets_in_seconds").and_then(Value::as_i64), event_at)
    {
        Some(at + chrono::Duration::seconds(secs))
    } else {
        None
    };
    Some(Window { used_percent: used, window_minutes, resets_at })
}

/// Parse one JSONL line; returns a reading if it is a token_count event with rate limits.
pub fn parse_line(line: &str) -> Option<Reading> {
    let v: Value = serde_json::from_str(line).ok()?;
    let payload = v.get("payload")?;
    if payload.get("type")?.as_str()? != "token_count" {
        return None;
    }
    let at = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let rl = payload.get("rate_limits")?;
    let primary = parse_window(rl.get("primary")?, at)?;
    let secondary = rl.get("secondary").and_then(|s| parse_window(s, at));
    Some(Reading { at, primary, secondary })
}

/// Newest reading in a file's contents (last matching line wins).
pub fn last_reading(contents: &str) -> Option<Reading> {
    contents.lines().rev().find_map(parse_line)
}

/// Find the newest reading across the most recently modified session files.
///
/// File mtime only selects candidates: a file can be touched by a non-quota
/// event after a newer reading was written elsewhere, so the winner is the
/// reading with the latest event timestamp. Readings without a timestamp rank
/// below timestamped ones; ties fall back to file mtime order.
pub fn newest_reading(sessions: &Path) -> Option<Reading> {
    let mut files = walk_files(sessions, 4, &is_jsonl);
    files.sort_by(|a, b| b.1.cmp(&a.1));
    let mut best: Option<Reading> = None;
    for (p, _) in files.iter().take(MAX_FILES) {
        let Some(r) = fs::read_to_string(p).ok().and_then(|c| last_reading(&c)) else { continue };
        if best.as_ref().map_or(true, |b| r.at > b.at) {
            best = Some(r);
        }
    }
    best
}

/// Turn a reading into the user-facing row fields.
pub fn apply(u: &mut AgentUsage, r: &Reading, now: DateTime<Utc>) {
    let p = &r.primary;
    u.window = Some(window_label(p.window_minutes.unwrap_or(300)));
    u.exact = true;
    let mut notes = Vec::new();
    match p.resets_at {
        Some(reset) if reset <= now => {
            // Usage since the reset has not been observed: assume full, flag as estimate.
            u.percent_left = Some(100.0);
            u.resets_at = None;
            u.exact = false;
            notes.push(format!(
                "Window reset at {}; no newer reading, assuming full",
                rfc3339(reset)
            ));
        }
        reset => {
            u.percent_left = Some(round1(clamp_pct(100.0 - p.used_percent)));
            u.resets_at = reset.map(rfc3339);
        }
    }
    if let Some(s) = &r.secondary {
        let label = window_label(s.window_minutes.unwrap_or(10080));
        let left = match s.resets_at {
            Some(reset) if reset <= now => 100.0,
            _ => round1(clamp_pct(100.0 - s.used_percent)),
        };
        notes.push(format!("{label} window: {left}% left"));
    }
    if let Some(at) = r.at {
        notes.push(format!("from session log at {}", rfc3339(at)));
    }
    u.note = Some(notes.join("; "));
}

impl Adapter for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn name(&self) -> &'static str {
        "Codex"
    }
    fn installed(&self, ctx: &Ctx) -> bool {
        ctx.path(".codex").is_dir()
    }
    fn poll(&self, ctx: &Ctx) -> Result<AgentUsage, String> {
        let mut u = AgentUsage::new(self.id(), self.name());
        u.installed = self.installed(ctx);
        if !u.installed {
            return Ok(u);
        }
        let sessions = ctx.path(".codex/sessions");
        let newest = walk_files(&sessions, 4, &is_jsonl).into_iter().map(|(_, m)| m).max();
        u.active = is_recent(newest, ctx.now);
        match newest_reading(&sessions) {
            Some(r) => apply(&mut u, &r, ctx.now),
            None => {
                u.note = Some(
                    "No rate-limit reading found in recent Codex session logs yet \
                     (run a Codex turn to populate)"
                        .into(),
                );
            }
        }
        Ok(u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn line(used: f64, resets_at: i64, ts: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":10}}}},"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{used},"window_minutes":300,"resets_at":{resets_at}}},"secondary":{{"used_percent":20.0,"window_minutes":10080,"resets_at":{resets_at}}},"plan_type":"plus"}}}}}}"#
        )
    }

    fn ctx(home: &Path, now: DateTime<Utc>) -> Ctx {
        Ctx { home: home.to_path_buf(), now, live: false }
    }

    #[test]
    fn parses_current_schema() {
        let r = parse_line(&line(31.0, 2_000_000_000, "2026-11-15T10:00:00.000Z")).unwrap();
        assert_eq!(r.primary.used_percent, 31.0);
        assert_eq!(r.primary.window_minutes, Some(300));
        assert_eq!(r.primary.resets_at.unwrap().timestamp(), 2_000_000_000);
        assert_eq!(r.secondary.unwrap().window_minutes, Some(10080));
    }

    #[test]
    fn parses_legacy_resets_in_seconds() {
        let l = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":50,"window_minutes":300,"resets_in_seconds":3600}}}}"#;
        let r = parse_line(l).unwrap();
        assert_eq!(rfc3339(r.primary.resets_at.unwrap()), "2026-01-01T01:00:00Z");
        assert!(r.secondary.is_none());
    }

    #[test]
    fn ignores_other_and_malformed_lines() {
        assert!(parse_line("not json").is_none());
        assert!(parse_line(r#"{"type":"event_msg","payload":{"type":"agent_message"}}"#).is_none());
        assert!(parse_line(r#"{"payload":{"type":"token_count","rate_limits":null}}"#).is_none());
        assert!(parse_line(r#"{"payload":{"type":"token_count","rate_limits":{"primary":{}}}}"#)
            .is_none());
    }

    #[test]
    fn last_matching_line_wins() {
        let c = format!(
            "{}\n{}\ngarbage\n{{\"payload\":{{\"type\":\"agent_message\"}}}}\n",
            line(10.0, 2_000_000_000, "2026-11-15T10:00:00Z"),
            line(42.0, 2_000_000_000, "2026-11-15T10:05:00Z")
        );
        assert_eq!(last_reading(&c).unwrap().primary.used_percent, 42.0);
    }

    #[test]
    fn full_adapter_with_fixture_tree() {
        let d = tempfile::tempdir().unwrap();
        let day = d.path().join(".codex/sessions/2026/11/15");
        fs::create_dir_all(&day).unwrap();
        let now = Utc::now();
        let reset = (now + Duration::hours(2)).timestamp();
        fs::write(day.join("rollout-a.jsonl"), line(25.0, reset, "2026-11-15T10:00:00Z")).unwrap();
        let u = Codex.poll(&ctx(d.path(), now)).unwrap();
        assert!(u.installed && u.exact && u.active);
        assert_eq!(u.percent_left, Some(75.0));
        assert_eq!(u.window.as_deref(), Some("5h"));
        assert!(u.resets_at.is_some());
        assert!(u.note.unwrap().contains("7d window: 80% left"));
    }

    #[test]
    fn expired_window_reads_full() {
        let now = Utc::now();
        let r = parse_line(&line(95.0, (now - Duration::minutes(5)).timestamp(), "2026-11-15T10:00:00Z"))
            .unwrap();
        let mut u = AgentUsage::new("codex", "Codex");
        apply(&mut u, &r, now);
        assert_eq!(u.percent_left, Some(100.0));
        assert!(u.resets_at.is_none());
        assert!(!u.exact, "post-reset usage is unobserved, so not exact");
        assert!(u.note.unwrap().contains("no newer reading"));
    }

    #[test]
    fn newest_reading_by_event_time_not_mtime() {
        use std::time::{Duration as StdDuration, SystemTime};
        let d = tempfile::tempdir().unwrap();
        let sessions = d.path().join("sessions");
        let day = sessions.join("2026/11/15");
        fs::create_dir_all(&day).unwrap();
        // Older session: older quota reading, but touched most recently by a non-quota event.
        let old = day.join("rollout-old.jsonl");
        fs::write(
            &old,
            format!(
                "{}\n{{\"timestamp\":\"2026-11-15T12:00:00Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"agent_message\"}}}}\n",
                line(10.0, 2_000_000_000, "2026-11-15T09:00:00Z")
            ),
        )
        .unwrap();
        // Newer session: newer quota reading, older mtime.
        let new = day.join("rollout-new.jsonl");
        fs::write(&new, line(55.0, 2_000_000_000, "2026-11-15T11:00:00Z")).unwrap();
        let t0 = SystemTime::now();
        fs::File::options().write(true).open(&new).unwrap().set_modified(t0 - StdDuration::from_secs(600)).unwrap();
        fs::File::options().write(true).open(&old).unwrap().set_modified(t0).unwrap();

        let r = newest_reading(&sessions).unwrap();
        assert_eq!(r.primary.used_percent, 55.0);
    }

    #[test]
    fn installed_without_sessions_or_with_malformed_file() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".codex")).unwrap();
        let u = Codex.poll(&ctx(d.path(), Utc::now())).unwrap();
        assert!(u.installed && u.percent_left.is_none() && u.note.is_some());

        let day = d.path().join(".codex/sessions/2026/11/15");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("bad.jsonl"), "{{{{\n\u{0}\nnope").unwrap();
        let u = Codex.poll(&ctx(d.path(), Utc::now())).unwrap();
        assert!(u.percent_left.is_none());
    }

    #[test]
    fn clamps_over_100_used() {
        let r = parse_line(&line(130.0, 2_000_000_000, "2026-11-15T10:00:00Z")).unwrap();
        let mut u = AgentUsage::new("codex", "Codex");
        apply(&mut u, &r, Utc.timestamp_opt(1_900_000_000, 0).unwrap());
        assert_eq!(u.percent_left, Some(0.0));
    }
}
