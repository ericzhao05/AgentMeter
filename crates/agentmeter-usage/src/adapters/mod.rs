use crate::{AgentUsage, Ctx};

pub mod claude;
pub mod codex;
pub mod detect;

/// One adapter per agent.
pub trait Adapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    /// Cheap installed check (used for error rows too).
    fn installed(&self, ctx: &Ctx) -> bool;
    /// Produce a full reading. `Err` is turned into a note by `poll_with`.
    fn poll(&self, ctx: &Ctx) -> Result<AgentUsage, String>;
}

/// Registry, in display order.
pub fn all() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(claude::ClaudeCode),
        Box::new(codex::Codex),
        Box::new(detect::gemini()),
        Box::new(detect::cursor()),
        Box::new(detect::opencode()),
        Box::new(detect::aider()),
    ]
}
