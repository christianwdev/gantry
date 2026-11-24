//! Startup health check for the external CLIs Gantry orchestrates (roadmap Phase 1).
//!
//! Every probe goes through `gantry_proc::ManagedCommand`, so it resolves to a real `.exe` on
//! the merged process + registry `PATH`, runs without a console window, and is killed with its
//! whole tree if it hangs past the timeout.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gantry_proc::{ExitStatus, ManagedCommand, Stdio};
use serde::Serialize;
use specta::Type;
use tokio::io::AsyncReadExt;

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Once output has appeared after the probed process exited, how long leftover processes may
/// keep stdout open before the probe kills its tree (a helper holding the pipe).
const READ_GRACE: Duration = Duration::from_secs(2);
/// How long to finish reading after killing the probe's tree.
const KILL_GRACE: Duration = Duration::from_secs(1);
/// Poll interval while waiting for leftover output after the probed process exits.
const POLL: Duration = Duration::from_millis(100);

/// CLIs probed on startup: (display name, program, version args).
const TOOLS: &[(&str, &str, &[&str])] = &[
    ("Claude Code", "claude", &["--version"]),
    ("Codex", "codex", &["--version"]),
    ("Antigravity", "agy", &["--version"]),
    ("Git", "git", &["--version"]),
    ("Node.js", "node", &["--version"]),
    ("Bun", "bun", &["--version"]),
];

/// Overall outcome of probing one CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ToolState {
    /// Resolved and reported a version.
    Ok,
    /// Not found on the (merged process + registry) `PATH`.
    Missing,
    /// Only a `.cmd`/`.bat`/`.ps1` shim exists (e.g. an npm global install); Gantry needs the
    /// real executable.
    ShimOnly,
    /// Found, but `--version` failed, timed out or printed nothing.
    Failed,
}

/// Result of probing one external CLI.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    /// Human-readable tool name.
    pub name: String,
    /// The program name that was resolved.
    pub program: String,
    pub state: ToolState,
    /// The resolved executable (or the rejected shim), if any.
    pub path: Option<String>,
    /// First non-empty line of `--version` output, when `state` is `ok`.
    pub version: Option<String>,
    /// Human-readable reason when `state` is not `ok`.
    pub error: Option<String>,
}

pub async fn probe_all() -> Vec<ToolStatus> {
    let probes = TOOLS.iter().map(|&(name, program, args)| {
        tauri::async_runtime::spawn(async move { probe(name, program, args).await })
    });
    let mut out = Vec::with_capacity(TOOLS.len());
    for (handle, &(name, program, _)) in probes.collect::<Vec<_>>().into_iter().zip(TOOLS) {
        out.push(handle.await.unwrap_or_else(|e| ToolStatus {
            name: name.to_owned(),
            program: program.to_owned(),
            state: ToolState::Failed,
            path: None,
            version: None,
            error: Some(format!("probe task failed: {e}")),
        }));
    }
    out
}

async fn probe(name: &str, program: &str, args: &[&str]) -> ToolStatus {
    let mut status = ToolStatus {
        name: name.to_owned(),
        program: program.to_owned(),
        state: ToolState::Failed,
        path: None,
        version: None,
        error: None,
    };
    let mut cmd = ManagedCommand::new(program);
    // stderr is captured too: real CLIs print their errors there (console.error, eprintln),
    // and a few print their version there.
    cmd.args(args)
        .env("DISABLE_AUTOUPDATER", "1")
        .stdout(Stdio::Piped)
        .stderr(Stdio::Piped);

    // These live outside `run` so a timeout (which drops `run`) doesn't lose them.
    let launched = Arc::new(Mutex::new(None::<String>));
    let launched_slot = Arc::clone(&launched);
    let captured = Captured::default();
    let sinks = captured.clone();
    let exited = Arc::new(Mutex::new(None::<ExitStatus>));
    let exited_slot = Arc::clone(&exited);

    // PROBE_TIMEOUT covers everything, including resolution.
    let run = async move {
        // Resolution and CreateProcess do blocking registry/filesystem work (a PATH entry on an
        // unreachable network share can stall for tens of seconds), so keep them off the async
        // workers. If we time out meanwhile, the late ManagedChild is dropped -> tree killed.
        let mut child = tokio::task::spawn_blocking(move || cmd.spawn())
            .await
            .map_err(|e| gantry_proc::Error::Io(std::io::Error::other(e)))??;
        // Remember what actually launched, so even a timed-out probe can say which binary hung.
        *lock(&launched_slot) = Some(child.program().display().to_string());
        // Drain both streams concurrently (so a full pipe can't stall the child), but finish on
        // the process exiting, not on EOF: a helper that inherited a stream would hold it open.
        let readers = [
            capture(child.stdout.take(), Arc::clone(&sinks.stdout)),
            capture(child.stderr.take(), Arc::clone(&sinks.stderr)),
        ];
        let exit = child.wait().await?;
        *lock(&exited_slot) = Some(exit);
        // After exit: a launcher stub may have handed off to the real binary, which can take a
        // while (cold start, AV scan) to print. So keep waiting while nothing has been printed
        // (bounded by PROBE_TIMEOUT). READ_GRACE is measured from when output first appeared,
        // so a binary that starts printing late still gets the full grace to finish its line;
        // a failed exit is already the answer, so its grace starts at the exit. When the grace
        // is up, kill the tree, which closes any pipe a lingering helper holds -> EOF.
        let mut grace_from = (!exit.success()).then(Instant::now);
        while !readers.iter().all(|r| r.is_finished()) {
            tokio::time::sleep(POLL).await;
            if grace_from.is_none() && sinks.has_output() {
                grace_from = Some(Instant::now());
            }
            if grace_from.is_some_and(|t| t.elapsed() >= READ_GRACE) {
                let _ = child.kill_tree();
                let deadline = Instant::now() + KILL_GRACE;
                while !readers.iter().all(|r| r.is_finished()) && Instant::now() < deadline {
                    tokio::time::sleep(POLL).await;
                }
                break;
            }
        }
        // Don't leave detached helpers running after a probe.
        let _ = child.kill_tree();
        Ok::<_, gantry_proc::Error>((exit, child.program().to_path_buf()))
    };

    match tokio::time::timeout(PROBE_TIMEOUT, run).await {
        Ok(Ok((exit, ran))) => {
            // Report the executable that actually ran.
            status.path = Some(ran.display().to_string());
            if !exit.success() {
                status.error = Some(failure_message(program, exit, &captured));
            } else if let Some(version) = captured.version() {
                status.state = ToolState::Ok;
                status.version = Some(version);
            } else if let Some(line) = captured.first_line() {
                // Exit 0 but nothing version-like (e.g. only a warning banner): not verified.
                status.error = Some(format!(
                    "`{program} --version` printed `{line}`, no version"
                ));
            } else {
                status.error = Some(format!("`{program} --version` printed no version"));
            }
        }
        Ok(Err(e)) => {
            match &e {
                gantry_proc::Error::NotFound { .. } => status.state = ToolState::Missing,
                gantry_proc::Error::ShimOnly { shim, .. } => {
                    status.state = ToolState::ShimOnly;
                    status.path = Some(shim.display().to_string());
                }
                // Failed after launch (e.g. the wait itself failed): still say what ran.
                _ => status.path = lock(&launched).take(),
            }
            status.error = Some(e.to_string());
        }
        // Dropping the child (and its job) inside `run` killed the probe's process tree.
        Err(_) => {
            status.path = lock(&launched).take();
            let exit = *lock(&exited);
            let secs = PROBE_TIMEOUT.as_secs();
            // Whether the probed process itself exited (only a leftover held a stream open).
            let exited_ok = exit.is_some_and(|e| e.success());
            let lingered = if exited_ok {
                format!("a leftover process kept its output open past {secs}s")
            } else {
                format!("it did not exit within {secs}s")
            };
            status.error = Some(match (exit, captured.version(), captured.first_line()) {
                // The process already failed; the timeout only hit while draining leftovers.
                (Some(e), _, _) if !e.success() => failure_message(program, e, &captured),
                // It answered but then hung (e.g. a slow update check): usable, but say so.
                (_, Some(version), _) => {
                    status.state = ToolState::Ok;
                    status.version = Some(version);
                    format!("`{program} --version` printed a version, but {lingered}")
                }
                // An error banner or other text, then a hang: not healthy.
                (_, None, Some(line)) => {
                    format!("`{program} --version` printed `{line}`, no version, and {lingered}")
                }
                (_, None, None) if exited_ok => {
                    format!(
                        "`{program} --version` exited without printing a version, but {lingered}"
                    )
                }
                (_, None, None) => format!("`{program} --version` timed out"),
            });
        }
    }
    status
}

/// Only this much of each stream is kept from a probe (the version is on the first lines); the
/// rest is still drained so the CLI never blocks on a full pipe.
const MAX_CAPTURE: usize = 64 * 1024;

/// A probe's captured stdout and stderr (each bounded by [`MAX_CAPTURE`]).
#[derive(Clone, Default)]
struct Captured {
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Captured {
    /// The version line: preferably from stdout, else from stderr (some CLIs print it there).
    fn version(&self) -> Option<String> {
        version_line(&lock(&self.stdout)).or_else(|| version_line(&lock(&self.stderr)))
    }

    /// The first non-empty line, preferring stdout; usually the reason for a failure.
    fn first_line(&self) -> Option<String> {
        first_line(&lock(&self.stdout)).or_else(|| first_line(&lock(&self.stderr)))
    }

    /// Whether anything other than whitespace has been printed on either stream yet.
    fn has_output(&self) -> bool {
        let printed = |b: &Mutex<Vec<u8>>| lock(b).iter().any(|c| !c.is_ascii_whitespace());
        printed(&self.stdout) || printed(&self.stderr)
    }
}

/// Drains `stream` into `sink` (keeping at most [`MAX_CAPTURE`] bytes) until EOF.
fn capture<R>(stream: Option<R>, sink: Arc<Mutex<Vec<u8>>>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut stream) = stream else {
            return;
        };
        let mut chunk = [0u8; 8192];
        // Stop only at EOF or on a run of errors: dropping the pipe early on a transient error
        // would give the CLI a broken pipe before it printed its version.
        let mut consecutive_errors = 0;
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    consecutive_errors = 0;
                    let mut kept = lock(&sink);
                    let room = MAX_CAPTURE.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
                Err(_) if consecutive_errors < 8 => consecutive_errors += 1,
                Err(_) => break,
            }
        }
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Lines starting with these (case-insensitive) are diagnostics, not the version, even if
/// they contain a version number (`Warning: Node.js v18.0.0 is deprecated`).
const DIAGNOSTIC_PREFIXES: &[&str] = &[
    "warning", "warn", "error", "err!", "deprecat", "note", "notice", "info", "debug", "update",
    "(node:",
];

/// The first line that [`looks_like_version`] and isn't a diagnostic, trimmed.
fn version_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let is_diagnostic = |l: &str| {
        let lower = l.to_ascii_lowercase();
        DIAGNOSTIC_PREFIXES.iter().any(|p| lower.starts_with(p))
    };
    text.lines()
        .map(str::trim)
        .find(|l| looks_like_version(l) && !is_diagnostic(l))
        .map(str::to_owned)
}

/// Whether a line printed by `--version` plausibly is one (CLIs print e.g. `2.1.330 (Claude
/// Code)`, `codex-cli 0.171.0`, `v24.18.1`, `git version 2.45.2`): it contains a dotted number
/// (`<digit>.<digit>`). Guards against reporting an error banner as a healthy tool's version —
/// a bare digit is not enough (`Error: Node.js 18 or newer is required`).
fn looks_like_version(line: &str) -> bool {
    line.as_bytes()
        .windows(3)
        .any(|w| w[0].is_ascii_digit() && w[1] == b'.' && w[2].is_ascii_digit())
}

/// "`x --version` failed: <exit>", plus the CLI's own first line of output (usually the reason,
/// typically from stderr).
fn failure_message(program: &str, exit: ExitStatus, captured: &Captured) -> String {
    let reason =
        first_line(&lock(&captured.stderr)).or_else(|| first_line(&lock(&captured.stdout)));
    match reason {
        Some(line) => format!("`{program} --version` failed: {exit}: {line}"),
        None => format!("`{program} --version` failed: {exit}"),
    }
}

/// The first non-empty line of `bytes` (lossy UTF-8), trimmed.
fn first_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    // Note: these Node helpers must be `detached: true`. libuv puts non-detached children in
    // its own kill-on-close job, so they would die with the parent and not model a real
    // surviving helper/launcher at all (verified empirically).

    /// Review fix: a launcher stub that starts the real binary and exits immediately must not
    /// have the real binary killed before it prints the version.
    /// Review fix: READ_GRACE starts when output first appears, so a late binary that writes
    /// its version in pieces isn't killed mid-line.
    #[tokio::test(flavor = "multi_thread")]
    async fn late_output_written_in_pieces_is_not_truncated() {
        let script = "require('child_process').spawn(process.execPath, ['-e', \
            'setTimeout(() => process.stdout.write(\"claude \"), 2500); \
             setTimeout(() => process.stdout.write(\"1.2.3\" + String.fromCharCode(10)), 2900)'], \
            { stdio: ['ignore', 'inherit', 'ignore'], detached: true }).unref();";
        let status = super::probe("Pieces", "node", &["-e", script]).await;
        assert_eq!(
            status.version.as_deref(),
            Some("claude 1.2.3"),
            "{status:?}"
        );
    }

    /// Review fix: an error banner followed by a hang is not a healthy tool.
    #[tokio::test(flavor = "multi_thread")]
    async fn banner_then_hang_is_not_reported_healthy() {
        let script = "console.log('Error: config file is corrupt'); setInterval(() => {}, 1000);";
        let status = super::probe("Banner", "node", &["-e", script]).await;
        assert_eq!(status.state, super::ToolState::Failed, "{status:?}");
        assert_eq!(status.version, None);
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|e| e.contains("config file is corrupt"))
        );
    }

    #[test]
    fn version_heuristic() {
        for v in [
            "2.1.330 (Claude Code)",
            "codex-cli 0.171.0",
            "v24.18.1",
            "1.4.4",
        ] {
            assert!(super::looks_like_version(v), "{v}");
        }
        assert!(!super::looks_like_version("Error: failed to load config"));
        assert!(!super::looks_like_version(
            "Error: Node.js 18 or newer is required"
        ));
    }

    /// Review fix: on exit 0 the version is the first version-looking line (a warning banner
    /// before it is skipped); only a banner and no version is not a verified tool.
    #[tokio::test(flavor = "multi_thread")]
    async fn exit_zero_picks_the_version_line_not_a_banner() {
        let script =
            "console.log('WARNING: could not update PATH'); console.log('codex-cli 0.171.0');";
        let status = super::probe("Banner0", "node", &["-e", script]).await;
        assert_eq!(
            status.version.as_deref(),
            Some("codex-cli 0.171.0"),
            "{status:?}"
        );

        let script = "console.log('WARNING: could not update PATH');";
        let status = super::probe("OnlyBanner", "node", &["-e", script]).await;
        assert_eq!(status.state, super::ToolState::Failed, "{status:?}");
        assert!(status.error.unwrap().contains("could not update PATH"));
    }

    /// Review fix: real CLIs print errors to stderr; the reason must still be reported.
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_exit_reports_the_reason_printed_to_stderr() {
        let script = "console.error('Error: Node.js 18 or newer is required'); process.exit(1);";
        let status = super::probe("StderrFail", "node", &["-e", script]).await;
        assert_eq!(status.state, super::ToolState::Failed, "{status:?}");
        let err = status.error.unwrap();
        assert!(err.contains("Node.js 18 or newer is required"), "{err}");
    }

    /// Review fix: a warning that contains a version number is not the version.
    #[tokio::test(flavor = "multi_thread")]
    async fn versioned_warning_is_skipped() {
        let script = "console.log('Warning: Node.js v18.0.0 is deprecated'); \
            console.log('codex-cli 0.171.0');";
        let status = super::probe("VersionedWarning", "node", &["-e", script]).await;
        assert_eq!(
            status.version.as_deref(),
            Some("codex-cli 0.171.0"),
            "{status:?}"
        );
    }

    /// Review fix: a failing `--version` reports the CLI's own reason, not just the code.
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_exit_includes_the_clis_message() {
        let script = "console.log('Error: invalid settings.json'); process.exit(3);";
        let status = super::probe("Fails", "node", &["-e", script]).await;
        assert_eq!(status.state, super::ToolState::Failed, "{status:?}");
        let err = status.error.unwrap();
        assert!(
            err.contains("exit code 3") && err.contains("invalid settings.json"),
            "{err}"
        );
    }

    /// Review fix: a CLI that prints its version and then hangs keeps its version.
    #[tokio::test(flavor = "multi_thread")]
    async fn version_printed_before_a_hang_is_kept() {
        let script = "console.log('v7.7.7'); setInterval(() => {}, 1000);";
        let status = super::probe("Hangs", "node", &["-e", script]).await;
        assert_eq!(status.state, super::ToolState::Ok, "{status:?}");
        assert_eq!(status.version.as_deref(), Some("v7.7.7"));
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|e| e.contains("did not exit"))
        );
        assert!(status.path.is_some());
    }

    /// Review fix: a slow real binary (cold start / AV scan) printing after READ_GRACE must
    /// still be reported, since nothing had been printed yet.
    #[tokio::test(flavor = "multi_thread")]
    async fn probe_waits_beyond_grace_for_a_slow_launcher() {
        let script = "require('child_process').spawn(process.execPath, \
            ['-e', 'setTimeout(() => console.log(\"v3.0.0\"), 3000)'], \
            { stdio: ['ignore', 'inherit', 'ignore'], detached: true }).unref();";
        let status = super::probe("SlowLauncher", "node", &["-e", script]).await;
        assert_eq!(status.state, super::ToolState::Ok, "{status:?}");
        assert_eq!(status.version.as_deref(), Some("v3.0.0"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_waits_for_output_from_a_launcher_stubs_real_binary() {
        let script = "require('child_process').spawn(process.execPath, \
            ['-e', 'setTimeout(() => console.log(\"v1.2.3\"), 300)'], \
            { stdio: ['ignore', 'inherit', 'ignore'], detached: true }).unref();";
        let status = super::probe("Launcher", "node", &["-e", script]).await;
        assert_eq!(status.state, super::ToolState::Ok, "{status:?}");
        assert_eq!(status.version.as_deref(), Some("v1.2.3"));
    }

    /// Review fix: a CLI that prints its version and exits while a helper it spawned keeps
    /// stdout open must not be reported as "timed out".
    #[tokio::test(flavor = "multi_thread")]
    async fn probe_finishes_on_exit_even_if_a_helper_holds_stdout() {
        let script = "require('child_process').spawn(process.execPath, \
            ['-e', 'setTimeout(() => {}, 30000)'], { stdio: ['ignore', 'inherit', 'ignore'], detached: true }).unref(); \
            console.log('v9.9.9');";
        let t = Instant::now();
        let status = super::probe("Helper", "node", &["-e", script]).await;
        assert_eq!(status.state, super::ToolState::Ok, "{status:?}");
        assert_eq!(status.version.as_deref(), Some("v9.9.9"));
        assert!(t.elapsed() < super::PROBE_TIMEOUT / 2, "{:?}", t.elapsed());
    }

    /// Runs the real probes. Git is a hard prerequisite of Gantry (worktrees, diffs), so it
    /// must resolve and report a version; the agent CLIs are optional on dev machines.
    #[tokio::test(flavor = "multi_thread")]
    async fn probes_real_tools() {
        let tools = super::probe_all().await;
        for t in &tools {
            eprintln!(
                "{:<12} {:<40} {}",
                t.name,
                t.version.as_deref().or(t.error.as_deref()).unwrap_or(""),
                t.path.as_deref().unwrap_or("-")
            );
        }
        assert_eq!(tools.len(), super::TOOLS.len());
        let git = tools.iter().find(|t| t.program == "git").unwrap();
        assert!(
            git.version
                .as_deref()
                .is_some_and(|v| v.starts_with("git version")),
            "{git:?}"
        );
    }
}
