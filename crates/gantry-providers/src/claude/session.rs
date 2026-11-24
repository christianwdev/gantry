//! A warm, persistent Claude Code worker (`claude --print --input-format stream-json ...`).
//!
//! One actor task owns the [`ManagedChild`]; a reader task frames stdout; all writes go through
//! the cloneable [`StdinWriter`]. Shared state lives behind a short-held std mutex that is never
//! held across an `.await`. Events are emitted *while holding* that mutex, so the stream is
//! totally ordered with the state transitions that produced it.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use gantry_proc::io::{DEFAULT_MAX_LINE, Line, LineReader, StderrTail};
use gantry_proc::{ManagedChild, ManagedCommand, Shutdown, StdinWriter, Stdio};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::config::{ClaudeConfig, MCP_TOKEN_ENV, PermissionMode, RuntimeFiles};
use super::normalize::{ASK_USER_QUESTION, Normalizer};
use super::wire;
use crate::ApprovalDecision;
use crate::event::{ProviderEvent, Resolution, TurnId};

/// How long Gantry waits for Claude to answer one of its control requests.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// Shutdown: interrupt a running turn, close stdin (EOF is the stream-json stop), then kill the
/// tree if the worker has not exited after this long.
const SHUTDOWN_KILL_AFTER: Duration = Duration::from_secs(5);
/// Consecutive stdout read errors after which the stream is treated as closed.
const MAX_CONSECUTIVE_READ_ERRORS: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("could not start claude: {0}")]
    Spawn(#[from] gantry_proc::Error),
    #[error("i/o error talking to claude: {0}")]
    Io(#[from] std::io::Error),
    #[error("a turn is already running; queue the prompt until it completes")]
    TurnActive,
    #[error("no pending interaction `{0}`")]
    UnknownInteraction(String),
    #[error("interaction `{0}` is not a question")]
    NotAQuestion(String),
    #[error("interaction `{0}` is a question: answer it with answer_questions (or deny it)")]
    IsAQuestion(String),
    #[error("claude rejected `{request}`: {message}")]
    Rejected {
        request: &'static str,
        message: String,
    },
    #[error("claude did not answer `{request}` within {}s", CONTROL_TIMEOUT.as_secs())]
    Timeout { request: &'static str },
    #[error("the claude process has exited")]
    Exited,
}

pub type Result<T, E = SessionError> = std::result::Result<T, E>;

type ControlReply = std::result::Result<Value, String>;

struct State {
    normalizer: Normalizer,
    /// Gantry's control requests awaiting Claude's `control_response`.
    pending_controls: HashMap<String, oneshot::Sender<ControlReply>>,
    native_session_id: String,
    next_turn: u64,
    exited: bool,
    /// The only sender of the event stream: dropped on termination so the stream ends.
    events: Option<mpsc::UnboundedSender<ProviderEvent>>,
}

impl State {
    fn emit(&mut self, events: Vec<ProviderEvent>) {
        if let Some(tx) = &self.events {
            for event in events {
                // A dropped receiver just means nobody is listening anymore.
                let _ = tx.send(event);
            }
        }
    }
}

struct Shared {
    state: Mutex<State>,
    raw_log: Option<Mutex<std::fs::File>>,
    next_request: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn log_frame(&self, dir: &str, frame: &Value) {
        let Some(log) = &self.raw_log else {
            return;
        };
        // The initialize response carries account details: keep them out of diagnostic logs.
        let mut frame = frame.clone();
        if let Some(payload) = frame
            .pointer_mut("/response/response")
            .and_then(Value::as_object_mut)
        {
            payload.remove("account");
        }
        let mut file = log.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(file, "{}", json!({ "dir": dir, "frame": frame }));
    }
}

/// Removes a pending control request if its waiter goes away (timeout / cancelled future).
struct PendingControl<'a> {
    shared: &'a Shared,
    id: String,
}

impl Drop for PendingControl<'_> {
    fn drop(&mut self) {
        self.shared.lock().pending_controls.remove(&self.id);
    }
}

/// Clears the interrupt flag unless the interrupt request was delivered and acknowledged.
struct InterruptFlag<'a> {
    shared: &'a Shared,
    acknowledged: bool,
}

impl Drop for InterruptFlag<'_> {
    fn drop(&mut self) {
        if !self.acknowledged {
            self.shared.lock().normalizer.clear_interrupt_requested();
        }
    }
}

enum Command {
    Shutdown {
        reply: oneshot::Sender<gantry_proc::Result<Shutdown>>,
    },
}

/// Handle to a running Claude Code worker. Cheap to share behind an `Arc`.
pub struct ClaudeSession {
    writer: StdinWriter,
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
    _files: RuntimeFiles,
}

impl ClaudeSession {
    /// Starts the worker, performs the `initialize` handshake, and returns the session plus
    /// its event stream. The stream ends right after [`ProviderEvent::SessionTerminated`].
    pub async fn spawn(
        config: ClaudeConfig,
    ) -> Result<(ClaudeSession, mpsc::UnboundedReceiver<ProviderEvent>)> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let files = RuntimeFiles::write(&config, &unique)?;

        let mut cmd = ManagedCommand::new(&config.program);
        cmd.args(config.args(&files))
            .current_dir(&config.cwd)
            .env("DISABLE_AUTOUPDATER", "1")
            .stdin(Stdio::Piped)
            .stdout(Stdio::Piped)
            .stderr(Stdio::Piped);
        for (key, value) in &config.env {
            cmd.env(key, value);
        }
        if let Some(mcp) = &config.mcp {
            cmd.env(MCP_TOKEN_ENV, &mcp.bearer_token);
        }
        // Resolution + CreateProcessW do blocking filesystem work; keep it off the workers.
        let mut child = tokio::task::spawn_blocking(move || cmd.spawn())
            .await
            .map_err(|e| SessionError::Io(std::io::Error::other(e)))??;

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let raw_log = match &config.raw_log {
            Some(path) => Some(Mutex::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?,
            )),
            None => None,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                normalizer: Normalizer::new(),
                pending_controls: HashMap::new(),
                native_session_id: String::new(),
                next_turn: 0,
                exited: false,
                events: Some(events_tx),
            }),
            raw_log,
            next_request: AtomicU64::new(0),
        });

        let writer = child
            .stdin_writer(256)
            .expect("stdin is piped and not yet taken");
        if let Some(stderr) = child.stderr.take() {
            let sink = Arc::clone(&shared);
            // Detached: the drain ends at EOF when the process exits; lines after termination
            // are dropped because the event sender is gone by then.
            drop(StderrTail::spawn(stderr, 200, move |line| {
                sink.lock().emit(vec![ProviderEvent::ProviderStderr {
                    line: line.to_owned(),
                }]);
            }));
        }
        let stdout = child.stdout.take().expect("stdout is piped");
        let (commands, command_rx) = mpsc::channel(4);
        tokio::spawn(run(
            child,
            stdout,
            Arc::clone(&shared),
            writer.clone(),
            command_rx,
        ));

        let session = ClaudeSession {
            writer,
            shared,
            commands,
            _files: files,
        };
        let init = session.control("initialize", wire::initialize()).await?;
        // Only selected fields: the response also carries account details, which must not
        // travel further than this adapter.
        session.shared.lock().emit(vec![ProviderEvent::Initialized {
            version: init
                .get("claude_code_version")
                .and_then(Value::as_str)
                .map(str::to_owned),
            permission_mode: init
                .get("current_permission_mode")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }]);
        Ok((session, events_rx))
    }

    /// Claude's own session id (known after the first prompt's `system:init`).
    pub fn native_session_id(&self) -> String {
        self.shared.lock().native_session_id.clone()
    }

    /// The running turn — Gantry's (`turn-N`) or one Claude started itself (`agent-turn-N`).
    pub fn active_turn(&self) -> Option<TurnId> {
        self.shared.lock().normalizer.active_turn().cloned()
    }

    /// Sends a text prompt as a new turn. Fails with [`SessionError::TurnActive`] while a turn
    /// is running: Claude doesn't fold mid-turn messages into the running turn, so callers
    /// queue them (or interrupt first) — see ARCHITECTURE.md §3.2.
    pub async fn send_prompt(&self, text: &str) -> Result<TurnId> {
        self.send_content(vec![wire::text_block(text)]).await
    }

    /// Sends Anthropic content blocks (text, base64 `image`, ...) as a new turn.
    pub async fn send_content(&self, content: Vec<Value>) -> Result<TurnId> {
        let (turn_id, frame) = {
            let mut state = self.shared.lock();
            if state.exited {
                return Err(SessionError::Exited);
            }
            if state.normalizer.active_turn().is_some() {
                return Err(SessionError::TurnActive);
            }
            state.next_turn += 1;
            let turn_id = format!("turn-{}", state.next_turn);
            let events = state.normalizer.begin_turn(turn_id.clone());
            state.emit(events);
            let frame = wire::user_message(content, &state.native_session_id);
            (turn_id, frame)
        };
        if let Err(e) = self.write(&frame).await {
            // Claude never received it: don't leave a phantom turn open.
            let mut state = self.shared.lock();
            let events = state
                .normalizer
                .abort_turn(&turn_id, "the prompt could not be delivered to claude");
            state.emit(events);
            return Err(e);
        }
        Ok(turn_id)
    }

    /// Interrupts the running turn without killing the process. No-op when idle.
    pub async fn interrupt(&self) -> Result<()> {
        {
            let mut state = self.shared.lock();
            if state.normalizer.active_turn().is_none() {
                return Ok(());
            }
            state.normalizer.note_interrupt_requested();
        }
        // If the request fails *or this future is dropped* (e.g. a caller's timeout), the
        // interrupt never took effect: don't label the turn's eventual end as one.
        let mut flag = InterruptFlag {
            shared: &self.shared,
            acknowledged: false,
        };
        self.control("interrupt", wire::interrupt()).await?;
        flag.acknowledged = true;
        Ok(())
    }

    /// Answers a tool / plan approval. A question card can be *denied* here (dismissed);
    /// answering it goes through [`ClaudeSession::answer_questions`].
    pub async fn respond_approval(
        &self,
        interaction_id: &str,
        decision: ApprovalDecision,
    ) -> Result<()> {
        let stops_turn = matches!(
            decision,
            ApprovalDecision::Deny {
                interrupt: true,
                ..
            }
        );
        // Resolve *before* writing, under the lock: once the response is on the wire Claude can
        // finish the turn immediately, and that must not cancel the interaction first. This
        // also makes a double answer fail with UnknownInteraction instead of answering twice.
        let frame = {
            let mut state = self.shared.lock();
            let pending = state
                .normalizer
                .pending(interaction_id)
                .cloned()
                .ok_or_else(|| SessionError::UnknownInteraction(interaction_id.to_owned()))?;
            let (frame, resolution) = match decision {
                ApprovalDecision::Allow { .. } if pending.tool_name == ASK_USER_QUESTION => {
                    // An "allow" without answers would run the question tool with nothing chosen.
                    return Err(SessionError::IsAQuestion(interaction_id.to_owned()));
                }
                ApprovalDecision::Allow {
                    updated_input,
                    remember,
                } => (
                    wire::allow(
                        interaction_id,
                        updated_input.unwrap_or(pending.input),
                        &remember,
                    ),
                    Resolution::Approved,
                ),
                ApprovalDecision::Deny { message, interrupt } => (
                    wire::deny(interaction_id, &message, interrupt),
                    Resolution::Denied,
                ),
            };
            if stops_turn {
                // "Deny and stop" ends the turn with an error result: that's an interrupt the
                // user asked for, not an agent failure.
                state.normalizer.note_interrupt_requested();
            }
            let events = state.normalizer.resolve(interaction_id, resolution);
            state.emit(events);
            frame
        };
        let written = self.write(&frame).await;
        if written.is_err() && stops_turn {
            self.shared.lock().normalizer.clear_interrupt_requested();
        }
        written
    }

    /// Answers an `AskUserQuestion` card. `answers` maps question id (the question text for
    /// Claude) to the chosen label(s) / free text.
    pub async fn answer_questions(
        &self,
        interaction_id: &str,
        answers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<()> {
        let answers: serde_json::Map<String, Value> = answers
            .into_iter()
            .map(|(q, a)| (q, Value::String(a)))
            .collect();
        let frame = {
            let mut state = self.shared.lock();
            let pending = state
                .normalizer
                .pending(interaction_id)
                .cloned()
                .ok_or_else(|| SessionError::UnknownInteraction(interaction_id.to_owned()))?;
            if pending.tool_name != ASK_USER_QUESTION {
                return Err(SessionError::NotAQuestion(interaction_id.to_owned()));
            }
            let mut input = pending.input;
            let Some(obj) = input.as_object_mut() else {
                // Never send an "allow" that silently drops the user's answers.
                return Err(SessionError::Io(std::io::Error::other(
                    "AskUserQuestion input is not a JSON object",
                )));
            };
            obj.insert("answers".into(), Value::Object(answers));
            let events = state
                .normalizer
                .resolve(interaction_id, Resolution::Answered);
            state.emit(events);
            wire::allow(interaction_id, input, &[])
        };
        self.write(&frame).await
    }

    /// Switches the model live. Returns the model Claude resolved (e.g. `sonnet` → full id).
    pub async fn set_model(&self, model: &str) -> Result<String> {
        let reply = self.control("set_model", wire::set_model(model)).await?;
        let resolved = reply
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(model)
            .to_owned();
        self.shared.lock().emit(vec![ProviderEvent::ConfigChanged {
            model: Some(resolved.clone()),
            permission_mode: None,
        }]);
        Ok(resolved)
    }

    /// Switches the permission mode live (`Manual` is accepted and reported back as `default`).
    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        let reply = self
            .control(
                "set_permission_mode",
                wire::set_permission_mode(mode.as_cli()),
            )
            .await?;
        self.shared.lock().emit(vec![ProviderEvent::ConfigChanged {
            model: None,
            permission_mode: Some(
                reply
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or(mode.as_cli())
                    .to_owned(),
            ),
        }]);
        Ok(())
    }

    /// Stops the worker: interrupts a running turn, then closes stdin (the stream-json stop)
    /// and kills the tree if it hasn't exited in time; helpers it started are reaped.
    pub async fn shutdown(&self) -> Result<Shutdown> {
        if self.active_turn().is_some() {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.interrupt()).await;
        }
        let (reply, rx) = oneshot::channel();
        self.commands
            .send(Command::Shutdown { reply })
            .await
            .map_err(|_| SessionError::Exited)?;
        rx.await
            .map_err(|_| SessionError::Exited)?
            .map_err(SessionError::Spawn)
    }

    async fn control(&self, request: &'static str, body: Value) -> Result<Value> {
        let id = format!(
            "gantry-{}",
            self.shared.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.shared.lock();
            if state.exited {
                return Err(SessionError::Exited);
            }
            state.pending_controls.insert(id.clone(), tx);
        }
        // Removes the entry however this function ends (answered, timed out, dropped).
        let _pending = PendingControl {
            shared: &self.shared,
            id: id.clone(),
        };
        self.write(&wire::control_request(&id, body)).await?;
        match tokio::time::timeout(CONTROL_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(SessionError::Rejected { request, message }),
            // Sender dropped: the process exited before answering.
            Ok(Err(_)) => Err(SessionError::Exited),
            Err(_) => Err(SessionError::Timeout { request }),
        }
    }

    async fn write(&self, frame: &Value) -> Result<()> {
        self.shared.log_frame("in", frame);
        self.writer
            .send_line(&frame.to_string())
            .await
            .map_err(|_| SessionError::Exited)
    }
}

/// The actor: frames stdout, routes control traffic, feeds the normalizer, and owns the
/// process for shutdown.
async fn run(
    child: ManagedChild,
    stdout: gantry_proc::ChildStdout,
    shared: Arc<Shared>,
    writer: StdinWriter,
    mut commands: mpsc::Receiver<Command>,
) {
    let (lines_tx, mut lines) = mpsc::channel::<Line>(256);
    tokio::spawn(async move {
        let mut reader = LineReader::new(stdout, DEFAULT_MAX_LINE);
        // Tolerate transient read errors (the reader keeps a partial line across them); only
        // EOF or a run of errors ends the session.
        let mut consecutive_errors = 0;
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    consecutive_errors = 0;
                    if lines_tx.send(line).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) if consecutive_errors < MAX_CONSECUTIVE_READ_ERRORS => {
                    consecutive_errors += 1;
                }
                Err(_) => break,
            }
        }
    });

    let mut child = Some(child);
    let mut ladder_reply: Option<oneshot::Sender<gantry_proc::Result<Shutdown>>> = None;
    let (ladder_tx, mut ladder_rx) =
        mpsc::channel::<(ManagedChild, gantry_proc::Result<Shutdown>)>(1);
    let mut commands_open = true;
    loop {
        tokio::select! {
            line = lines.recv() => match line {
                Some(Line::Text(text)) => handle_line(&shared, &writer, &text).await,
                Some(Line::Oversized { len }) => shared.lock().emit(vec![ProviderEvent::Other {
                    kind: "oversized_line".into(),
                    frame: json!({ "bytes": len }),
                }]),
                None => break, // EOF: the process is gone (or closed stdout)
            },
            command = commands.recv(), if commands_open && child.is_some() => {
                match command {
                    Some(Command::Shutdown { reply }) => ladder_reply = Some(reply),
                    // Every handle dropped: nobody can talk to this worker anymore.
                    None => commands_open = false,
                }
                // The ladder runs in its own task so this loop keeps draining stdout (Claude may
                // still flush final frames; a full pipe must not stall its exit).
                let mut c = child.take().expect("guarded by child.is_some()");
                let done = ladder_tx.clone();
                tokio::spawn(async move {
                    // For a stream-json worker EOF *is* the protocol-level stop: close stdin
                    // first instead of waiting for an exit that won't come on its own.
                    c.close_stdin();
                    let outcome = c.shutdown(Duration::ZERO, SHUTDOWN_KILL_AFTER).await;
                    let _ = done.send((c, outcome)).await;
                });
            }
            Some((c, outcome)) = ladder_rx.recv() => {
                child = Some(c);
                if let Some(reply) = ladder_reply.take() {
                    let _ = reply.send(outcome);
                }
            }
        }
    }

    // A ladder may still be running: get the process back from it before the final wait.
    let mut child = match child {
        Some(c) => c,
        None => match ladder_rx.recv().await {
            Some((c, outcome)) => {
                if let Some(reply) = ladder_reply.take() {
                    let _ = reply.send(outcome);
                }
                c
            }
            None => unreachable!("this task holds a ladder sender"),
        },
    };
    let exit_code = match child.wait_timeout(Duration::from_secs(5)).await {
        Ok(Some(status)) => Some(status.code()),
        _ => None,
    };
    let mut state = shared.lock();
    state.exited = true;
    let events = state.normalizer.process_exited(exit_code);
    state.emit(events);
    // End the event stream: no sender outlives termination.
    state.events = None;
    // Dropping the senders makes waiting control requests fail with `SessionError::Exited`.
    state.pending_controls.clear();
}

async fn handle_line(shared: &Shared, writer: &StdinWriter, text: &str) {
    let frame: Value = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(_) => {
            shared.lock().emit(vec![ProviderEvent::Other {
                kind: "non_json_output".into(),
                frame: Value::String(text.to_owned()),
            }]);
            return;
        }
    };
    shared.log_frame("out", &frame);

    match frame.get("type").and_then(Value::as_str) {
        Some("control_response") => {
            let response = frame.get("response").unwrap_or(&Value::Null);
            let id = response
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut state = shared.lock();
            match state.pending_controls.remove(id) {
                Some(reply) => {
                    let result =
                        if response.get("subtype").and_then(Value::as_str) == Some("success") {
                            Ok(response.get("response").cloned().unwrap_or(Value::Null))
                        } else {
                            Err(response
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown error")
                                .to_owned())
                        };
                    let _ = reply.send(result);
                }
                None => state.emit(vec![ProviderEvent::Other {
                    kind: "unmatched_control_response".into(),
                    frame: frame.clone(),
                }]),
            }
            return;
        }
        Some("control_request") => {
            let subtype = frame
                .pointer("/request/subtype")
                .and_then(Value::as_str)
                .unwrap_or("");
            if subtype != "can_use_tool" {
                // Never leave Claude blocked on a request Gantry can't serve.
                if let Some(id) = frame.get("request_id").and_then(Value::as_str) {
                    let reply = wire::error(
                        id,
                        &format!("Gantry does not support control request `{subtype}`"),
                    );
                    shared.log_frame("in", &reply);
                    let _ = writer.send_line(&reply.to_string()).await;
                }
            }
        }
        Some("system") if frame.get("subtype").and_then(Value::as_str) == Some("init") => {
            if let Some(id) = frame.get("session_id").and_then(Value::as_str) {
                shared.lock().native_session_id = id.to_owned();
            }
        }
        _ => {}
    }

    let mut state = shared.lock();
    let events = state.normalizer.ingest(&frame);
    state.emit(events);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Diagnostic raw logs never contain the initialize response's account details.
    #[test]
    fn raw_log_strips_account_details() {
        let path =
            std::env::temp_dir().join(format!("gantry-rawlog-{}.ndjson", std::process::id()));
        let shared = Shared {
            state: Mutex::new(State {
                normalizer: Normalizer::new(),
                pending_controls: HashMap::new(),
                native_session_id: String::new(),
                next_turn: 0,
                exited: false,
                events: None,
            }),
            raw_log: Some(Mutex::new(std::fs::File::create(&path).unwrap())),
            next_request: AtomicU64::new(0),
        };
        shared.log_frame(
            "out",
            &json!({"type":"control_response","response":{"subtype":"success","request_id":"gantry-0",
                "response":{"account":{"email":"someone@example.com"},"claude_code_version":"2.1.330"}}}),
        );
        drop(shared);
        let logged = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(!logged.contains("someone@example.com"), "{logged}");
        assert!(logged.contains("2.1.330"));
    }
}
