//! The X2 pass criteria (docs/ROADMAP.md) as scenarios that run live or as replays.

use gantry_providers::ApprovalDecision;
use gantry_providers::claude::{ClaudeSession, PermissionMode, Resume, SessionError};
use gantry_providers::event::{ApprovalKind, ProviderEvent, Resolution, TurnStatus};

use super::{Events, Harness, Mode};

async fn shutdown(session: ClaudeSession, events: &mut Events) {
    let t = std::time::Instant::now();
    let outcome = session
        .shutdown()
        .await
        .expect("shutdown")
        .expect("the worker was still running");
    assert!(
        outcome.tree_empty,
        "claude's process tree must be gone after shutdown"
    );
    assert!(
        !outcome.forced,
        "an idle worker exits on stdin EOF without being killed"
    );
    // EOF is the stream-json stop: no waiting for an exit that won't come on its own.
    assert!(
        t.elapsed() < std::time::Duration::from_secs(3),
        "shutdown took {:?}",
        t.elapsed()
    );
    // Idempotent: a second shutdown reports "already stopped" instead of an error.
    assert!(matches!(session.shutdown().await, Ok(None)));
    events.terminated().await;
}

/// Replay-only: dropping every session handle mid-turn (how a thread is closed) stops the
/// worker; the cut-short turn is an interrupt the user asked for, not a crash.
pub async fn drop_busy_worker(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session.send_prompt("work").await.unwrap();
    events
        .until("ToolStarted", |e| {
            matches!(e, ProviderEvent::ToolStarted { .. })
        })
        .await;
    drop(session);
    assert_eq!(events.turn_completed().await.0, TurnStatus::Interrupted);
    events.terminated().await;
}

/// Replay-only: a worker that dies during the `initialize` handshake (not logged in, bad flag)
/// fails `spawn` with its stderr attached, not a bare "exited".
pub async fn startup_failure(h: &Harness) {
    let cwd = h.workdir("a");
    let err = match ClaudeSession::spawn(h.config("main", &cwd)).await {
        Ok(_) => panic!("the fixture dies before answering initialize"),
        Err(e) => e,
    };
    match &err {
        SessionError::StartupFailed { cause, stderr } => {
            assert!(matches!(**cause, SessionError::Exited), "{cause:?}");
            assert!(
                stderr.iter().any(|l| l.contains("/login")),
                "stderr must carry the reason: {stderr:?}"
            );
        }
        other => panic!("expected StartupFailed, got {other:?}"),
    }
    assert!(err.to_string().contains("/login"), "{err}");
}

/// Five consecutive turns on one warm worker; the native session id stays stable.
pub async fn multi_turn(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    events
        .until("Initialized", |e| {
            matches!(e, ProviderEvent::Initialized { .. })
        })
        .await;
    for i in 1..=5 {
        session
            .send_prompt(&format!("Reply with only the number {i}, nothing else."))
            .await
            .unwrap();
        let (status, result) = events.turn_completed().await;
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            result
                .as_deref()
                .is_some_and(|r| r.contains(&i.to_string())),
            "turn {i}: {result:?}"
        );
    }
    assert!(!session.native_session_id().is_empty());
    assert!(events.count(|e| matches!(e, ProviderEvent::SessionReady { .. })) >= 1);
    // Prompts can't overlap a running turn: callers queue instead (ARCHITECTURE.md §3.2).
    session.send_prompt("Reply with only OK.").await.unwrap();
    assert!(matches!(
        session.send_prompt("second").await,
        Err(gantry_providers::claude::SessionError::TurnActive)
    ));
    assert_eq!(events.turn_completed().await.0, TurnStatus::Completed);
    shutdown(session, &mut events).await;
    h.save_fixtures(&["main"]);
}

/// Tool approvals: allow a file write, deny a shell command.
pub async fn approve_and_deny(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;

    session
        .send_prompt("Create a file named a.txt containing the word hello, using your Write tool. Then reply DONE.")
        .await
        .unwrap();
    let id = approval(&mut events, "Write").await;
    session
        .respond_approval(
            &id,
            ApprovalDecision::Allow {
                updated_input: None,
                remember: vec![],
            },
        )
        .await
        .unwrap();
    let (status, _) = events.turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(resolved(&events, &id, Resolution::Approved));
    if h.mode == Mode::Live {
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap().trim(),
            "hello"
        );
    }

    session
        .send_prompt(
            "Run the shell command `ping -n 2 127.0.0.1` using your Bash tool and summarize it.",
        )
        .await
        .unwrap();
    let id = approval(&mut events, "Bash").await;
    session
        .respond_approval(
            &id,
            ApprovalDecision::Deny {
                message: "Denied by Gantry test".into(),
                interrupt: false,
            },
        )
        .await
        .unwrap();
    let (status, _) = events.turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(resolved(&events, &id, Resolution::Denied));
    assert!(
        events.count(|e| matches!(e, ProviderEvent::ToolCompleted { is_error: true, .. })) >= 1,
        "the denied call comes back as an error tool result"
    );
    shutdown(session, &mut events).await;
    h.save_fixtures(&["main"]);
}

/// AskUserQuestion arrives as a structured question card and is answered by question id.
pub async fn ask_question(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session
        .send_prompt("Use the AskUserQuestion tool to ask me which color I prefer, red or blue. Then reply with only my answer.")
        .await
        .unwrap();
    let (id, question) = match events
        .until("QuestionsRequested", |e| {
            matches!(e, ProviderEvent::QuestionsRequested { .. })
        })
        .await
    {
        ProviderEvent::QuestionsRequested {
            interaction_id,
            questions,
            ..
        } => (
            interaction_id,
            questions.into_iter().next().expect("a question"),
        ),
        _ => unreachable!(),
    };
    let blue = question
        .options
        .iter()
        .find(|o| o.label.to_lowercase().contains("blue"))
        .map(|o| o.label.clone())
        .unwrap_or_else(|| "Blue".into());
    // ...nor answered with nothing.
    assert!(matches!(
        session
            .answer_questions(&id, std::iter::empty::<(String, String)>())
            .await,
        Err(SessionError::IncompleteAnswers(_))
    ));
    // A question card can't be "approved" without answers.
    assert!(matches!(
        session
            .respond_approval(
                &id,
                ApprovalDecision::Allow {
                    updated_input: None,
                    remember: vec![]
                }
            )
            .await,
        Err(SessionError::IsAQuestion(_))
    ));
    session
        .answer_questions(&id, [(question.id.clone(), blue)])
        .await
        .unwrap();
    let (status, result) = events.turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(resolved(&events, &id, Resolution::Answered));
    assert!(result.unwrap_or_default().to_lowercase().contains("blue"));
    shutdown(session, &mut events).await;
    h.save_fixtures(&["main"]);
}

/// Interrupt a running tool, then keep using the same warm worker.
pub async fn interrupt_running_tool(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session
        .send_prompt(
            "Run the shell command `ping -n 30 127.0.0.1` using your Bash tool, then summarize it.",
        )
        .await
        .unwrap();
    let id = approval(&mut events, "Bash").await;
    session
        .respond_approval(
            &id,
            ApprovalDecision::Allow {
                updated_input: None,
                remember: vec![],
            },
        )
        .await
        .unwrap();
    if h.mode == Mode::Live {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await; // let the ping run
    }
    session.interrupt().await.unwrap();
    assert_eq!(events.turn_completed().await.0, TurnStatus::Interrupted);

    session
        .send_prompt("Reply with only the word OK.")
        .await
        .unwrap();
    assert_eq!(events.turn_completed().await.0, TurnStatus::Completed);
    shutdown(session, &mut events).await;
    h.save_fixtures(&["main"]);
}

/// Interrupting while an approval is pending: Claude withdraws it (control_cancel_request).
pub async fn interrupt_pending_approval(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session
        .send_prompt(
            "Run the shell command `ping -n 3 127.0.0.1` using your Bash tool and summarize it.",
        )
        .await
        .unwrap();
    let id = approval(&mut events, "Bash").await;
    session.interrupt().await.unwrap();
    assert_eq!(events.turn_completed().await.0, TurnStatus::Interrupted);
    assert!(resolved(&events, &id, Resolution::Cancelled));
    shutdown(session, &mut events).await;
    h.save_fixtures(&["main"]);
}

/// Plan mode: ExitPlanMode is a Plan approval; other tools still ask.
pub async fn plan_mode(h: &Harness) {
    let cwd = h.workdir("a");
    let mut config = h.config("main", &cwd);
    config.permission_mode = PermissionMode::Plan;
    let (session, rx) = ClaudeSession::spawn(config).await.unwrap();
    let mut events = h.events(rx);
    session
        .send_prompt("Make a one-step plan to create a file named plan.txt containing hi. When the plan is ready, call ExitPlanMode to present it, then carry it out.")
        .await
        .unwrap();
    let mut saw_plan = false;
    loop {
        let event = events
            .until("an approval or the end of the turn", |e| {
                matches!(
                    e,
                    ProviderEvent::ApprovalRequested { .. } | ProviderEvent::TurnCompleted { .. }
                )
            })
            .await;
        match event {
            ProviderEvent::ApprovalRequested {
                interaction_id,
                kind,
                ..
            } => {
                saw_plan |= kind == ApprovalKind::Plan;
                session
                    .respond_approval(
                        &interaction_id,
                        ApprovalDecision::Allow {
                            updated_input: None,
                            remember: vec![],
                        },
                    )
                    .await
                    .unwrap();
            }
            ProviderEvent::TurnCompleted { status, .. } => {
                assert_eq!(status, TurnStatus::Completed);
                break;
            }
            _ => unreachable!(),
        }
    }
    assert!(saw_plan, "ExitPlanMode must surface as a Plan approval");
    shutdown(session, &mut events).await;
    h.save_fixtures(&["main"]);
}

/// Live model / permission-mode switches are acknowledged and reported.
pub async fn live_config(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session
        .set_permission_mode(PermissionMode::AcceptEdits)
        .await
        .unwrap();
    let model = session.set_model("sonnet").await.unwrap();
    assert!(model.contains("sonnet"), "{model}");
    session
        .send_prompt("Reply with only the word READY.")
        .await
        .unwrap();
    let ready = events
        .until("SessionReady", |e| {
            matches!(e, ProviderEvent::SessionReady { .. })
        })
        .await;
    match ready {
        ProviderEvent::SessionReady {
            permission_mode,
            model: m,
            ..
        } => {
            assert_eq!(permission_mode.as_deref(), Some("acceptEdits"));
            assert!(m.unwrap_or_default().contains("sonnet"));
        }
        _ => unreachable!(),
    }
    assert_eq!(events.turn_completed().await.0, TurnStatus::Completed);
    // Back to Manual mid-session: the CLI accepts `manual` and reports it as `default`.
    session
        .set_permission_mode(PermissionMode::Manual)
        .await
        .unwrap();
    events
        .until("ConfigChanged(default)", |e| {
            matches!(e, ProviderEvent::ConfigChanged { permission_mode: Some(m), .. } if m == "default")
        })
        .await;
    assert_eq!(
        events.count(|e| matches!(e, ProviderEvent::ConfigChanged { .. })),
        3
    );
    shutdown(session, &mut events).await;
    h.save_fixtures(&["main"]);
}

/// Resume a session from a *different* working directory and fork it (ARCHITECTURE.md:
/// cross-workspace fork). Records whether Claude finds the session across cwds.
pub async fn resume_fork_cross_cwd(h: &Harness) {
    let (first, native_id) = {
        let cwd = h.workdir("a");
        let (session, mut events) = h.spawn("first", &cwd).await;
        session
            .send_prompt("Remember the code word ZEBRA-42. Reply with only OK.")
            .await
            .unwrap();
        assert_eq!(events.turn_completed().await.0, TurnStatus::Completed);
        let id = session.native_session_id();
        shutdown(session, &mut events).await;
        (events, id)
    };
    drop(first);
    assert!(!native_id.is_empty());

    let cwd = h.workdir("b");
    let mut config = h.config("fork", &cwd);
    config.resume = Some(Resume {
        session_id: native_id.clone(),
        fork: true,
    });
    let (session, rx) = ClaudeSession::spawn(config).await.unwrap();
    let mut events = h.events(rx);
    session
        .send_prompt(
            "What was the code word I asked you to remember? Reply with only the code word.",
        )
        .await
        .unwrap();
    let (status, result) = events.turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(
        result.as_deref().is_some_and(|r| r.contains("ZEBRA-42")),
        "forked session from another cwd must keep the history: {result:?}"
    );
    let forked = session.native_session_id();
    assert_ne!(
        forked, native_id,
        "--fork-session must produce a new session id"
    );
    shutdown(session, &mut events).await;
    h.save_fixtures(&["first", "fork"]);
}

/// Replay-only (hand-written fixture): a control request Gantry doesn't support (hooks, SDK MCP)
/// gets an explicit error reply — fake-claude fails the run unless a `control_response`
/// arrives — and is surfaced as `Other` instead of leaving Claude blocked.
pub async fn unsupported_control_request(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session.send_prompt("hi").await.unwrap();
    assert_eq!(events.turn_completed().await.0, TurnStatus::Completed);
    assert_eq!(
        events
            .count(|e| matches!(e, ProviderEvent::Other { kind, .. } if kind == "control_request")),
        1
    );
    shutdown(session, &mut events).await;
}

/// Replay-only (hand-written fixture): the process dies while a control request is pending.
/// The caller must see `Exited` (not a "rejected" request), and the session terminates.
pub async fn exit_during_control(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    let err = session.set_model("sonnet").await.unwrap_err();
    assert!(matches!(err, SessionError::Exited), "{err:?}");
    events.terminated().await;
    assert!(matches!(
        session.send_prompt("hi").await,
        Err(SessionError::Exited)
    ));
}

/// A question card can be dismissed (denied) without interrupting the whole turn.
pub async fn dismiss_question(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session
        .send_prompt("Use the AskUserQuestion tool to ask me which color I prefer, red or blue. If I don't answer, reply with only the word SKIPPED.")
        .await
        .unwrap();
    let id = match events
        .until("QuestionsRequested", |e| {
            matches!(e, ProviderEvent::QuestionsRequested { .. })
        })
        .await
    {
        ProviderEvent::QuestionsRequested { interaction_id, .. } => interaction_id,
        _ => unreachable!(),
    };
    session
        .respond_approval(
            &id,
            ApprovalDecision::Deny {
                message: "The user dismissed the question.".into(),
                interrupt: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.turn_completed().await.0, TurnStatus::Completed);
    assert!(resolved(&events, &id, Resolution::Denied));
    shutdown(session, &mut events).await;
    h.save_fixtures(&["main"]);
}

/// Replay-only: a control request whose waiter gave up (timeout / dropped future) but which
/// Claude applies late is still reported as ConfigChanged.
pub async fn late_control_reply(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    let gave_up = tokio::time::timeout(
        std::time::Duration::from_millis(150),
        session.set_model("sonnet"),
    )
    .await;
    assert!(gave_up.is_err(), "the fixture delays the reply");
    events
        .until("late ConfigChanged", |e| {
            matches!(e, ProviderEvent::ConfigChanged { model: Some(m), .. } if m.contains("sonnet"))
        })
        .await;
    shutdown(session, &mut events).await;
}

/// Replay-only: shutting down a busy worker reports the turn as Interrupted (not Crashed),
/// delivers the process's last stderr line before termination, and a second shutdown is a
/// no-op instead of an error.
pub async fn shutdown_busy_worker(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session.send_prompt("work").await.unwrap();
    events
        .until("ToolStarted", |e| {
            matches!(e, ProviderEvent::ToolStarted { .. })
        })
        .await;
    let _ = session.shutdown().await.expect("shutdown");
    assert_eq!(events.turn_completed().await.0, TurnStatus::Interrupted);
    events.terminated().await;
    assert!(
        events.seen.iter().any(|e| matches!(
            e,
            ProviderEvent::ProviderStderr { line } if line.contains("worker crashed while stopping")
        )),
        "the last stderr line must arrive before SessionTerminated"
    );
    assert!(matches!(session.shutdown().await, Ok(None)));
}

/// Replay-only: Claude dies while a descendant it started still holds its stdout open. The
/// session must still end promptly (process watched directly, not just stdout EOF).
pub async fn crash_with_inherited_stdout(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session.send_prompt("work").await.unwrap();
    let t = std::time::Instant::now();
    assert_eq!(events.turn_completed().await.0, TurnStatus::Crashed);
    events.terminated().await;
    assert!(
        t.elapsed() < std::time::Duration::from_secs(8),
        "session must end despite the held pipe ({:?})",
        t.elapsed()
    );
    assert!(matches!(session.shutdown().await, Ok(None)));
}

/// Replay-only: shutdown of a busy worker whose interrupt Claude *rejects* is still an
/// interrupt the user asked for (not Crashed).
pub async fn shutdown_rejected_interrupt(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session.send_prompt("work").await.unwrap();
    events
        .until("ToolStarted", |e| {
            matches!(e, ProviderEvent::ToolStarted { .. })
        })
        .await;
    let _ = session.shutdown().await.expect("shutdown");
    assert_eq!(events.turn_completed().await.0, TurnStatus::Interrupted);
    events.terminated().await;
}

/// "Deny and stop": the turn ends as an interrupt the user asked for, not a failure.
pub async fn deny_and_stop(h: &Harness) {
    let cwd = h.workdir("a");
    let (session, mut events) = h.spawn("main", &cwd).await;
    session
        .send_prompt(
            "Run the shell command `ping -n 3 127.0.0.1` using your Bash tool and summarize it.",
        )
        .await
        .unwrap();
    let id = approval(&mut events, "Bash").await;
    session
        .respond_approval(
            &id,
            ApprovalDecision::Deny {
                message: "Stopped from Gantry".into(),
                interrupt: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(events.turn_completed().await.0, TurnStatus::Interrupted);
    assert!(resolved(&events, &id, Resolution::Denied));
    shutdown(session, &mut events).await;
    h.save_fixtures(&["main"]);
}

async fn approval(events: &mut Events, tool: &str) -> String {
    match events
        .until(&format!("approval for {tool}"), |e| {
            matches!(e, ProviderEvent::ApprovalRequested { tool_name, .. } if tool_name == tool)
        })
        .await
    {
        ProviderEvent::ApprovalRequested { interaction_id, .. } => interaction_id,
        _ => unreachable!(),
    }
}

fn resolved(events: &Events, id: &str, resolution: Resolution) -> bool {
    events.seen.iter().any(|e| {
        matches!(e, ProviderEvent::InteractionResolved { interaction_id, resolution: r, .. }
            if interaction_id == id && *r == resolution)
    })
}
