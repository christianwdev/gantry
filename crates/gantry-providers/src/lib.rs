//! # gantry-providers
//!
//! Agent adapters behind one provider-neutral event model ([`event::ProviderEvent`]):
//! Claude Code today; Codex app-server and ACP (Antigravity) next. See
//! docs/ARCHITECTURE.md §3.1.

pub mod claude;
pub mod event;

use serde_json::Value;

/// A user's answer to an approval request.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    Allow {
        /// The tool input to run with; `None` runs it exactly as requested.
        updated_input: Option<Value>,
        /// Provider "remember" suggestions the user accepted (e.g. allow for the session).
        remember: Vec<Value>,
    },
    Deny {
        /// Shown to the model as the reason.
        message: String,
        /// Also stop the whole turn.
        interrupt: bool,
    },
}
