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

/// CLIs probed on startup: (display name, program, version args).
const TOOLS: &[(&str, &str, &[&str])] = &[
    ("Claude Code", "claude", &["--version"]),
    ("Codex", "codex", &["--version"]),
    ("Antigravity", "agy", &["--version"]),
    ("Git", "git", &["--version"]),
    ("Node.js", "node", &["--version"]),
    ("Bun", "bun", &["--version"]),
];

/// Result of probing one external CLI.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    /// Human-readable tool name.
    pub name: String,
    /// The program name that was resolved.
    pub program: String,
    /// The resolved executable, if found.
    pub path: Option<String>,
    /// First line of `--version` output, if the probe succeeded.
    pub version: Option<String>,
    /// Why resolution or the probe failed.
    pub error: Option<String>,
}

pub async fn probe_all() -> Vec<ToolStatus> {
    let probes = TOOLS.iter().map(|&(name, program, args)| {
        tauri::async_runtime::spawn(async move { probe(name, program, args).await })
    });
    let mut out = Vec::with_capacity(TOOLS.len());
    for handle in probes.collect::<Vec<_>>() {
        if let Ok(status) = handle.await {
            out.push(status);
        }
    }
    out
}

async fn probe(name: &str, program: &str, args: &[&str]) -> ToolStatus {
    let mut status = ToolStatus {
        name: name.to_owned(),
        program: program.to_owned(),
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
            status.error = Some(e.to_string());
            return status;
        }
    }

    let run = async {
        let mut child = cmd.spawn()?;
        let mut out = String::new();
        if let Some(stdout) = child.stdout.as_mut() {
            stdout.read_to_string(&mut out).await?;
        }
        let exit = child.wait().await?;
        Ok::<_, gantry_proc::Error>((exit, out))
    };
    match tokio::time::timeout(PROBE_TIMEOUT, run).await {
        Ok(Ok((exit, out))) if exit.success() => {
            status.version = out
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_owned);
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
