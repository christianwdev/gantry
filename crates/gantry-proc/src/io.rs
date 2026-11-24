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

/// Consecutive stderr read errors after which `StderrTail` gives up on the stream.
const MAX_CONSECUTIVE_READ_ERRORS: u32 = 8;

/// Default maximum line length: 64 MiB.
pub const DEFAULT_MAX_LINE: usize = 64 * 1024 * 1024;

/// One framed line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// The line, with the trailing `\n` / `\r\n` removed. Invalid UTF-8 is replaced lossily.
    Text(String),
    /// The line exceeded the limit and was discarded. `len` is the raw number of bytes
    /// consumed, **including** its `\n` / `\r\n` terminator (unlike `Text`, which strips it).
    /// The reader has resynchronized on the next newline.
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
        // Strip "\n" or "\r\n" only; a lone trailing '\r' on an unterminated final line is
        // content, not a terminator.
        if self.buf.last() == Some(&b'\n') {
            self.buf.pop();
            if self.buf.last() == Some(&b'\r') {
                self.buf.pop();
            }
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
            // Keep draining through transient read errors: if this task ended, `reader` (the
            // pipe's server end) would be dropped and a still-running child's next stderr write
            // would fail (EPIPE), which can crash e.g. Node-based agents. Only a run of
            // consecutive errors (a genuinely broken pipe) ends the drain.
            let mut consecutive_errors = 0u32;
            loop {
                let text = match reader.next_line().await {
                    Ok(None) => break,
                    Ok(Some(line)) => {
                        consecutive_errors = 0;
                        match line {
                            Line::Text(t) => t,
                            Line::Oversized { len } => format!("<{len} byte line elided>"),
                        }
                    }
                    Err(e) => {
                        consecutive_errors += 1;
                        if consecutive_errors >= MAX_CONSECUTIVE_READ_ERRORS {
                            break;
                        }
                        format!("<stderr read error: {e}>")
                    }
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

    /// Yields one transient error, then a line, then EOF.
    struct FlakyOnce(u8);

    impl AsyncRead for FlakyOnce {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.0 += 1;
            std::task::Poll::Ready(match self.0 {
                1 => Err(std::io::Error::other("transient")),
                2 => {
                    buf.put_slice(b"after error\n");
                    Ok(())
                }
                _ => Ok(()),
            })
        }
    }

    #[tokio::test]
    async fn stderr_tail_keeps_draining_after_a_transient_error() {
        let tail = StderrTail::spawn(FlakyOnce(0), 10, |_| {});
        let lines = tail.finished().await;
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("<stderr read error"), "{lines:?}");
        assert_eq!(lines[1], "after error");
    }

    #[tokio::test]
    async fn lone_trailing_cr_on_final_line_is_content() {
        assert_eq!(
            lines(b"a\r\nabc\r", 1024).await,
            vec![Line::Text("a".into()), Line::Text("abc\r".into())]
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
