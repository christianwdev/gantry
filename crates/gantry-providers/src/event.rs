//! The provider-neutral event stream every adapter (Claude, Codex, ACP) produces.
//!
//! These are *provider* events: the `ThreadCoordinator` folds them into persisted
//! `ThreadEvent`s (docs/ARCHITECTURE.md §3.3). They deliberately carry only what the rest of
//! Gantry needs, never a provider's raw wire frame — except [`ProviderEvent::Other`], which keeps
//! unmapped frames visible for diagnostics and forward compatibility.

use serde::Serialize;
use serde_json::Value;

/// Gantry's id for one user→agent turn (assigned by the session when the prompt is sent).
pub type TurnId = String;

/// Id of a pending human-in-the-loop interaction (an approval or a question card).
pub type InteractionId = String;

#[derive(Debug, Clone, PartialEq, Serialize)]
// Tags snake_case (`turn_completed`), fields camelCase (`turnId`) — one casing for every field,
// including the nested structs, so generated TS bindings are consistent.
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ProviderEvent {
    /// The agent process finished its handshake and accepts prompts.
    Initialized {
        /// Provider CLI version, when reported.
        version: Option<String>,
        /// The permission mode the agent reports as active.
        permission_mode: Option<String>,
    },
    /// Authoritative session metadata (Claude only sends it once the first prompt arrives).
    SessionReady {
        native_session_id: String,
        model: Option<String>,
        permission_mode: Option<String>,
        cwd: Option<String>,
        mcp_servers: Vec<McpServerStatus>,
    },
    /// Model or permission mode changed (live switch acknowledged by the agent).
    ConfigChanged {
        model: Option<String>,
        permission_mode: Option<String>,
    },

    TurnStarted {
        turn_id: TurnId,
    },
    /// Streaming assistant text. `item_id` groups deltas of one content block.
    TextDelta {
        turn_id: TurnId,
        item_id: String,
        /// Set when a subagent (nested tool) produced this output.
        parent_tool_id: Option<String>,
        delta: String,
    },
    ThinkingDelta {
        turn_id: TurnId,
        item_id: String,
        parent_tool_id: Option<String>,
        delta: String,
    },
    ToolStarted {
        turn_id: TurnId,
        tool_call_id: String,
        parent_tool_id: Option<String>,
        tool_name: String,
        input: Value,
    },
    ToolCompleted {
        turn_id: TurnId,
        tool_call_id: String,
        is_error: bool,
        output: Value,
    },

    /// The agent wants permission to run a tool (or, for plans, to leave plan mode).
    ApprovalRequested {
        turn_id: TurnId,
        interaction_id: InteractionId,
        kind: ApprovalKind,
        tool_call_id: Option<String>,
        tool_name: String,
        /// Short human-readable description the agent supplied.
        description: Option<String>,
        input: Value,
        /// Provider-specific "remember this" options (e.g. allow for the session).
        suggestions: Vec<Value>,
    },
    /// The agent asks the user structured questions (Claude's `AskUserQuestion`).
    QuestionsRequested {
        turn_id: TurnId,
        interaction_id: InteractionId,
        tool_call_id: Option<String>,
        questions: Vec<Question>,
    },
    InteractionResolved {
        turn_id: TurnId,
        interaction_id: InteractionId,
        resolution: Resolution,
    },

    TurnCompleted {
        turn_id: TurnId,
        status: TurnStatus,
        /// Final assistant text, when the provider reports one.
        result: Option<String>,
        usage: Option<Value>,
        cost_usd: Option<f64>,
        error: Option<String>,
    },

    /// Rate-limit / quota status from the provider.
    RateLimit {
        info: Value,
    },
    ProviderStderr {
        line: String,
    },
    /// The agent process exited.
    SessionTerminated {
        exit_code: Option<u32>,
        reason: String,
    },
    /// A frame this adapter doesn't map (yet). Kept for diagnostics, never dropped silently.
    Other {
        kind: String,
        frame: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Run a tool (shell command, file edit, MCP call, ...).
    Tool,
    /// Approve a plan and leave plan mode (Claude's `ExitPlanMode`).
    Plan,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    /// Stable key for the answer. Claude keys answers by question text.
    pub id: String,
    pub question: String,
    pub header: Option<String>,
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Approved,
    Denied,
    Answered,
    /// The agent withdrew the request (e.g. the turn was interrupted) or the turn ended.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    /// Ended by an interrupt.
    Interrupted,
    /// The agent reported an error.
    Failed,
    /// The agent process died mid-turn.
    Crashed,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One casing on the wire: snake_case tags, camelCase fields (nested structs included).
    #[test]
    fn serialized_events_use_camel_case_fields() {
        let e = ProviderEvent::TurnCompleted {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            result: None,
            usage: None,
            cost_usd: Some(0.1),
            error: None,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "turn_completed");
        assert_eq!(v["turnId"], "turn-1");
        assert_eq!(v["costUsd"], 0.1);
        assert!(v.get("turn_id").is_none());

        let q = ProviderEvent::QuestionsRequested {
            turn_id: "t".into(),
            interaction_id: "i".into(),
            tool_call_id: None,
            questions: vec![Question {
                id: "q".into(),
                question: "q".into(),
                header: None,
                multi_select: true,
                options: vec![],
            }],
        };
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["interactionId"], "i");
        assert_eq!(v["questions"][0]["multiSelect"], true);
    }
}
