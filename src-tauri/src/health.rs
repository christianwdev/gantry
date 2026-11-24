//! Startup health check for the external CLIs Gantry orchestrates (roadmap Phase 1).
//!
//! Every probe goes through `gantry_proc::ManagedCommand`, so it resolves to a real `.exe` on
//! the merged process + registry `PATH`, runs without a console window, and is killed with its
//! whole tree if it hangs past the timeout.

use std::time::Duration;

use gantry_proc::{ManagedCommand, Stdio};
use serde::Serialize;
use specta::Type;
use tokio::io::AsyncReadExt;

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// How long to keep reading stdout after the probed process has exited.
const READ_GRACE: Duration = Duration::from_secs(2);

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

    match cmd.resolve_program() {
        Ok(path) => status.path = Some(path.display().to_string()),
        Err(e) => {
            status.state = match &e {
                gantry_proc::Error::NotFound { .. } => ToolState::Missing,
                gantry_proc::Error::ShimOnly { shim, .. } => {
                    status.path = Some(shim.display().to_string());
                    ToolState::ShimOnly
                }
                _ => ToolState::Failed,
            };
            status.error = Some(e.to_string());
            return status;
        }
    }

    let run = async {
        let mut child = cmd.spawn()?;
        // Drain stdout concurrently (so a full pipe can't stall the child), but finish on the
        // process exiting, not on EOF: a helper that inherited stdout would hold it open.
        let mut stdout = child.stdout.take();
        let reader = tokio::spawn(async move {
            let mut out = Vec::new();
            if let Some(stdout) = stdout.as_mut() {
                let _ = stdout.read_to_end(&mut out).await;
            }
            out
        });
        let exit = child.wait().await?;
        // A --version probe needs no survivors; killing them closes the pipe so the reader
        // gets everything already written, then EOF.
        let _ = child.kill_tree();
        let out = tokio::time::timeout(READ_GRACE, reader)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        Ok::<_, gantry_proc::Error>((exit, String::from_utf8_lossy(&out).into_owned()))
    };
    match tokio::time::timeout(PROBE_TIMEOUT, run).await {
        Ok(Ok((exit, out))) if exit.success() => {
            match out.lines().map(str::trim).find(|l| !l.is_empty()) {
                Some(line) => {
                    status.state = ToolState::Ok;
                    status.version = Some(line.to_owned());
                }
                None => {
                    status.error = Some(format!("`{program} --version` printed no version"));
                }
            }
        }
        Ok(Ok((exit, _))) => status.error = Some(format!("`{program} --version` failed: {exit}")),
        Ok(Err(e)) => status.error = Some(e.to_string()),
        // Dropping the child (and its job) inside `run` killed the probe's process tree.
        Err(_) => status.error = Some(format!("`{program} --version` timed out")),
    }
    status
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    /// Review fix: a CLI that prints its version and exits while a helper it spawned keeps
    /// stdout open must not be reported as "timed out".
    #[tokio::test(flavor = "multi_thread")]
    async fn probe_finishes_on_exit_even_if_a_helper_holds_stdout() {
        let script = "require('child_process').spawn(process.execPath, \
            ['-e', 'setTimeout(() => {}, 30000)'], { stdio: ['ignore', 'inherit', 'ignore'] }).unref(); \
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
