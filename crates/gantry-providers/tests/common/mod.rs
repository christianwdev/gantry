//! Shared harness for the Claude X2 scenarios. Each scenario runs either **live** (real
//! `claude`, recording sanitized fixtures) or as a **replay** (the `fake-claude` binary plays
//! the fixture back), so CI exercises the real session actor without a Claude login.

#![allow(dead_code)] // each test binary uses a different subset

pub mod scenarios;

use std::path::{Path, PathBuf};
use std::time::Duration;

use gantry_providers::claude::{ClaudeConfig, ClaudeSession};
use gantry_providers::event::{ProviderEvent, TurnStatus};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Live,
    Replay,
}

pub struct Harness {
    pub mode: Mode,
    pub name: &'static str,
    root: PathBuf,
}

impl Harness {
    pub fn live(name: &'static str) -> Self {
        Self::new(Mode::Live, name)
    }

    pub fn replay(name: &'static str) -> Self {
        Self::new(Mode::Replay, name)
    }

    fn new(mode: Mode, name: &'static str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "gantry-x2-{name}-{:?}-{}",
            mode,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Harness { mode, name, root }
    }

    /// A fresh absolute working directory for one session.
    pub fn workdir(&self, label: &str) -> PathBuf {
        let dir = self.root.join(format!("work-{label}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixture(&self, session: &str) -> PathBuf {
        fixtures_dir().join(format!("{}-{session}.ndjson", self.name))
    }

    fn raw(&self, session: &str) -> PathBuf {
        self.root.join(format!("raw-{session}.ndjson"))
    }

    /// Config for session `session` (several per scenario are allowed), running in `cwd`.
    pub fn config(&self, session: &str, cwd: &Path) -> ClaudeConfig {
        let mut config = ClaudeConfig::new(cwd, self.root.join("runtime"));
        match self.mode {
            Mode::Live => {
                config.model = Some("haiku".into());
                config.raw_log = Some(self.raw(session));
            }
            Mode::Replay => {
                config.program = env!("CARGO_BIN_EXE_fake-claude").into();
                let fixture = self.fixture(session);
                assert!(
                    fixture.is_file(),
                    "missing fixture {} — record it with the ignored live test",
                    fixture.display()
                );
                config
                    .env
                    .push(("FAKE_CLAUDE_FIXTURE".into(), fixture.into()));
            }
        }
        config
    }

    pub async fn spawn(&self, session: &str, cwd: &Path) -> (ClaudeSession, Events) {
        let config = self.config(session, cwd);
        let (s, rx) = ClaudeSession::spawn(config)
            .await
            .expect("claude session starts");
        (s, self.events(rx))
    }

    pub fn events(&self, rx: UnboundedReceiver<ProviderEvent>) -> Events {
        Events {
            rx,
            seen: Vec::new(),
            timeout: match self.mode {
                Mode::Live => Duration::from_secs(180),
                Mode::Replay => Duration::from_secs(20),
            },
        }
    }

    /// Live: turns the raw logs of `sessions` into committed, sanitized fixtures.
    pub fn save_fixtures(&self, sessions: &[&str]) {
        if self.mode != Mode::Live {
            return;
        }
        std::fs::create_dir_all(fixtures_dir()).unwrap();
        for session in sessions {
            let raw = std::fs::read_to_string(self.raw(session)).expect("raw log");
            let sanitized: Vec<String> = raw
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| sanitize_record(serde_json::from_str(l).unwrap(), &self.root).to_string())
                .collect();
            std::fs::write(self.fixture(session), sanitized.join("\n") + "\n").unwrap();
        }
    }
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(r"tests\fixtures\claude")
}

/// Strips machine- and account-specific data so fixtures can live in a public repo.
fn sanitize_record(mut record: Value, root: &Path) -> Value {
    let frame = &mut record["frame"];
    // initialize response: account details and the user's installed commands/agents.
    if frame["type"] == "control_response"
        && let Some(resp) = frame
            .pointer_mut("/response/response")
            .and_then(Value::as_object_mut)
    {
        for key in [
            "account",
            "commands",
            "agents",
            "available_output_styles",
            "user_output_styles_dir",
            "pid",
        ] {
            resp.remove(key);
        }
    }
    // system:init: the user's tools/skills/plugins inventory.
    if frame["type"] == "system"
        && frame["subtype"] == "init"
        && let Some(obj) = frame.as_object_mut()
    {
        for key in ["tools", "slash_commands", "skills", "plugins", "agents"] {
            obj.remove(key);
        }
    }
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let root = root.to_string_lossy().into_owned();
    // The username also appears outside full paths: in Claude's project slug
    // (`C--Users-<name>-...`) and inside streamed partial_json/text chunks that split a path.
    // The account name and the profile folder name can differ (e.g. a renamed account), and
    // it's the folder name that appears in paths — scrub both.
    let account = std::env::var("USERNAME").unwrap_or_default();
    let folder = Path::new(&home)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A name this short can't be scrubbed without mangling unrelated text: refuse to record
    // rather than silently commit it to a public repo.
    for name in [&account, &folder] {
        assert!(
            name.chars().count() >= 3,
            "username `{name}` is too short to scrub from fixtures safely; record elsewhere"
        );
    }
    // Longest first, so "Alexandra" isn't half-replaced by "Alex".
    let mut names = [account, folder];
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    replace_strings(
        &mut record,
        &[
            (&root, "%WORK%"),
            (&home, "%HOME%"),
            (&names[0], "user"),
            (&names[1], "user"),
        ],
    );
    record
}

/// Case-insensitive replacement: Windows paths and names come back in any case
/// (`C:\Users\X` vs `c:\users\x`).
fn replace_strings(v: &mut Value, pairs: &[(&str, &str)]) {
    match v {
        Value::String(s) => {
            for (from, to) in pairs {
                if !from.is_empty() {
                    *s = replace_ci(s, from, to);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| replace_strings(x, pairs)),
        Value::Object(o) => o.values_mut().for_each(|x| replace_strings(x, pairs)),
        _ => {}
    }
}

fn replace_ci(haystack: &str, needle: &str, with: &str) -> String {
    let lower_hay = haystack.to_lowercase();
    let lower_needle = needle.to_lowercase();
    // Lower-casing can change byte lengths for non-ASCII text; fall back to exact matching then.
    if lower_hay.len() != haystack.len() || lower_needle.len() != needle.len() {
        return haystack.replace(needle, with);
    }
    let mut out = String::with_capacity(haystack.len());
    let mut rest = 0;
    for (at, _) in lower_hay.match_indices(&lower_needle) {
        out.push_str(&haystack[rest..at]);
        out.push_str(with);
        rest = at + needle.len();
    }
    out.push_str(&haystack[rest..]);
    out
}

pub struct Events {
    rx: UnboundedReceiver<ProviderEvent>,
    pub seen: Vec<ProviderEvent>,
    timeout: Duration,
}

impl Events {
    /// Waits for the first event matching `pred` (recording everything seen).
    pub async fn until(
        &mut self,
        what: &str,
        pred: impl Fn(&ProviderEvent) -> bool,
    ) -> ProviderEvent {
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}; saw {:#?}", self.seen))
                .unwrap_or_else(|| {
                    panic!(
                        "event stream ended waiting for {what}; saw {:#?}",
                        self.seen
                    )
                });
            self.seen.push(event.clone());
            if pred(&event) {
                return event;
            }
        }
    }

    /// Waits for the current turn to complete; returns its status and result text.
    pub async fn turn_completed(&mut self) -> (TurnStatus, Option<String>) {
        match self
            .until("TurnCompleted", |e| {
                matches!(e, ProviderEvent::TurnCompleted { .. })
            })
            .await
        {
            ProviderEvent::TurnCompleted { status, result, .. } => (status, result),
            _ => unreachable!(),
        }
    }

    /// Waits for `SessionTerminated`, then checks the stream really ends (no sender outlives
    /// termination, even while the session handle is still alive).
    pub async fn terminated(&mut self) {
        self.until("SessionTerminated", |e| {
            matches!(e, ProviderEvent::SessionTerminated { .. })
        })
        .await;
        match tokio::time::timeout(Duration::from_secs(5), self.rx.recv()).await {
            Ok(None) => {}
            Ok(Some(e)) => panic!("event after SessionTerminated: {e:?}"),
            Err(_) => panic!("event stream did not end after SessionTerminated"),
        }
    }

    pub fn count(&self, pred: impl Fn(&ProviderEvent) -> bool) -> usize {
        self.seen.iter().filter(|e| pred(e)).count()
    }
}
