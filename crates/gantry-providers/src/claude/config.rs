//! How a Claude Code worker is launched.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Claude Code permission modes (`--permission-mode`, `set_permission_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Ask before anything risky (CLI `manual`; reported back as `default`).
    Manual,
    AcceptEdits,
    Plan,
    Auto,
    DontAsk,
    BypassPermissions,
}

impl PermissionMode {
    pub fn as_cli(self) -> &'static str {
        match self {
            PermissionMode::Manual => "manual",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::Auto => "auto",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::BypassPermissions => "bypassPermissions",
        }
    }
}

/// Gantry's local MCP server, injected into the session.
#[derive(Debug, Clone)]
pub struct McpEndpoint {
    pub url: String,
    /// Passed only via the child's environment (`GANTRY_MCP_TOKEN`), never written to disk.
    pub bearer_token: String,
}

/// Resume an existing Claude session.
#[derive(Debug, Clone)]
pub struct Resume {
    pub session_id: String,
    /// Continue under a new session id (`--fork-session`), leaving the original intact.
    pub fork: bool,
}

#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    /// `claude` (resolved on PATH) or a path to `claude.exe`.
    pub program: OsString,
    /// Working directory (must be absolute).
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// `low` | `medium` | `high` | `xhigh` | `max`.
    pub effort: Option<String>,
    pub permission_mode: PermissionMode,
    /// Appended to the system prompt via `--append-system-prompt-file` (never multi-line argv).
    pub append_system_prompt: Option<String>,
    pub mcp: Option<McpEndpoint>,
    /// Also load the user's own MCP servers. Locked default (ARCHITECTURE.md §1): `false`,
    /// i.e. `--strict-mcp-config` so only Gantry's server is loaded.
    pub load_user_mcp_servers: bool,
    /// Extra `--plugin-dir`s (e.g. Gantry's skill pack).
    pub plugin_dirs: Vec<PathBuf>,
    pub resume: Option<Resume>,
    /// Fixed id for a new session (`--session-id`, a UUID).
    pub session_id: Option<String>,
    /// Where per-session temp files (prompt appendix, MCP config) are written.
    pub runtime_dir: PathBuf,
    /// Append every raw frame (both directions) to this file as NDJSON, for diagnostics and
    /// for recording replay fixtures.
    pub raw_log: Option<PathBuf>,
    pub extra_args: Vec<OsString>,
    /// Extra environment variables for the worker.
    pub env: Vec<(OsString, OsString)>,
}

impl ClaudeConfig {
    pub fn new(cwd: impl Into<PathBuf>, runtime_dir: impl Into<PathBuf>) -> Self {
        ClaudeConfig {
            program: "claude".into(),
            cwd: cwd.into(),
            model: None,
            effort: None,
            permission_mode: PermissionMode::Manual,
            append_system_prompt: None,
            mcp: None,
            load_user_mcp_servers: false,
            plugin_dirs: Vec::new(),
            resume: None,
            session_id: None,
            runtime_dir: runtime_dir.into(),
            raw_log: None,
            extra_args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// The CLI arguments, given the temp files that were written for this session.
    pub(crate) fn args(&self, files: &RuntimeFiles) -> Vec<OsString> {
        let mut args: Vec<OsString> = [
            "--print",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
        ]
        .iter()
        .map(Into::into)
        .collect();
        args.push(self.permission_mode.as_cli().into());
        if let Some(model) = &self.model {
            args.extend(["--model".into(), model.into()]);
        }
        if let Some(effort) = &self.effort {
            args.extend(["--effort".into(), effort.into()]);
        }
        if let Some(path) = &files.system_prompt {
            args.extend(["--append-system-prompt-file".into(), path.into()]);
        }
        if let Some(path) = &files.mcp_config {
            args.extend(["--mcp-config".into(), path.into()]);
        }
        if !self.load_user_mcp_servers {
            args.push("--strict-mcp-config".into());
        }
        for dir in &self.plugin_dirs {
            args.extend(["--plugin-dir".into(), dir.into()]);
        }
        if let Some(resume) = &self.resume {
            args.extend(["--resume".into(), (&resume.session_id).into()]);
            if resume.fork {
                args.push("--fork-session".into());
            }
        }
        if let Some(id) = &self.session_id {
            args.extend(["--session-id".into(), id.into()]);
        }
        args.extend(self.extra_args.iter().cloned());
        args
    }
}

/// Per-session temp files, deleted when the session is dropped.
#[derive(Debug, Default)]
pub(crate) struct RuntimeFiles {
    pub system_prompt: Option<PathBuf>,
    pub mcp_config: Option<PathBuf>,
}

impl RuntimeFiles {
    pub fn write(config: &ClaudeConfig, unique: &str) -> std::io::Result<Self> {
        let mut files = RuntimeFiles::default();
        if config.append_system_prompt.is_none() && config.mcp.is_none() {
            return Ok(files);
        }
        std::fs::create_dir_all(&config.runtime_dir)?;
        if let Some(prompt) = &config.append_system_prompt {
            let path = config
                .runtime_dir
                .join(format!("claude-prompt-{unique}.md"));
            std::fs::write(&path, prompt)?;
            files.system_prompt = Some(path);
        }
        if let Some(mcp) = &config.mcp {
            let path = config.runtime_dir.join(format!("claude-mcp-{unique}.json"));
            std::fs::write(&path, mcp_config_json(mcp).to_string())?;
            files.mcp_config = Some(path);
        }
        Ok(files)
    }

    fn paths(&self) -> impl Iterator<Item = &Path> {
        self.system_prompt
            .iter()
            .chain(&self.mcp_config)
            .map(PathBuf::as_path)
    }
}

impl Drop for RuntimeFiles {
    fn drop(&mut self) {
        for path in self.paths() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The env var the MCP config's Authorization header expands at runtime.
pub(crate) const MCP_TOKEN_ENV: &str = "GANTRY_MCP_TOKEN";

fn mcp_config_json(mcp: &McpEndpoint) -> serde_json::Value {
    serde_json::json!({
        "mcpServers": {
            "gantry": {
                "type": "http",
                "url": mcp.url,
                "headers": { "Authorization": format!("Bearer ${{{MCP_TOKEN_ENV}}}") }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_cover_the_launch_contract() {
        let mut config = ClaudeConfig::new(r"C:\w", r"C:\rt");
        config.model = Some("haiku".into());
        config.resume = Some(Resume {
            session_id: "abc".into(),
            fork: true,
        });
        let files = RuntimeFiles {
            system_prompt: Some(r"C:\rt\p.md".into()),
            mcp_config: Some(r"C:\rt\m.json".into()),
        };
        let args: Vec<String> = config
            .args(&files)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let joined = args.join(" ");
        for needle in [
            "--print",
            "--input-format stream-json",
            "--output-format stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool stdio",
            "--permission-mode manual",
            "--model haiku",
            r"--append-system-prompt-file C:\rt\p.md",
            r"--mcp-config C:\rt\m.json",
            "--strict-mcp-config",
            "--resume abc --fork-session",
        ] {
            assert!(joined.contains(needle), "missing `{needle}` in `{joined}`");
        }
        std::mem::forget(files); // the paths don't exist; skip the deleting Drop
    }

    #[test]
    fn mcp_config_references_the_token_env_var_not_the_token() {
        let json = mcp_config_json(&McpEndpoint {
            url: "http://127.0.0.1:1/mcp".into(),
            bearer_token: "secret-token".into(),
        })
        .to_string();
        assert!(!json.contains("secret-token"));
        assert!(json.contains("Bearer ${GANTRY_MCP_TOKEN}"), "{json}");
    }
}
