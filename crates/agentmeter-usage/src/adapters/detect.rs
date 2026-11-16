//! Agents without a reliable local quota source: detect installation and
//! activity only, and say so in the note. Numbers are never fabricated.

use std::path::Path;

use super::Adapter;
use crate::util::{is_recent, newest_mtime};
use crate::{AgentUsage, Ctx};

pub struct DetectOnly {
    id: &'static str,
    name: &'static str,
    /// Paths relative to $HOME whose existence means "installed".
    markers: &'static [&'static str],
    /// Absolute paths (e.g. app bundles) checked only in live mode.
    abs_markers: &'static [&'static str],
    /// Dirs relative to $HOME (with walk depth) whose newest mtime signals activity.
    activity: &'static [(&'static str, usize)],
    note: &'static str,
}

impl Adapter for DetectOnly {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn installed(&self, ctx: &Ctx) -> bool {
        self.markers.iter().any(|m| ctx.path(m).exists())
            || (ctx.live && self.abs_markers.iter().any(|m| Path::new(m).exists()))
    }
    fn poll(&self, ctx: &Ctx) -> Result<AgentUsage, String> {
        let mut u = AgentUsage::new(self.id, self.name);
        u.installed = self.installed(ctx);
        if !u.installed {
            return Ok(u);
        }
        let newest = self
            .activity
            .iter()
            .filter_map(|(rel, depth)| newest_mtime(&ctx.path(rel), *depth))
            .max();
        u.active = is_recent(newest, ctx.now);
        u.note = Some(self.note.to_string());
        Ok(u)
    }
}

pub fn gemini() -> DetectOnly {
    DetectOnly {
        id: "gemini",
        name: "Gemini CLI",
        markers: &[".gemini"],
        abs_markers: &[],
        activity: &[(".gemini/tmp", 4)],
        note: "No local quota source: Gemini CLI keeps no rate-limit state on disk \
               (quota is only reported by Google's servers), so remaining usage is unknown.",
    }
}

pub fn cursor() -> DetectOnly {
    DetectOnly {
        id: "cursor",
        name: "Cursor",
        markers: &[".cursor"],
        abs_markers: &["/Applications/Cursor.app"],
        activity: &[(".cursor/projects", 3), (".cursor/ai-tracking", 1)],
        note: "No local quota source: Cursor usage/limits live only on cursor.com \
               (behind the web session), so remaining usage is unknown.",
    }
}

pub fn opencode() -> DetectOnly {
    DetectOnly {
        id: "opencode",
        name: "opencode",
        markers: &[".local/share/opencode", ".config/opencode"],
        abs_markers: &[],
        activity: &[(".local/share/opencode", 4)],
        note: "opencode uses your own provider API keys; it has no single usage-limit window.",
    }
}

pub fn aider() -> DetectOnly {
    DetectOnly {
        id: "aider",
        name: "Aider",
        markers: &[".aider.conf.yml", ".aider"],
        abs_markers: &[],
        activity: &[(".aider", 2)],
        note: "Aider uses your own provider API keys; it has no single usage-limit window.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::fs;

    fn ctx(home: &Path) -> Ctx {
        Ctx { home: home.to_path_buf(), now: Utc::now(), live: false }
    }

    #[test]
    fn not_installed_without_marker() {
        let d = tempfile::tempdir().unwrap();
        let u = gemini().poll(&ctx(d.path())).unwrap();
        assert!(!u.installed);
        assert!(u.note.is_none());
    }

    #[test]
    fn installed_unknown_with_note_and_activity() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".gemini/tmp/abc/chats")).unwrap();
        fs::write(d.path().join(".gemini/tmp/abc/chats/session.json"), "{}").unwrap();
        let u = gemini().poll(&ctx(d.path())).unwrap();
        assert!(u.installed);
        assert!(u.active);
        assert!(u.percent_left.is_none());
        assert!(!u.exact);
        assert!(u.note.unwrap().contains("unknown"));
    }

    #[test]
    fn cursor_installed_idle() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".cursor")).unwrap();
        let u = cursor().poll(&ctx(d.path())).unwrap();
        assert!(u.installed && !u.active && u.percent_left.is_none());
    }
}
