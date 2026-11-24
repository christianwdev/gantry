//! A warm, persistent Claude Code worker (`claude --print --input-format stream-json ...`).
//!
//! One actor task owns the [`ManagedChild`]; a reader task frames stdout; all writes go through
//! the cloneable [`StdinWriter`]. Shared state lives behind a short-held std mutex that is never
//! held across an `.await`.

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
use super::normalize::Normalizer;
use super::wire;
use crate::ApprovalDecision;
use crate::event::{ProviderEvent, Resolution, TurnId};

/// How long Gantry waits for Claude to answer one of its control requests.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// Shutdown ladder: protocol cancel → EOF after this → kill after `SHUTDOWN_KILL_AFTER`.
const SHUTDOWN_EOF_AFTER: Duration = Duration::from_secs(3);
const SHUTDOWN_KILL_AFTER: Duration = Duration::from_secs(5);

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
}

struct Shared {
    state: Mutex<State>,
    events: mpsc::UnboundedSender<ProviderEvent>,
    raw_log: Option<Mutex<std::fs::File>>,
    next_request: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn emit(&self, events: Vec<ProviderEvent>) {
        for event in events {
            // A dropped receiver just means nobody is listening anymore.
            let _ = self.events.send(event);
        }
    }

    fn log_frame(&self, dir: &str, frame: &Value) {
        if let Some(log) = &self.raw_log {
            let mut file = log.lock().unwrap_or_else(|p| p.into_inner());
            let _ = writeln!(file, "{}", json!({ "dir": dir, "frame": frame }));
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
    /// its event stream. The stream ends after [`ProviderEvent::SessionTerminated`].
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
            }),
            events: events_tx,
            raw_log,
            next_request: AtomicU64::new(0),
        });

        let writer = child
            .stdin_writer(256)
            .expect("stdin is piped and not yet taken");
        if let Some(stderr) = child.stderr.take() {
            let sink = Arc::clone(&shared);
            // Detached: the drain ends at EOF when the process exits.
            drop(StderrTail::spawn(stderr, 200, move |line| {
                sink.emit(vec![ProviderEvent::ProviderStderr {
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
        session.shared.emit(vec![ProviderEvent::Initialized {
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
            self.shared.emit(events);
            let frame = wire::user_message(content, &state.native_session_id);
            (turn_id, frame)
        };
        self.write(&frame).await?;
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
        self.control("interrupt", wire::interrupt()).await.map(drop)
    }

    /// Answers a tool / plan approval.
    pub async fn respond_approval(
        &self,
        interaction_id: &str,
        decision: ApprovalDecision,
    ) -> Result<()> {
        let pending = self
            .shared
            .lock()
            .normalizer
            .pending(interaction_id)
            .cloned()
            .ok_or_else(|| SessionError::UnknownInteraction(interaction_id.to_owned()))?;
        let (frame, resolution) = match decision {
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
        self.write(&frame).await?;
        let events = self
            .shared
            .lock()
            .normalizer
            .resolve(interaction_id, resolution);
        self.shared.emit(events);
        Ok(())
    }

    /// Answers an `AskUserQuestion` card. `answers` maps question id (the question text for
    /// Claude) to the chosen label(s) / free text.
    pub async fn answer_questions(
        &self,
        interaction_id: &str,
        answers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<()> {
        let pending = self
            .shared
            .lock()
            .normalizer
            .pending(interaction_id)
            .cloned()
            .ok_or_else(|| SessionError::UnknownInteraction(interaction_id.to_owned()))?;
        if pending.tool_name != "AskUserQuestion" {
            return Err(SessionError::NotAQuestion(interaction_id.to_owned()));
        }
        let mut input = pending.input;
        let answers: serde_json::Map<String, Value> = answers
            .into_iter()
            .map(|(q, a)| (q, Value::String(a)))
            .collect();
        if let Some(obj) = input.as_object_mut() {
            obj.insert("answers".into(), Value::Object(answers));
        }
        self.write(&wire::allow(interaction_id, input, &[])).await?;
        let events = self
            .shared
            .lock()
            .normalizer
            .resolve(interaction_id, Resolution::Answered);
        self.shared.emit(events);
        Ok(())
    }

    /// Switches the model live. Returns the model Claude resolved (e.g. `sonnet` → full id).
    pub async fn set_model(&self, model: &str) -> Result<String> {
        let reply = self.control("set_model", wire::set_model(model)).await?;
        let resolved = reply
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(model)
            .to_owned();
        self.shared.emit(vec![ProviderEvent::ConfigChanged {
            model: Some(resolved.clone()),
            permission_mode: None,
        }]);
        Ok(resolved)
    }

    /// Switches the permission mode live.
    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        let reply = self
            .control(
                "set_permission_mode",
                wire::set_permission_mode(mode.as_cli()),
            )
            .await?;
        self.shared.emit(vec![ProviderEvent::ConfigChanged {
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

    /// Stops the worker: interrupts a running turn, then runs the process shutdown ladder
    /// (EOF, then kill) and reaps any helpers it started.
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
        if let Err(e) = self.write(&wire::control_request(&id, body)).await {
            self.shared.lock().pending_controls.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(CONTROL_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(SessionError::Rejected { request, message }),
            Ok(Err(_)) => Err(SessionError::Exited),
            Err(_) => {
                self.shared.lock().pending_controls.remove(&id);
                Err(SessionError::Timeout { request })
            }
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
    mut child: ManagedChild,
    stdout: gantry_proc::ChildStdout,
    shared: Arc<Shared>,
    writer: StdinWriter,
    mut commands: mpsc::Receiver<Command>,
) {
    let (lines_tx, mut lines) = mpsc::channel::<Line>(256);
    tokio::spawn(async move {
        let mut reader = LineReader::new(stdout, DEFAULT_MAX_LINE);
        while let Ok(Some(line)) = reader.next_line().await {
            if lines_tx.send(line).await.is_err() {
                break;
            }
        }
    });

    let mut shutdown_reply = None;
    let mut commands_open = true;
    loop {
        tokio::select! {
            line = lines.recv() => match line {
                Some(Line::Text(text)) => handle_line(&shared, &writer, &text).await,
                Some(Line::Oversized { len }) => shared.emit(vec![ProviderEvent::Other {
                    kind: "oversized_line".into(),
                    frame: json!({ "bytes": len }),
                }]),
                None => break, // EOF: the process is gone (or closed stdout)
            },
            command = commands.recv(), if commands_open => match command {
                Some(Command::Shutdown { reply }) => {
                    let outcome = child.shutdown(SHUTDOWN_EOF_AFTER, SHUTDOWN_KILL_AFTER).await;
                    let _ = reply.send(outcome);
                    // Keep draining stdout to EOF so the final frames are still delivered.
                }
                None => {
                    // Every handle dropped: nobody can talk to this worker anymore.
                    commands_open = false;
                    shutdown_reply = Some(child.shutdown(SHUTDOWN_EOF_AFTER, SHUTDOWN_KILL_AFTER).await);
                }
            },
        }
    }

    let exit_code = match child.wait_timeout(Duration::from_secs(5)).await {
        Ok(Some(status)) => Some(status.code()),
        _ => None,
    };
    drop(shutdown_reply);
    let (events, pending) = {
        let mut state = shared.lock();
        state.exited = true;
        let events = state.normalizer.process_exited(exit_code);
        (events, std::mem::take(&mut state.pending_controls))
    };
    for (_, reply) in pending {
        let _ = reply.send(Err("the claude process exited".into()));
    }
    shared.emit(events);
}

async fn handle_line(shared: &Shared, writer: &StdinWriter, text: &str) {
    let frame: Value = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(_) => {
            shared.emit(vec![ProviderEvent::Other {
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
            let reply = shared.lock().pending_controls.remove(id);
            match reply {
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
                None => shared.emit(vec![ProviderEvent::Other {
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

    let events = shared.lock().normalizer.ingest(&frame);
    shared.emit(events);
}
