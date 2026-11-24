//! Test fixture: replays a recorded Claude Code `stream-json` session (spike X2).
//! Not part of the product.
//!
//! Reads the fixture named by `FAKE_CLAUDE_FIXTURE` (NDJSON lines `{"dir":"in"|"out",
//! "frame":{...}}`, as written by `ClaudeConfig::raw_log`). For every recorded `in` frame it
//! reads one line from stdin (checking the frame type matches); every `out` frame is printed.
//! Request ids Gantry generates are remapped from the recording to the live run, so the
//! replayed `control_response`s answer the requests actually sent. All CLI args are ignored.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde_json::Value;

fn main() {
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
                    return; // Gantry closed stdin: end of session
                }
                let actual: Value = serde_json::from_str(got.trim()).unwrap_or(Value::Null);
                if actual["type"] != frame["type"] {
                    eprintln!(
                        "fake-claude: fixture line {}: expected `{}` frame, got `{}`",
                        n + 1,
                        frame["type"],
                        actual["type"]
                    );
                    std::process::exit(3);
                }
                if let (Some(rec), Some(act)) =
                    (frame["request_id"].as_str(), actual["request_id"].as_str())
                {
                    ids.insert(rec.to_owned(), act.to_owned());
                }
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
            // Hand-written fixtures only: simulate the process dying at this point.
            Some("exit") => std::process::exit(record["code"].as_i64().unwrap_or(1) as i32),
            _ => {}
        }
    }
    // Like the real CLI: stay alive until Gantry closes stdin.
    let mut sink = String::new();
    while input.read_line(&mut sink).unwrap_or(0) > 0 {
        sink.clear();
    }
}
