//! # gantry-proc — Gantry's process spine
//!
//! Every child process Gantry starts (agents, Stage Runners, `next dev`, headless Edge, `git`)
//! goes through [`ManagedCommand`]. Raw `std::process::Command` / `tokio::process::Command` are
//! banned workspace-wide via `clippy.toml`. See `docs/ARCHITECTURE.md` §3.5 and
//! `docs/research/01-runtime-and-protocols.md` §4.
//!
//! What `ManagedCommand` guarantees on Windows:
//!
//! * **Atomic Job Object containment.** The child is created *inside* its [`Job`] via
//!   `PROC_THREAD_ATTRIBUTE_JOB_LIST`, so there is no window in which a fast child can spawn a
//!   grandchild that escapes (the classic `CREATE_SUSPENDED` + `AssignProcessToJobObject` race).
//!   Jobs are `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` without any breakaway flag: when the last
//!   [`Job`] handle closes — including when Gantry itself is killed with `taskkill /F` — the whole
//!   descendant tree dies.
//! * **No console flashes.** `CREATE_NO_WINDOW` on every spawn.
//! * **No handle leakage into managed children.** `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` restricts
//!   inheritance to exactly the child's three stdio handles, so unrelated inheritable handles in
//!   Gantry (other children's pipes, files) never leak into a child spawned here. The reverse
//!   direction has one residual, Windows-imposed window: the handle list requires the child's
//!   stdio ends to be inheritable, so they are flipped inheritable only for the microseconds
//!   around `CreateProcessW`. A *foreign* spawn (raw `std::process` in a dependency) racing that
//!   instant could still inherit them — which is why raw spawns are banned in this workspace.
//! * **Real executables only.** Programs resolve to a real `.exe`/`.com` (searching the process
//!   `PATH` merged with the registry `PATH`, so Start-menu launches see freshly installed CLIs).
//!   `.cmd`/`.bat` shims are rejected to avoid BatBadBut-style argument injection.
//! * **Deadlock-free async stdio.** Piped stdio uses overlapped named pipes driven by Tokio; see
//!   [`io::LineReader`] for bounded, CRLF- and UTF-8-tolerant NDJSON framing.

#[cfg(not(windows))]
compile_error!(
    "gantry-proc currently supports Windows only; macOS/Linux parity is Phase 3 (docs/ROADMAP.md)."
);

mod cmdline;
mod error;
pub mod io;
mod job;
mod paths;
mod pipe;
mod process;
pub mod resolve;
mod wait;

pub use error::{Error, Result};
pub use job::Job;
pub use paths::normalize_path;
pub use process::{
    ChildStderr, ChildStdin, ChildStdout, ExitStatus, ManagedChild, ManagedCommand, Shutdown,
    StdinWriter, Stdio,
};
pub use wait::is_process_alive;
