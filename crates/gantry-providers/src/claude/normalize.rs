//! Pure, deterministic mapping from Claude Code `stream-json` frames to [`ProviderEvent`]s.
//!
//! Kept free of I/O so it can be unit-tested and replay-tested against recorded fixtures.
//! Control *responses* (answers to requests Gantry sent) are handled by the session, not here.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::event::{
    ApprovalKind, InteractionId, McpServerStatus, ProviderEvent, Question, QuestionOption,
    Resolution, TurnId, TurnStatus,
};

/// Tool names Claude routes through `can_use_tool` that are really user interactions.
pub(crate) const ASK_USER_QUESTION: &str = "AskUserQuestion";
const EXIT_PLAN_MODE: &str = "ExitPlanMode";

/// A pending `can_use_tool` request, remembered so a response can be built from it.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingInteraction {
    pub turn_id: TurnId,
    pub tool_name: String,
    /// The tool input as Claude sent it; an "allow" must echo it back as `updatedInput`.
    pub input: Value,
}

#[derive(Debug, Default)]
pub struct Normalizer {
    turn: Option<TurnId>,
    interrupt_requested: bool,
    /// Current streamed message id per subagent scope (`parent_tool_use_id`).
    message_ids: HashMap<Option<String>, String>,
    /// Streamed content blocks: (scope, index) -> (item id, block type).
    blocks: HashMap<(Option<String>, u64), (String, String)>,
    /// Tool calls already announced (assistant frames can repeat a block).
    started_tools: HashSet<String>,
    /// Open interactions keyed by Claude's control `request_id`, in arrival order.
    open: Vec<(InteractionId, PendingInteraction)>,
    /// Counter for turns Claude starts on its own (e.g. after a background task finishes).
    agent_turns: u64,
}

impl Normalizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The turn currently running, if any.
    pub fn active_turn(&self) -> Option<&TurnId> {
        self.turn.as_ref()
    }

    pub fn pending(&self, id: &str) -> Option<&PendingInteraction> {
        self.open.iter().find(|(i, _)| i == id).map(|(_, p)| p)
    }

    /// Gantry sent a prompt: a new turn starts.
    pub fn begin_turn(&mut self, turn_id: TurnId) -> Vec<ProviderEvent> {
        self.turn = Some(turn_id.clone());
        self.interrupt_requested = false;
        vec![ProviderEvent::TurnStarted { turn_id }]
    }

    /// Gantry asked to interrupt: an *error* `result` for this turn reads as `Interrupted`
    /// (a success that raced the interrupt still counts as `Completed`).
    pub fn note_interrupt_requested(&mut self) {
        if self.turn.is_some() {
            self.interrupt_requested = true;
        }
    }

    /// The interrupt request failed (rejected / timed out): it never took effect.
    pub fn clear_interrupt_requested(&mut self) {
        self.interrupt_requested = false;
    }

    /// The prompt for `turn_id` could not be delivered: end that turn as `Failed`.
    pub fn abort_turn(&mut self, turn_id: &str, error: &str) -> Vec<ProviderEvent> {
        if self.turn.as_deref() != Some(turn_id) {
            return Vec::new();
        }
        self.turn = None;
        self.interrupt_requested = false;
        vec![ProviderEvent::TurnCompleted {
            turn_id: turn_id.to_owned(),
            status: TurnStatus::Failed,
            result: None,
            usage: None,
            cost_usd: None,
            error: Some(error.to_owned()),
        }]
    }

    /// Claude produced turn activity with no Gantry prompt outstanding (it can start a turn
    /// itself, e.g. after a background task completes). Open an agent-initiated turn so the
    /// work is attributed — and so a user prompt can't be sent into it (`send_prompt` sees
    /// an active turn and the caller queues).
    fn ensure_turn(&mut self, out: &mut Vec<ProviderEvent>) {
        if self.turn.is_none() {
            self.agent_turns += 1;
            let turn_id = format!("agent-turn-{}", self.agent_turns);
            self.turn = Some(turn_id.clone());
            self.interrupt_requested = false;
            out.push(ProviderEvent::TurnStarted { turn_id });
        }
    }

    /// Gantry answered an interaction; returns the resolution event (once).
    pub fn resolve(&mut self, id: &str, resolution: Resolution) -> Vec<ProviderEvent> {
        match self.open.iter().position(|(i, _)| i == id) {
            Some(pos) => {
                let (interaction_id, pending) = self.open.remove(pos);
                vec![ProviderEvent::InteractionResolved {
                    turn_id: pending.turn_id,
                    interaction_id,
                    resolution,
                }]
            }
            None => Vec::new(),
        }
    }

    /// The process exited. A turn still running ends as `Crashed` (or `Interrupted` if Gantry
    /// had asked for that), and open interactions are cancelled.
    pub fn process_exited(&mut self, exit_code: Option<u32>) -> Vec<ProviderEvent> {
        let mut out = self.cancel_open_interactions();
        self.clear_turn_scratch();
        if let Some(turn_id) = self.turn.take() {
            let status = if self.interrupt_requested {
                TurnStatus::Interrupted
            } else {
                TurnStatus::Crashed
            };
            out.push(ProviderEvent::TurnCompleted {
                turn_id,
                status,
                result: None,
                usage: None,
                cost_usd: None,
                error: Some(match exit_code {
                    Some(code) => format!("agent process exited (code {code}) mid-turn"),
                    None => "agent process exited mid-turn".into(),
                }),
            });
        }
        out.push(ProviderEvent::SessionTerminated {
            exit_code,
            reason: "agent process exited".into(),
        });
        out
    }

    /// Maps one parsed stdout frame.
    pub fn ingest(&mut self, frame: &Value) -> Vec<ProviderEvent> {
        let kind = str_field(frame, "type").unwrap_or("");
        // Frames that only occur inside a turn: attribute them even if Claude started it.
        let mut out = Vec::new();
        if matches!(
            kind,
            "stream_event" | "assistant" | "user" | "control_request" | "result"
        ) {
            self.ensure_turn(&mut out);
        }
        out.extend(self.ingest_frame(kind, frame));
        out
    }

    fn ingest_frame(&mut self, kind: &str, frame: &Value) -> Vec<ProviderEvent> {
        match kind {
            "stream_event" => self.stream_event(frame),
            "assistant" => self.assistant(frame),
            "user" => self.user(frame),
            "control_request" => self.control_request(frame),
            "control_cancel_request" => match str_field(frame, "request_id") {
                Some(id) => self.resolve(id, Resolution::Cancelled),
                None => vec![other("control_cancel_request", frame)],
            },
            "system" => self.system(frame),
            "rate_limit_event" => vec![ProviderEvent::RateLimit {
                info: frame.get("rate_limit_info").cloned().unwrap_or(Value::Null),
            }],
            "result" => self.result(frame),
            // Answers to Gantry's own requests are consumed by the session before this point.
            "control_response" => Vec::new(),
            _ => vec![other(if kind.is_empty() { "unknown" } else { kind }, frame)],
        }
    }

    fn turn_id(&self) -> TurnId {
        self.turn.clone().unwrap_or_default()
    }

    fn stream_event(&mut self, frame: &Value) -> Vec<ProviderEvent> {
        let scope = str_field(frame, "parent_tool_use_id").map(str::to_owned);
        let Some(event) = frame.get("event") else {
            return Vec::new();
        };
        match str_field(event, "type").unwrap_or("") {
            "message_start" => {
                if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
                    self.message_ids.insert(scope, id.to_owned());
                }
                Vec::new()
            }
            "content_block_start" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block_type = event
                    .pointer("/content_block/type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let message = self.message_ids.get(&scope).cloned().unwrap_or_default();
                let item_id = format!("{message}:{index}");
                self.blocks.insert((scope, index), (item_id, block_type));
                Vec::new()
            }
            "content_block_delta" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let item_id = self
                    .blocks
                    .get(&(scope.clone(), index))
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| {
                        let message = self.message_ids.get(&scope).cloned().unwrap_or_default();
                        format!("{message}:{index}")
                    });
                let Some(delta) = event.get("delta") else {
                    return Vec::new();
                };
                match str_field(delta, "type").unwrap_or("") {
                    "text_delta" => vec![ProviderEvent::TextDelta {
                        turn_id: self.turn_id(),
                        item_id,
                        parent_tool_id: scope,
                        delta: str_field(delta, "text").unwrap_or("").to_owned(),
                    }],
                    "thinking_delta" => vec![ProviderEvent::ThinkingDelta {
                        turn_id: self.turn_id(),
                        item_id,
                        parent_tool_id: scope,
                        delta: str_field(delta, "thinking").unwrap_or("").to_owned(),
                    }],
                    // input_json_delta / signature_delta: the complete tool input arrives in the
                    // following `assistant` frame, which is what ToolStarted uses.
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn assistant(&mut self, frame: &Value) -> Vec<ProviderEvent> {
        let scope = str_field(frame, "parent_tool_use_id").map(str::to_owned);
        let mut out = Vec::new();
        for block in content_blocks(frame) {
            if str_field(block, "type") != Some("tool_use") {
                continue; // text/thinking were already streamed as deltas
            }
            let Some(id) = str_field(block, "id") else {
                continue;
            };
            if !self.started_tools.insert(id.to_owned()) {
                continue;
            }
            out.push(ProviderEvent::ToolStarted {
                turn_id: self.turn_id(),
                tool_call_id: id.to_owned(),
                parent_tool_id: scope.clone(),
                tool_name: str_field(block, "name").unwrap_or("").to_owned(),
                input: block.get("input").cloned().unwrap_or(Value::Null),
            });
        }
        out
    }

    fn user(&mut self, frame: &Value) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        for block in content_blocks(frame) {
            if str_field(block, "type") != Some("tool_result") {
                continue;
            }
            let Some(id) = str_field(block, "tool_use_id") else {
                continue;
            };
            out.push(ProviderEvent::ToolCompleted {
                turn_id: self.turn_id(),
                tool_call_id: id.to_owned(),
                is_error: block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                output: block.get("content").cloned().unwrap_or(Value::Null),
            });
        }
        out
    }

    fn control_request(&mut self, frame: &Value) -> Vec<ProviderEvent> {
        let request = frame.get("request").unwrap_or(&Value::Null);
        let (Some(request_id), Some("can_use_tool")) = (
            str_field(frame, "request_id"),
            str_field(request, "subtype"),
        ) else {
            // Unsupported request kinds are answered with an error by the session.
            return vec![other("control_request", frame)];
        };
        let tool_name = str_field(request, "tool_name").unwrap_or("").to_owned();
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        let tool_call_id = str_field(request, "tool_use_id").map(str::to_owned);
        let interaction_id = request_id.to_owned();
        self.open.push((
            interaction_id.clone(),
            PendingInteraction {
                turn_id: self.turn_id(),
                tool_name: tool_name.clone(),
                input: input.clone(),
            },
        ));
        if tool_name == ASK_USER_QUESTION {
            return vec![ProviderEvent::QuestionsRequested {
                turn_id: self.turn_id(),
                interaction_id,
                tool_call_id,
                questions: parse_questions(&input),
            }];
        }
        vec![ProviderEvent::ApprovalRequested {
            turn_id: self.turn_id(),
            interaction_id,
            kind: if tool_name == EXIT_PLAN_MODE {
                ApprovalKind::Plan
            } else {
                ApprovalKind::Tool
            },
            tool_call_id,
            tool_name,
            description: str_field(request, "description")
                .or_else(|| str_field(request, "decision_reason"))
                .map(str::to_owned),
            input,
            suggestions: request
                .get("permission_suggestions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }]
    }

    fn system(&mut self, frame: &Value) -> Vec<ProviderEvent> {
        match str_field(frame, "subtype").unwrap_or("") {
            "init" => vec![ProviderEvent::SessionReady {
                native_session_id: str_field(frame, "session_id").unwrap_or("").to_owned(),
                model: str_field(frame, "model").map(str::to_owned),
                permission_mode: str_field(frame, "permissionMode").map(str::to_owned),
                cwd: str_field(frame, "cwd").map(str::to_owned),
                mcp_servers: frame
                    .get("mcp_servers")
                    .and_then(Value::as_array)
                    .map(|servers| {
                        servers
                            .iter()
                            .map(|s| McpServerStatus {
                                name: str_field(s, "name").unwrap_or("").to_owned(),
                                status: str_field(s, "status").unwrap_or("").to_owned(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }],
            // High-frequency progress noise with no consumer yet.
            "status" | "thinking_tokens" => Vec::new(),
            subtype => vec![other(&format!("system:{subtype}"), frame)],
        }
    }

    fn result(&mut self, frame: &Value) -> Vec<ProviderEvent> {
        let mut out = self.cancel_open_interactions();
        let subtype = str_field(frame, "subtype").unwrap_or("");
        let is_error = frame
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(subtype != "success");
        // An interrupt ends the turn with an error result; a *success* that raced a late
        // interrupt request is still a completed turn.
        let status = match (is_error, self.interrupt_requested) {
            (true, true) => TurnStatus::Interrupted,
            (true, false) => TurnStatus::Failed,
            (false, _) => TurnStatus::Completed,
        };
        let result = str_field(frame, "result")
            .filter(|r| !r.is_empty())
            .map(str::to_owned);
        out.push(ProviderEvent::TurnCompleted {
            turn_id: self.turn.take().unwrap_or_default(),
            status,
            error: (status == TurnStatus::Failed).then(|| {
                frame
                    .get("errors")
                    .and_then(Value::as_array)
                    .and_then(|e| e.first())
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| result.clone())
                    .unwrap_or_else(|| subtype.to_owned())
            }),
            result,
            usage: frame.get("usage").cloned(),
            cost_usd: frame.get("total_cost_usd").and_then(Value::as_f64),
        });
        self.interrupt_requested = false;
        self.clear_turn_scratch();
        out
    }

    /// Per-turn bookkeeping; cleared so a long-lived warm worker doesn't grow without bound.
    fn clear_turn_scratch(&mut self) {
        self.blocks.clear();
        self.message_ids.clear();
        self.started_tools.clear();
    }

    /// Ends every open interaction (turn over, interrupted, or process gone).
    fn cancel_open_interactions(&mut self) -> Vec<ProviderEvent> {
        std::mem::take(&mut self.open)
            .into_iter()
            .map(
                |(interaction_id, pending)| ProviderEvent::InteractionResolved {
                    turn_id: pending.turn_id,
                    interaction_id,
                    resolution: Resolution::Cancelled,
                },
            )
            .collect()
    }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn content_blocks(frame: &Value) -> impl Iterator<Item = &Value> {
    frame
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn other(kind: &str, frame: &Value) -> ProviderEvent {
    ProviderEvent::Other {
        kind: kind.to_owned(),
        frame: frame.clone(),
    }
}

fn parse_questions(input: &Value) -> Vec<Question> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|q| {
            let question = str_field(q, "question").unwrap_or("").to_owned();
            Question {
                // Claude keys answers by the question text.
                id: question.clone(),
                question,
                header: str_field(q, "header").map(str::to_owned),
                multi_select: q
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                options: q
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|o| QuestionOption {
                        label: str_field(o, "label").unwrap_or("").to_owned(),
                        description: str_field(o, "description").map(str::to_owned),
                    })
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn completed(events: &[ProviderEvent]) -> Option<(String, TurnStatus)> {
        events.iter().find_map(|e| match e {
            ProviderEvent::TurnCompleted {
                turn_id, status, ..
            } => Some((turn_id.clone(), *status)),
            _ => None,
        })
    }

    fn tool_use(id: &str) -> Value {
        json!({"type":"assistant","message":{"id":"m1","content":[
            {"type":"tool_use","id":id,"name":"Bash","input":{"command":"x"}}]}})
    }

    #[test]
    fn agent_initiated_activity_opens_and_closes_an_agent_turn() {
        let mut n = Normalizer::new();
        let events = n.ingest(&tool_use("t1"));
        assert!(
            matches!(&events[0], ProviderEvent::TurnStarted { turn_id } if turn_id == "agent-turn-1")
        );
        assert_eq!(n.active_turn().map(String::as_str), Some("agent-turn-1"));
        let events = n.ingest(&json!({"type":"result","subtype":"success","is_error":false}));
        assert_eq!(
            completed(&events),
            Some(("agent-turn-1".into(), TurnStatus::Completed))
        );
        assert_eq!(n.active_turn(), None);
    }

    #[test]
    fn interrupt_flag_only_labels_error_results() {
        let mut n = Normalizer::new();
        n.begin_turn("turn-1".into());
        n.note_interrupt_requested();
        // A success that raced the interrupt is still a completed turn.
        let events = n.ingest(&json!({"type":"result","subtype":"success","is_error":false}));
        assert_eq!(completed(&events).unwrap().1, TurnStatus::Completed);

        n.begin_turn("turn-2".into());
        n.note_interrupt_requested();
        let events =
            n.ingest(&json!({"type":"result","subtype":"error_during_execution","is_error":true}));
        assert_eq!(completed(&events).unwrap().1, TurnStatus::Interrupted);

        // A failed interrupt request is forgotten: the error is a real failure.
        n.begin_turn("turn-3".into());
        n.note_interrupt_requested();
        n.clear_interrupt_requested();
        let events =
            n.ingest(&json!({"type":"result","subtype":"error_during_execution","is_error":true}));
        assert_eq!(completed(&events).unwrap().1, TurnStatus::Failed);
    }

    #[test]
    fn undelivered_prompt_aborts_its_turn() {
        let mut n = Normalizer::new();
        n.begin_turn("turn-1".into());
        let events = n.abort_turn("turn-1", "not delivered");
        assert_eq!(
            completed(&events),
            Some(("turn-1".into(), TurnStatus::Failed))
        );
        assert_eq!(n.active_turn(), None);
        assert!(n.abort_turn("turn-1", "again").is_empty());
    }

    #[test]
    fn per_turn_tool_ids_are_forgotten_after_the_turn() {
        let mut n = Normalizer::new();
        n.begin_turn("turn-1".into());
        assert_eq!(n.ingest(&tool_use("t1")).len(), 1);
        assert!(
            n.ingest(&tool_use("t1")).is_empty(),
            "repeated block is deduplicated"
        );
        n.ingest(&json!({"type":"result","subtype":"success","is_error":false}));
        assert!(n.started_tools.is_empty());
    }
}
