//! Deterministic demo data for UI development.

use chrono::{Duration, TimeZone, Utc};

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
pub fn mock_all(t: f64) -> Vec<AgentUsage> {
    let t = if t.is_finite() { t } else { 0.0 };
    // Fixed epoch so resets_at is deterministic for a given t.
    let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let at = |secs: f64| rfc3339(base + Duration::milliseconds((secs * 1000.0) as i64));

    let phase = t.rem_euclid(CLAUDE_PERIOD) / CLAUDE_PERIOD; // 0..1
    let claude_left = round1(100.0 - 90.0 * phase);
    let claude_reset = (t / CLAUDE_PERIOD).floor() * CLAUDE_PERIOD + CLAUDE_PERIOD;

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
    x.resets_at = Some(at(t + 3600.0));
    x.window = Some("5h".into());
    x.active = (t / 13.0).cos() > 0.3;
    x.exact = true;
    x.note = Some("Mock data; 7d window: 71% left".into());
    v.push(x);

    let mut g = AgentUsage::new("gemini", "Gemini CLI");
    g.installed = true;
    g.percent_left = Some(gemini_left);
    g.resets_at = Some(at(t + 1800.0));
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
        let a = serde_json::to_string(&mock_all(42.5)).unwrap();
        let b = serde_json::to_string(&mock_all(42.5)).unwrap();
        assert_eq!(a, b);
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
