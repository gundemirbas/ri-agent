use tokio::sync::mpsc::UnboundedSender;

use crate::{
    agent::types::{AgentEvent, AskRequest, ToolResult},
    llm::ProviderError,
};

/// Background events delivered to the interactive app loop.
#[derive(Debug)]
pub enum AppEvent {
    Agent(AgentEvent),
    ModelsReady(Result<Vec<String>, ProviderError>),
    AskUser(AskRequest),
    /// A local shell command subprocess has finished.
    ShellComplete {
        call_id: String,
        result: ToolResult,
    },
}

pub type AppEventTx = UnboundedSender<AppEvent>;

/// Extension trait for fire-and-forget channel sends.
///
/// `send_ignore` is equivalent to `let _ = self.send(val)` — it discards
/// the error that occurs when all receivers have been dropped.  Use this
/// instead of the noisy `let _ =` pattern at every call site.
pub trait SendIgnore<T> {
    fn send_ignore(&self, val: T);
}

impl<T> SendIgnore<T> for UnboundedSender<T> {
    fn send_ignore(&self, val: T) {
        let _ = self.send(val);
    }
}
