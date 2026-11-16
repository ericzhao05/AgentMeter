//! AgentMeter data layer: reads, per installed AI coding agent, how much of its
//! current usage-limit window is left.
//!
//! The public surface is intentionally tiny:
//! * [`AgentUsage`] – one row per known agent.
//! * [`poll_all`] – synchronous real reading (never panics).
//! * [`mock_all`] – deterministic demo data for UI development.
//!
//! Internally each agent is an [`adapters::Adapter`]; the home directory and
//! "now" are injected through [`Ctx`] so every parser is testable against
//! synthetic fixtures.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;

mod adapters;
mod mock;
mod util;

pub use mock::{mock_all, mock_at};

/// Usage snapshot for one agent. Serialized camelCase for the Tauri frontend.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsage {
    /// Stable id: "claude-code" | "codex" | "gemini" | "cursor" | "opencode" | "aider".
    pub id: String,
    /// Human readable name.
    pub name: String,
    /// Whether the agent looks installed on this machine.
    pub installed: bool,
    /// Percentage of the current limit window still available (0..=100). `None` = unknown.
    pub percent_left: Option<f64>,
    /// When the current window resets, RFC3339.
    pub resets_at: Option<String>,
    /// Window length label, e.g. "5h".
    pub window: Option<String>,
    /// Agent showed activity within roughly the last 2 minutes.
    pub active: bool,
    /// `true` = value comes from an authoritative source; `false` = estimate.
    pub exact: bool,
    /// Free-form explanation (source, caveats, errors).
    pub note: Option<String>,
}

impl AgentUsage {
    pub(crate) fn new(id: &str, name: &str) -> Self {
        AgentUsage {
            id: id.to_string(),
            name: name.to_string(),
            installed: false,
            percent_left: None,
            resets_at: None,
            window: None,
            active: false,
            exact: false,
            note: None,
        }
    }
}

/// Environment handed to every adapter. Injectable for tests.
#[derive(Clone, Debug)]
pub(crate) struct Ctx {
    pub home: PathBuf,
    pub now: DateTime<Utc>,
    /// Allow network / Keychain access (Claude OAuth usage endpoint). Off in tests.
    pub live: bool,
    /// Where adapters may persist small state files (no secrets). None = don't persist.
    pub cache_dir: Option<PathBuf>,
}

impl Ctx {
    pub fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }
}

/// Poll every known agent once. Synchronous; never panics. One entry per known
/// agent, installed or not. Per-adapter failures become `note` + `percent_left: None`.
pub fn poll_all() -> Vec<AgentUsage> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let cache_dir = dirs::cache_dir().map(|d| d.join("agentmeter"));
    poll_with(&Ctx { home, now: Utc::now(), live: true, cache_dir })
}

/// Same as [`poll_all`] but against an arbitrary home directory with network and
/// Keychain access disabled (file-based sources only). Useful for tests/demos.
pub fn poll_all_at(home: &Path) -> Vec<AgentUsage> {
    poll_with(&Ctx { home: home.to_path_buf(), now: Utc::now(), live: false, cache_dir: None })
}

pub(crate) fn poll_with(ctx: &Ctx) -> Vec<AgentUsage> {
    adapters::all()
        .iter()
        .map(|a| {
            let res = catch_unwind(AssertUnwindSafe(|| a.poll(ctx)));
            match res {
                Ok(Ok(u)) => u,
                Ok(Err(e)) => {
                    let mut u = AgentUsage::new(a.id(), a.name());
                    u.installed = a.installed(ctx);
                    u.note = Some(format!("error: {e}"));
                    u
                }
                Err(_) => {
                    let mut u = AgentUsage::new(a.id(), a.name());
                    u.note = Some("error: adapter panicked".into());
                    u
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_home_lists_every_agent_uninstalled() {
        let dir = tempfile::tempdir().unwrap();
        let all = poll_all_at(dir.path());
        let ids: Vec<_> = all.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["claude-code", "codex", "gemini", "cursor", "opencode", "aider"]);
        assert!(all.iter().all(|a| !a.installed && a.percent_left.is_none()));
    }

    #[test]
    fn serializes_camel_case() {
        let mut u = AgentUsage::new("codex", "Codex");
        u.percent_left = Some(50.0);
        let v = serde_json::to_value(&u).unwrap();
        assert!(v.get("percentLeft").is_some());
        assert!(v.get("resetsAt").is_some());
        assert!(v.get("percent_left").is_none());
    }
}
