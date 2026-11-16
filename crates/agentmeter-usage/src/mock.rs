//! Deterministic demo data for UI development.

use chrono::{DateTime, Duration, Utc};

use crate::util::{rfc3339, round1};
use crate::AgentUsage;

/// Length of the demo "5h" window in demo seconds (drains then resets).
const CLAUDE_PERIOD: f64 = 90.0;

/// Deterministic demo data that varies smoothly with `t` seconds:
/// * Claude Code drains 100 → ~10% over 90 s, then resets to 100 (periodic reset).
/// * Codex oscillates gently around 60%.
/// * Gemini hovers low (~6–13%) to exercise the "low" state.
/// * Cursor installed but unknown (`percent_left: None`).
/// * opencode / aider not installed.
///
/// Percentages depend only on `t`. `resets_at` is relative to the current wall
/// clock, so demo countdowns look realistic (Claude's maps its demo cycle onto
/// a 5h window: full = ~5h to go, nearly empty = minutes to go).
pub fn mock_all(t: f64) -> Vec<AgentUsage> {
    mock_at(t, Utc::now())
}

/// [`mock_all`] with an explicit "now" (fully deterministic).
pub fn mock_at(t: f64, now: DateTime<Utc>) -> Vec<AgentUsage> {
    let t = if t.is_finite() { t } else { 0.0 };
    let at = |secs: f64| rfc3339(now + Duration::seconds(secs.max(60.0) as i64));

    let phase = t.rem_euclid(CLAUDE_PERIOD) / CLAUDE_PERIOD; // 0..1
    let claude_left = round1(100.0 - 90.0 * phase);
    let claude_reset = (1.0 - phase) * 5.0 * 3600.0;

    let codex_left = round1(60.0 + 25.0 * (t / 23.0).sin());
    let gemini_left = round1(9.5 + 3.5 * (t / 11.0).sin());

    let mut v = Vec::new();

    let mut c = AgentUsage::new("claude-code", "Claude Code");
    c.installed = true;
    c.percent_left = Some(claude_left);
    c.resets_at = Some(at(claude_reset));
    c.window = Some("5h".into());
    c.active = (t / 7.0).sin() > 0.0;
    c.exact = true;
    c.note = Some("Mock data".into());
    v.push(c);

    let mut x = AgentUsage::new("codex", "Codex");
    x.installed = true;
    x.percent_left = Some(codex_left);
    x.resets_at = Some(at(2.0 * 3600.0 + 3600.0 * (t / 41.0).cos()));
    x.window = Some("5h".into());
    x.active = (t / 13.0).cos() > 0.3;
    x.exact = true;
    x.note = Some("Mock data; 7d window: 71% left".into());
    v.push(x);

    let mut g = AgentUsage::new("gemini", "Gemini CLI");
    g.installed = true;
    g.percent_left = Some(gemini_left);
    g.resets_at = Some(at(45.0 * 60.0 + 600.0 * (t / 17.0).sin()));
    g.window = Some("24h".into());
    g.active = false;
    g.exact = false;
    g.note = Some("Mock data (low)".into());
    v.push(g);

    let mut cu = AgentUsage::new("cursor", "Cursor");
    cu.installed = true;
    cu.note = Some("Mock data: usage unknown".into());
    v.push(cu);

    v.push(AgentUsage::new("opencode", "opencode"));
    v.push(AgentUsage::new("aider", "Aider"));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pct(v: &[AgentUsage], id: &str) -> Option<f64> {
        v.iter().find(|a| a.id == id).unwrap().percent_left
    }

    #[test]
    fn deterministic() {
        let now = Utc::now();
        let a = serde_json::to_string(&mock_at(42.5, now)).unwrap();
        let b = serde_json::to_string(&mock_at(42.5, now)).unwrap();
        assert_eq!(a, b);
        // Percentages don't depend on the wall clock.
        let later = now + Duration::hours(3);
        let p = |v: Vec<AgentUsage>| v.into_iter().map(|a| a.percent_left).collect::<Vec<_>>();
        assert_eq!(p(mock_at(42.5, now)), p(mock_at(42.5, later)));
    }

    #[test]
    fn resets_are_in_the_future_relative_to_now() {
        // Whole-second "now" (resets_at is serialized at second precision).
        let now = DateTime::parse_from_rfc3339("2026-11-15T12:00:00Z").unwrap().with_timezone(&Utc);
        for i in 0..500 {
            for a in mock_at(i as f64 * 0.7, now) {
                if let Some(r) = a.resets_at {
                    let r = DateTime::parse_from_rfc3339(&r).unwrap().with_timezone(&Utc);
                    let secs = (r - now).num_seconds();
                    assert!((60..=5 * 3600).contains(&secs), "{} resets in {secs}s", a.id);
                }
            }
        }
        // Claude: ~5h to go when full, minutes to go when nearly drained.
        let reset = |t: f64| {
            let r = mock_at(t, now)[0].resets_at.clone().unwrap();
            (DateTime::parse_from_rfc3339(&r).unwrap().with_timezone(&Utc) - now).num_seconds()
        };
        assert!(reset(0.0) > 4 * 3600);
        assert!(reset(89.0) < 30 * 60);
    }

    #[test]
    fn has_low_agent_and_bounds() {
        for i in 0..2000 {
            let v = mock_all(i as f64 * 0.37);
            assert!(v.iter().any(|a| a.percent_left.is_some_and(|p| p < 15.0)));
            for a in &v {
                if let Some(p) = a.percent_left {
                    assert!((0.0..=100.0).contains(&p));
                }
            }
        }
    }

    #[test]
    fn claude_resets_periodically_and_moves_smoothly() {
        assert_eq!(pct(&mock_all(0.0), "claude-code"), Some(100.0));
        assert!(pct(&mock_all(89.0), "claude-code").unwrap() < 15.0);
        assert_eq!(pct(&mock_all(90.0), "claude-code"), Some(100.0));
        let d = pct(&mock_all(10.0), "codex").unwrap() - pct(&mock_all(10.1), "codex").unwrap();
        assert!(d.abs() < 1.0);
    }

    #[test]
    fn nan_is_safe() {
        assert_eq!(mock_all(f64::NAN).len(), 6);
    }
}
