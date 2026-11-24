//! Claude Code adapter: a persistent `claude` worker speaking `stream-json` plus the control
//! protocol (`--permission-prompt-tool stdio`). Protocol notes verified against claude 2.1.330
//! live in docs/research/01-runtime-and-protocols.md §2.1 and the fixtures under
//! `tests/fixtures/claude/`.

mod config;
mod normalize;
mod session;
pub mod wire;

pub use config::{ClaudeConfig, McpEndpoint, PermissionMode, Resume};
pub use normalize::{Normalizer, PendingInteraction};
pub use session::{ClaudeSession, SessionError};
