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
            let records: Vec<Value> = raw
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let sanitized: Vec<String> = sanitize_session(records, &self.root)
                .iter()
                .map(Value::to_string)
                .collect();
            std::fs::write(self.fixture(session), sanitized.join("\n") + "\n").unwrap();
        }
    }
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(r"tests\fixtures\claude")
}

/// Scrubs machine- and account-specific strings (work dir, home dir, user names).
struct Scrubber {
    /// (from, to), applied in order; names longest first.
    pairs: Vec<(String, String)>,
    /// The user names, for the final "nothing left" check.
    names: Vec<String>,
}

impl Scrubber {
    fn new(root: &Path) -> Self {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let root = root.to_string_lossy().into_owned();
        // The username also appears outside full paths: in Claude's project slug
        // (`C--Users-<name>-...`) and in streamed chunks. The account name and the profile
        // folder name can differ (e.g. a renamed account), and it's the folder name that
        // appears in paths — scrub both.
        let account = std::env::var("USERNAME").unwrap_or_default();
        let folder = Path::new(&home)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        // A name this short can't be scrubbed without mangling unrelated text: refuse to
        // record rather than silently commit it to a public repo.
        for name in [&account, &folder] {
            assert!(
                name.chars().count() >= 3,
                "username `{name}` is too short to scrub from fixtures safely; record elsewhere"
            );
        }
        // Longest first, so "Alexandra" isn't half-replaced by "Alex".
        let mut names = vec![account, folder];
        names.sort_by_key(|n| std::cmp::Reverse(n.len()));
        names.dedup();
        let escaped = |s: &str| s.replace('\\', "\\\\"); // as it appears inside JSON text
        let mut pairs = vec![
            (root.clone(), "%WORK%".to_owned()),
            (escaped(&root), "%WORK%".to_owned()),
            (home.clone(), "%HOME%".to_owned()),
            (escaped(&home), "%HOME%".to_owned()),
        ];
        pairs.extend(names.iter().map(|n| (n.clone(), "user".to_owned())));
        Scrubber { pairs, names }
    }

    fn scrub_str(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (from, to) in &self.pairs {
            if !from.is_empty() {
                out = replace_ci(&out, from, to);
            }
        }
        out
    }

    fn scrub_value(&self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.scrub_str(s),
            Value::Array(a) => a.iter_mut().for_each(|x| self.scrub_value(x)),
            Value::Object(o) => o.values_mut().for_each(|x| self.scrub_value(x)),
            _ => {}
        }
    }

    fn leaks(&self, s: &str) -> bool {
        let lower = s.to_lowercase();
        self.names.iter().any(|n| lower.contains(&n.to_lowercase()))
    }
}

/// Strips machine- and account-specific data from a whole recorded session so it can live in
/// a public repo, and fails loudly if anything identifying survives.
fn sanitize_session(mut records: Vec<Value>, root: &Path) -> Vec<Value> {
    let scrubber = Scrubber::new(root);
    for record in &mut records {
        strip_inventory(&mut record["frame"]);
        scrubber.scrub_value(record);
    }
    // Streamed deltas can split a name across chunks (`...Users\\Ch` + `ris\\App...`), which
    // per-string scrubbing can't see. Join each content block's deltas, scrub the joined text,
    // and put it back as one delta (the rest emptied) — the replayed text is unchanged.
    scrub_split_deltas(&mut records, &scrubber);
    for record in &records {
        let text = record.to_string();
        assert!(
            !scrubber.leaks(&text),
            "a user name survived sanitizing: {text}"
        );
    }
    let joined: String = records.iter().filter_map(delta_text).collect();
    assert!(
        !scrubber.leaks(&joined),
        "a user name survives in the joined streamed deltas"
    );
    records
}

/// Removes the user's installed-tooling inventory and account details.
fn strip_inventory(frame: &mut Value) {
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
}

/// The delta payload field name for a `content_block_delta` record, if any.
fn delta_field(record: &Value) -> Option<&'static str> {
    if record["frame"]["type"] != "stream_event"
        || record["frame"]["event"]["type"] != "content_block_delta"
    {
        return None;
    }
    match record["frame"]["event"]["delta"]["type"].as_str()? {
        "text_delta" => Some("text"),
        "thinking_delta" => Some("thinking"),
        "input_json_delta" => Some("partial_json"),
        _ => None,
    }
}

fn delta_text(record: &Value) -> Option<String> {
    let field = delta_field(record)?;
    record["frame"]["event"]["delta"][field]
        .as_str()
        .map(str::to_owned)
}

fn scrub_split_deltas(records: &mut [Value], scrubber: &Scrubber) {
    use std::collections::HashMap;
    // One group per streamed content block: a `content_block_start` for (scope, index) opens a
    // new group (later messages reuse block indices), deltas join the open group.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut open: HashMap<(String, u64), usize> = HashMap::new();
    for (i, record) in records.iter().enumerate() {
        let event = &record["frame"]["event"];
        if record["frame"]["type"] != "stream_event" {
            continue;
        }
        let key = (
            record["frame"]["parent_tool_use_id"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            event["index"].as_u64().unwrap_or(0),
        );
        if event["type"] == "content_block_start" {
            groups.push(Vec::new());
            open.insert(key, groups.len() - 1);
        } else if delta_field(record).is_some() {
            let group = *open.entry(key).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[group].push(i);
        }
    }
    for idxs in &groups {
        let joined: String = idxs
            .iter()
            .filter_map(|&i| delta_text(&records[i]))
            .collect();
        if !scrubber.leaks(&joined) {
            continue;
        }
        let scrubbed = scrubber.scrub_str(&joined);
        for (n, &i) in idxs.iter().enumerate() {
            let field = delta_field(&records[i]).expect("delta record");
            records[i]["frame"]["event"]["delta"][field] = Value::String(if n == 0 {
                scrubbed.clone()
            } else {
                String::new()
            });
        }
    }
}

/// Case-insensitive replacement: Windows paths and names come back in any case
/// (`C:\Users\X` vs `c:\users\x`).
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

#[cfg(test)]
mod sanitize_tests {
    use serde_json::json;

    use super::*;

    fn delta(text: &str) -> Value {
        json!({"dir":"out","frame":{"type":"stream_event","parent_tool_use_id":null,
            "event":{"type":"content_block_delta","index":0,
                "delta":{"type":"input_json_delta","partial_json":text}}}})
    }

    /// A user name split across two streamed chunks is still scrubbed, and the replayed text
    /// (the joined deltas) keeps everything else.
    #[test]
    fn names_split_across_deltas_are_scrubbed() {
        let home = std::env::var("USERPROFILE").unwrap();
        let name = Path::new(&home)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let (a, b) = name.split_at(name.len() / 2);
        let records = vec![
            json!({"dir":"out","frame":{"type":"stream_event","parent_tool_use_id":null,
                "event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use"}}}}),
            delta(&format!("{{\"file_path\":\"D:\\x\\{a}")),
            delta(&format!("{b}\\notes.txt\"}}")),
        ];
        let out = sanitize_session(records, Path::new(r"C:\no-such-root"));
        let joined: String = out.iter().filter_map(delta_text).collect();
        assert!(
            !joined.to_lowercase().contains(&name.to_lowercase()),
            "{joined}"
        );
        assert_eq!(joined, "{\"file_path\":\"D:\\x\\user\\notes.txt\"}");
    }
}
