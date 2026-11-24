//! Stdio framing helpers for NDJSON agent protocols.
//!
//! * [`LineReader`] — bounded newline framing that tolerates CRLF, invalid UTF-8 and
//!   multi-megabyte lines (base64 screenshots, large file reads) without unbounded memory.
//! * [`StderrTail`] — continuously drains a stream into a ring buffer so a chatty child can
//!   never fill its stderr pipe and stall (the classic Windows 4 KiB pipe deadlock).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, BufReader};
use tokio::task::JoinHandle;

/// Default maximum line length: 64 MiB.
pub const DEFAULT_MAX_LINE: usize = 64 * 1024 * 1024;

/// One framed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// The line, with the trailing `\n` / `\r\n` removed. Invalid UTF-8 is replaced lossily.
    Text(String),
    /// The line exceeded the limit and was discarded; `len` is its full length in bytes. The
    /// reader has resynchronized on the next newline.
    Oversized { len: usize },
}

/// Bounded line reader. Not cancel-safe: drive it from one dedicated reader task.
#[derive(Debug)]
pub struct LineReader<R> {
    inner: R,
    buf: Vec<u8>,
    max: usize,
}

impl<R: AsyncRead + Unpin> LineReader<BufReader<R>> {
    /// Wraps a raw reader with a 64 KiB buffer.
    pub fn new(reader: R, max_line: usize) -> Self {
        Self::from_buf_read(BufReader::with_capacity(64 * 1024, reader), max_line)
    }
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    pub fn from_buf_read(inner: R, max_line: usize) -> Self {
        LineReader {
            inner,
            buf: Vec::new(),
            max: max_line,
        }
    }

    /// Reads the next line; `Ok(None)` at EOF. A final unterminated line is still returned.
    pub async fn next_line(&mut self) -> std::io::Result<Option<Line>> {
        self.buf.clear();
        let mut total = 0usize;
        let mut oversized = false;
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                if total == 0 {
                    return Ok(None);
                }
                break;
            }
            let (take, done) = match available.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (available.len(), false),
            };
            total += take;
            if !oversized {
                if self.buf.len() + take > self.max.saturating_add(2) {
                    oversized = true;
                    self.buf = Vec::new();
                } else {
                    self.buf.extend_from_slice(&available[..take]);
                }
            }
            self.inner.consume(take);
            if done {
                break;
            }
        }
        if oversized {
            return Ok(Some(Line::Oversized { len: total }));
        }
        if self.buf.last() == Some(&b'\n') {
            self.buf.pop();
        }
        if self.buf.last() == Some(&b'\r') {
            self.buf.pop();
        }
        if self.buf.len() > self.max {
            return Ok(Some(Line::Oversized { len: total }));
        }
        let bytes = std::mem::take(&mut self.buf);
        let text = String::from_utf8(bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        Ok(Some(Line::Text(text)))
    }
}

/// Continuously drains a stream line-by-line into a bounded ring buffer.
#[derive(Debug)]
pub struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
    task: JoinHandle<()>,
}

impl StderrTail {
    /// Spawns the drain task. `on_line` is called for every line (e.g. to emit a
    /// `ProviderStderr` event); the last `capacity` lines are retained for diagnostics.
    pub fn spawn<R>(
        reader: R,
        capacity: usize,
        mut on_line: impl FnMut(&str) + Send + 'static,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let lines = Arc::new(Mutex::new(VecDeque::with_capacity(capacity.min(1024))));
        let sink = Arc::clone(&lines);
        let task = tokio::spawn(async move {
            // stderr lines are diagnostics; cap them far lower than protocol lines.
            let mut reader = LineReader::new(reader, 1024 * 1024);
            while let Ok(Some(line)) = reader.next_line().await {
                let text = match line {
                    Line::Text(t) => t,
                    Line::Oversized { len } => format!("<{len} byte line elided>"),
                };
                on_line(&text);
                let mut ring = sink.lock().unwrap_or_else(|p| p.into_inner());
                if ring.len() == capacity {
                    ring.pop_front();
                }
                if capacity > 0 {
                    ring.push_back(text);
                }
            }
        });
        StderrTail { lines, task }
    }

    /// The most recent lines, oldest first.
    pub fn snapshot(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Waits for the stream to reach EOF.
    pub async fn finished(self) -> Vec<String> {
        let StderrTail { lines, task } = self;
        let _ = task.await;
        lines
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn lines(input: &[u8], max: usize) -> Vec<Line> {
        let mut r = LineReader::from_buf_read(BufReader::with_capacity(7, input), max);
        let mut out = Vec::new();
        while let Some(l) = r.next_line().await.unwrap() {
            out.push(l);
        }
        out
    }

    #[tokio::test]
    async fn frames_crlf_lossy_and_unterminated() {
        let got = lines(b"one\r\ntwo\n\nbad\xffutf8\nlast", 1024).await;
        assert_eq!(
            got,
            vec![
                Line::Text("one".into()),
                Line::Text("two".into()),
                Line::Text(String::new()),
                Line::Text("bad\u{fffd}utf8".into()),
                Line::Text("last".into()),
            ]
        );
    }

    #[tokio::test]
    async fn oversized_lines_resync() {
        let mut input = vec![b'x'; 100];
        input.extend_from_slice(b"\nok\r\n");
        input.extend_from_slice(&[b'y'; 10]);
        input.push(b'\n');
        let got = lines(&input, 10).await;
        assert_eq!(
            got,
            vec![
                Line::Oversized { len: 101 },
                Line::Text("ok".into()),
                Line::Text("y".repeat(10))
            ]
        );
    }
}
