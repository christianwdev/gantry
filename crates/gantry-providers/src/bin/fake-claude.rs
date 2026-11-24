//! Test fixture: replays a recorded Claude Code `stream-json` session (spike X2).
//! Not part of the product.
//!
//! Reads the fixture named by `FAKE_CLAUDE_FIXTURE` (NDJSON lines `{"dir":"in"|"out",
//! "frame":{...}}`, as written by `ClaudeConfig::raw_log`). For every recorded `in` frame it
//! reads one line from stdin, which must equal the recorded frame; every `out` frame is printed.
//! Request ids Gantry generates are remapped from the recording to the live run, so the
//! replayed `control_response`s answer the requests actually sent. Any divergence (a different
//! frame, stdin closing early, input after the end) is reported on stderr as `fake-claude: ...`
//! with exit code 3, which the test harness fails on. All CLI args are ignored.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde_json::Value;

fn main() {
    // Helper mode (see the `orphan_stdout` directive): just hold the inherited stdout open.
    if std::env::var_os("FAKE_CLAUDE_HOLD_STDOUT").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    let path = std::env::var("FAKE_CLAUDE_FIXTURE").expect("FAKE_CLAUDE_FIXTURE is set");
    let fixture = std::fs::read_to_string(&path).expect("fixture readable");
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout().lock();
    let mut ids: HashMap<String, String> = HashMap::new();

    for (n, line) in fixture
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let record: Value = serde_json::from_str(line).expect("fixture line is JSON");
        let mut frame = record["frame"].clone();
        match record["dir"].as_str() {
            Some("in") => {
                let mut got = String::new();
                if input.read_line(&mut got).unwrap_or(0) == 0 {
                    // Gantry closed stdin before writing a frame the recording says it wrote.
                    fail(&format!(
                        "fixture line {}: stdin closed, expected {frame}",
                        n + 1
                    ));
                }
                let mut actual: Value = serde_json::from_str(got.trim()).unwrap_or(Value::Null);
                // Gantry's own request ids are remapped, not compared; everything else Gantry
                // writes must match the recording exactly (a wrong allow/deny, answer or
                // request id would otherwise replay as if it were right).
                if frame["type"] == "control_request"
                    && let (Some(rec), Some(act)) =
                        (frame["request_id"].as_str(), actual["request_id"].as_str())
                {
                    ids.insert(rec.to_owned(), act.to_owned());
                    actual["request_id"] = frame["request_id"].clone();
                }
                if actual != frame {
                    fail(&format!(
                        "fixture line {}: expected {frame}, got {}",
                        n + 1,
                        got.trim()
                    ));
                }
            }
            Some("out") if record.get("raw").is_some() => {
                // A non-JSON stdout line, replayed verbatim.
                writeln!(out, "{}", record["raw"].as_str().unwrap_or("")).unwrap();
                out.flush().unwrap();
            }
            Some("out") => {
                // Only control responses answer Gantry's requests (indexing others mutably
                // would insert keys into them).
                if frame["type"] == "control_response"
                    && let Some(rid) = frame.pointer_mut("/response/request_id")
                    && let Some(mapped) = rid.as_str().and_then(|r| ids.get(r))
                {
                    *rid = Value::String(mapped.clone());
                }
                writeln!(out, "{frame}").unwrap();
                out.flush().unwrap();
            }
            // Hand-written fixtures only: Gantry closes stdin here (nothing more is written);
            // like the real CLI, stop without a result...
            Some("eof") => {
                let mut extra = String::new();
                if input.read_line(&mut extra).unwrap_or(0) > 0 {
                    fail(&format!(
                        "fixture line {}: expected stdin to close, got {}",
                        n + 1,
                        extra.trim()
                    ));
                }
                return;
            }
            // ...simulate the process dying at this point...
            Some("exit") => std::process::exit(record["code"].as_i64().unwrap_or(1) as i32),
            // ...a slow reply...
            Some("sleep") => std::thread::sleep(std::time::Duration::from_millis(
                record["ms"].as_u64().unwrap_or(0),
            )),
            // ...or a diagnostic on stderr (e.g. a crash message).
            Some("stderr") => {
                eprintln!("{}", record["text"].as_str().unwrap_or(""));
            }
            // Start a descendant that inherits (and keeps open) this process's stdout, like a
            // background helper would.
            Some("orphan_stdout") => {
                #[allow(
                    clippy::disallowed_methods,
                    clippy::disallowed_types,
                    clippy::zombie_processes
                )]
                std::process::Command::new(std::env::current_exe().unwrap())
                    .env("FAKE_CLAUDE_HOLD_STDOUT", "1")
                    .stdin(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn stdout holder");
            }
            _ => {}
        }
    }
    // Like the real CLI: stay alive until Gantry closes stdin. Anything Gantry sends now
    // (a duplicate answer, an extra prompt) wasn't in the recording.
    let mut extra = String::new();
    while input.read_line(&mut extra).unwrap_or(0) > 0 {
        if !extra.trim().is_empty() {
            fail(&format!(
                "unexpected input after the fixture ended: {}",
                extra.trim()
            ));
        }
        extra.clear();
    }
}

/// Reports a divergence from the recording. The harness fails any test whose session printed
/// a `fake-claude:` line, so this can't go unnoticed even when the exit itself would.
fn fail(message: &str) -> ! {
    eprintln!("fake-claude: {message}");
    std::process::exit(3);
}
