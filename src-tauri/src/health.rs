//! Startup health check for the external CLIs Gantry orchestrates (roadmap Phase 1).
//!
//! Every probe goes through `gantry_proc::ManagedCommand`, so it resolves to a real `.exe` on
//! the merged process + registry `PATH`, runs without a console window, and is killed with its
//! whole tree if it hangs past the timeout.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gantry_proc::{ManagedCommand, Stdio};
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
    cmd.args(args)
        .env("DISABLE_AUTOUPDATER", "1")
        .stdout(Stdio::Piped);

    // PROBE_TIMEOUT covers everything, including resolution.
    let run = async move {
        // Resolution and CreateProcess do blocking registry/filesystem work (a PATH entry on an
        // unreachable network share can stall for tens of seconds), so keep them off the async
        // workers. If we time out meanwhile, the late ManagedChild is dropped -> tree killed.
        let mut child = tokio::task::spawn_blocking(move || cmd.spawn())
            .await
            .map_err(|e| gantry_proc::Error::Io(std::io::Error::other(e)))??;
        // Drain stdout concurrently (so a full pipe can't stall the child) into a shared
        // buffer, but finish on the process exiting, not on EOF: a helper that inherited stdout
        // would hold it open.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&buf);
        let mut stdout = child.stdout.take();
        let mut reader = tokio::spawn(async move {
            let Some(stdout) = stdout.as_mut() else {
                return;
            };
            let mut chunk = [0u8; 8192];
            while let Ok(n @ 1..) = stdout.read(&mut chunk).await {
                sink.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .extend_from_slice(&chunk[..n]);
            }
        });
        let exit = child.wait().await?;
        // After exit: a launcher stub may have handed off to the real binary, which can take a
        // while (cold start, AV scan) to print. So keep waiting while nothing has been printed
        // (bounded by PROBE_TIMEOUT); once there is output, allow READ_GRACE for the rest, then
        // kill the tree, which closes any pipe a lingering helper holds -> EOF.
        let exited_at = Instant::now();
        loop {
            if tokio::time::timeout(POLL, &mut reader).await.is_ok() {
                break;
            }
            let has_output = buf
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .any(|b| !b.is_ascii_whitespace());
            if has_output && exited_at.elapsed() >= READ_GRACE {
                let _ = child.kill_tree();
                let _ = tokio::time::timeout(KILL_GRACE, &mut reader).await;
                break;
            }
        }
        // Don't leave detached helpers running after a probe.
        let _ = child.kill_tree();
        let out =
            String::from_utf8_lossy(&buf.lock().unwrap_or_else(|p| p.into_inner())).into_owned();
        Ok::<_, gantry_proc::Error>((exit, out, child.program().to_path_buf()))
    };
    match tokio::time::timeout(PROBE_TIMEOUT, run).await {
        Ok(Ok((exit, out, ran))) => {
            // Report the executable that actually ran.
            status.path = Some(ran.display().to_string());
            if !exit.success() {
                status.error = Some(format!("`{program} --version` failed: {exit}"));
            } else if let Some(line) = out.lines().map(str::trim).find(|l| !l.is_empty()) {
                status.state = ToolState::Ok;
                status.version = Some(line.to_owned());
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
                _ => {}
            }
            status.error = Some(e.to_string());
        }
        // Dropping the child (and its job) inside `run` killed the probe's process tree.
        Err(_) => status.error = Some(format!("`{program} --version` timed out")),
    }
    status
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    // Note: these Node helpers must be `detached: true`. libuv puts non-detached children in
    // its own kill-on-close job, so they would die with the parent and not model a real
    // surviving helper/launcher at all (verified empirically).

    /// Review fix: a launcher stub that starts the real binary and exits immediately must not
    /// have the real binary killed before it prints the version.
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
