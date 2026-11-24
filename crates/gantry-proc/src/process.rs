use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Foundation::{
    HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::cmdline::{EnvMap, command_line};
use crate::pipe::{Direction, null_handle, pipe_pair};
use crate::resolve::{find_executable, merged_path, wide_nul};
use crate::wait::wait_for_handle;
use crate::{Error, Job, Result};

/// How long `shutdown` waits for a terminated job to report zero processes.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// How a child's stdio stream is connected.
///
/// There is deliberately no `Inherit`: Gantry is a GUI process with no console to share, and
/// inheriting arbitrary parent handles is exactly what the handle list exists to prevent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Stdio {
    /// Connected to the `NUL` device.
    #[default]
    Null,
    /// Connected to an async pipe exposed on [`ManagedChild`].
    Piped,
}

/// The only sanctioned way to start a child process in Gantry. See the crate docs.
#[derive(Debug, Clone)]
pub struct ManagedCommand {
    program: OsString,
    args: Vec<OsString>,
    env_clear: bool,
    env_ops: Vec<(OsString, Option<OsString>)>,
    cwd: Option<PathBuf>,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
    job: Option<Arc<Job>>,
}

impl ManagedCommand {
    /// `program` is a bare name (resolved on the merged process + registry `PATH`) or a path to
    /// a `.exe`/`.com`.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        ManagedCommand {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            env_clear: false,
            env_ops: Vec::new(),
            cwd: None,
            stdin: Stdio::Null,
            stdout: Stdio::Null,
            stderr: Stdio::Null,
            job: None,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env_ops
            .push((key.as_ref().to_owned(), Some(value.as_ref().to_owned())));
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.env_ops.push((key.as_ref().to_owned(), None));
        self
    }

    /// Starts from an empty environment instead of Gantry's own. A bare program name is then
    /// resolved on Gantry's PATH unless you set `PATH`. Most Windows programs also need
    /// `SystemRoot` (Winsock, crypto), so pass it through explicitly if the child needs it.
    pub fn env_clear(&mut self) -> &mut Self {
        self.env_clear = true;
        self.env_ops.clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_owned());
        self
    }

    pub fn stdin(&mut self, cfg: Stdio) -> &mut Self {
        self.stdin = cfg;
        self
    }

    pub fn stdout(&mut self, cfg: Stdio) -> &mut Self {
        self.stdout = cfg;
        self
    }

    pub fn stderr(&mut self, cfg: Stdio) -> &mut Self {
        self.stderr = cfg;
        self
    }

    /// Also places the child in a group job (e.g. a workspace's), for whole-group teardown and
    /// accounting. The child still gets its own job, nested under the group, so shutting it
    /// down never affects its siblings.
    pub fn job(&mut self, job: Arc<Job>) -> &mut Self {
        self.job = Some(job);
        self
    }

    /// The resolved executable path this command would launch.
    pub fn resolve_program(&self) -> Result<PathBuf> {
        let env = self.build_env();
        find_executable(&self.program, &search_path(&env), self.cwd.as_deref())
    }

    fn build_env(&self) -> EnvMap {
        let mut env = if self.env_clear {
            EnvMap::default()
        } else {
            EnvMap::from_current()
        };
        if !self.env_clear {
            // Start-menu launches inherit a stale PATH; merge in the live registry PATH.
            env.set("PATH".into(), merged_path());
        }
        for (k, v) in &self.env_ops {
            match v {
                Some(v) => env.set(k.clone(), v.clone()),
                None => env.remove(k),
            }
        }
        env
    }

    /// Spawns the child inside its job with `CREATE_NO_WINDOW` and a restricted handle list.
    ///
    /// Must be called within a Tokio runtime when any stream is [`Stdio::Piped`].
    ///
    /// This is synchronous and does blocking work: registry reads (PATH merge), a stat per
    /// candidate file in every PATH directory (slow on unreachable network shares) and
    /// `CreateProcessW`. From latency-sensitive async code, call it via
    /// `tokio::task::spawn_blocking` (blocking-pool threads have the runtime context it needs).
    pub fn spawn(&self) -> Result<ManagedChild> {
        let env = self.build_env();
        let program = find_executable(&self.program, &search_path(&env), self.cwd.as_deref())?;
        let mut cmdline = command_line(&program, &self.args)?;
        let app_name = wide_nul(program.as_os_str());
        let env_block = env.to_block()?;
        let cwd = match &self.cwd {
            Some(dir) => {
                let dir = dunce::simplified(dir);
                let w = wide_nul(dir.as_os_str());
                if w[..w.len() - 1].contains(&0) {
                    return Err(Error::InteriorNul {
                        what: "current directory",
                    });
                }
                Some(w)
            }
            None => None,
        };

        // Every child gets its own job so it can be torn down without touching siblings.
        let job = Arc::new(Job::new()?);

        let (stdin_parent, stdin_child) = stdio_end(self.stdin, Direction::ChildReads)?;
        let (stdout_parent, stdout_child) = stdio_end(self.stdout, Direction::ChildWrites)?;
        let (stderr_parent, stderr_child) = stdio_end(self.stderr, Direction::ChildWrites)?;

        let inherit: [HANDLE; 3] = [
            stdin_child.as_raw_handle() as HANDLE,
            stdout_child.as_raw_handle() as HANDLE,
            stderr_child.as_raw_handle() as HANDLE,
        ];
        // Order matters: the process joins the group first, then its own (empty) job, which
        // Windows therefore nests *under* the group. Terminating or closing the group still
        // kills everything; terminating the child's job kills only its subtree.
        let jobs: Vec<HANDLE> = match &self.job {
            Some(group) => vec![group.raw(), job.raw()],
            None => vec![job.raw()],
        };
        let attrs = AttributeList::new(&inherit, &jobs)?;

        // SAFETY: zero is a valid bit pattern for these plain-old-data structs.
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = inherit[0];
        si.StartupInfo.hStdOutput = inherit[1];
        si.StartupInfo.hStdError = inherit[2];
        si.lpAttributeList = attrs.as_ptr();
        // SAFETY: as above.
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        // The child ends are created non-inheritable and only flipped to inheritable here, for
        // the few microseconds around CreateProcessW (they are closed right after). Foreign
        // spawns elsewhere in the process (std::process inside a dependency, Tauri internals)
        // inherit *every* inheritable handle, so this keeps that leak window as small as
        // Windows allows; our own spawns are already isolated by the handle list.
        for &handle in &inherit {
            // SAFETY: `handle` is a valid handle we own for the duration of this call.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(Error::last_os("SetHandleInformation"));
            }
        }

        let flags = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
        // SAFETY: every pointer is valid for the duration of the call: NUL-terminated
        // application name, mutable NUL-terminated command line, double-NUL environment block,
        // optional NUL-terminated cwd, and a STARTUPINFOEXW whose attribute list (and the
        // handle/job arrays it points at) outlive the call. bInheritHandles must be TRUE for
        // the handle list to apply; the list restricts inheritance to exactly `inherit`.
        let ok = unsafe {
            CreateProcessW(
                app_name.as_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                env_block.as_ptr().cast(),
                cwd.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
                (&raw const si).cast(),
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(Error::last_os("CreateProcessW"));
        }
        drop(attrs);
        // The child now owns its copies; close ours so EOF propagates correctly.
        drop((stdin_child, stdout_child, stderr_child));

        // SAFETY: CreateProcessW returned valid process and thread handles that we now own.
        let (process, _thread) = unsafe {
            (
                OwnedHandle::from_raw_handle(pi.hProcess as RawHandle),
                OwnedHandle::from_raw_handle(pi.hThread as RawHandle),
            )
        };
        tracing::debug!(pid = pi.dwProcessId, program = %program.display(), "spawned managed process");

        Ok(ManagedChild {
            pid: pi.dwProcessId,
            program,
            process,
            job,
            status: None,
            stdin_close: None,
            stdin: stdin_parent.map(ChildStdin),
            stdout: stdout_parent.map(ChildStdout),
            stderr: stderr_parent.map(ChildStderr),
        })
    }
}

/// The PATH used to resolve the program: the child's own, or Gantry's merged PATH when
/// the child's environment has none (e.g. after `env_clear`), matching Rust std's fallback.
fn search_path(env: &EnvMap) -> OsString {
    match env.get(OsStr::new("PATH")) {
        Some(path) => path.to_owned(),
        None => merged_path(),
    }
}

fn stdio_end(cfg: Stdio, direction: Direction) -> Result<(Option<NamedPipeServer>, OwnedHandle)> {
    match cfg {
        Stdio::Null => Ok((None, null_handle(direction)?)),
        Stdio::Piped => {
            let (server, client) = pipe_pair(direction)?;
            Ok((Some(server), client))
        }
    }
}

/// An initialized `PROC_THREAD_ATTRIBUTE_LIST` carrying the handle list and job list.
struct AttributeList<'a> {
    /// Backing storage; never touched again after `ptr` is derived from it (moving the Vec
    /// does not move its heap buffer).
    _buf: Vec<usize>,
    /// Derived via `as_mut_ptr`, since Win32 writes through it.
    ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
    initialized: bool,
    _borrows: std::marker::PhantomData<&'a [HANDLE]>,
}

impl<'a> AttributeList<'a> {
    fn new(handles: &'a [HANDLE], jobs: &'a [HANDLE]) -> Result<Self> {
        let mut size = 0usize;
        // SAFETY: a null list with a size pointer queries the required size (expected to
        // "fail" with ERROR_INSUFFICIENT_BUFFER).
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size) };
        if size == 0 {
            return Err(Error::last_os("InitializeProcThreadAttributeList"));
        }
        let mut buf = vec![0usize; size.div_ceil(size_of::<usize>())];
        let ptr = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        let mut list = AttributeList {
            _buf: buf,
            ptr,
            initialized: false,
            _borrows: std::marker::PhantomData,
        };
        // SAFETY: `ptr` points at least `size` writable, pointer-aligned bytes.
        if unsafe { InitializeProcThreadAttributeList(list.ptr, 2, 0, &mut size) } == 0 {
            return Err(Error::last_os("InitializeProcThreadAttributeList"));
        }
        list.initialized = true;
        // SAFETY: the value arrays are borrowed for 'a, which outlives every use of the list.
        unsafe {
            if UpdateProcThreadAttribute(
                list.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                std::mem::size_of_val(handles),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                return Err(Error::last_os("UpdateProcThreadAttribute(HANDLE_LIST)"));
            }
            if UpdateProcThreadAttribute(
                list.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr().cast(),
                std::mem::size_of_val(jobs),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                return Err(Error::last_os("UpdateProcThreadAttribute(JOB_LIST)"));
            }
        }
        Ok(list)
    }

    fn as_ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.ptr
    }
}

impl Drop for AttributeList<'_> {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: the list was successfully initialized and is deleted exactly once.
            unsafe { DeleteProcThreadAttributeList(self.ptr) };
        }
    }
}

/// A process's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExitStatus(u32);

impl ExitStatus {
    pub fn code(&self) -> u32 {
        self.0
    }

    pub fn success(&self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit code {} (0x{:08X})", self.0, self.0)
    }
}

/// A running (or exited) managed child process.
///
/// Every child has its **own** job (see [`ManagedChild::job`]), nested inside the group job
/// given to [`ManagedCommand::job`] if any. Dropping the `ManagedChild` drops its job's last
/// reference (unless you cloned it), which kills the child and all its descendants — but never
/// siblings in the same group.
#[derive(Debug)]
pub struct ManagedChild {
    pid: u32,
    program: PathBuf,
    process: OwnedHandle,
    job: Arc<Job>,
    status: Option<ExitStatus>,
    stdin_close: Option<oneshot::Sender<()>>,
    /// Raw stdin. Prefer [`ManagedChild::stdin_writer`]: if you `take()` this, the shutdown
    /// ladder can no longer close stdin for you.
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

/// Cloneable, queue-backed handle to a child's stdin, served by a dedicated writer task.
///
/// The task owns the pipe exclusively (no pipe deadlocks from interleaved I/O) and drops it —
/// delivering EOF to the child — when every writer is dropped or when
/// [`ManagedChild::shutdown`] / [`ManagedChild::close_stdin`] asks it to, even mid-write.
#[derive(Debug, Clone)]
pub struct StdinWriter {
    tx: mpsc::Sender<Vec<u8>>,
}

impl StdinWriter {
    /// Queues bytes for the child, waiting while the queue is full.
    ///
    /// Fails with `BrokenPipe` once stdin is closed or the child stopped reading.
    pub async fn send(&self, bytes: impl Into<Vec<u8>>) -> io::Result<()> {
        self.tx
            .send(bytes.into())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "child stdin is closed"))
    }

    /// Queues one NDJSON frame (`line` + `\n`).
    pub async fn send_line(&self, line: &str) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.send(bytes).await
    }
}

impl ManagedChild {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// The resolved executable that was launched.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// This child's own job: exactly this process and its descendants (plus their
    /// `conhost.exe` instances). Terminating it never affects siblings in a group job.
    pub fn job(&self) -> &Arc<Job> {
        &self.job
    }

    /// Moves stdin into a dedicated writer task with a queue of `capacity` frames and returns
    /// a cloneable handle to it. `None` if stdin was not piped or was already taken.
    ///
    /// Must be called within a Tokio runtime.
    pub fn stdin_writer(&mut self, capacity: usize) -> Option<StdinWriter> {
        let mut pipe = self.stdin.take()?;
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(capacity.max(1));
        let (close_tx, mut close_rx) = oneshot::channel::<()>();
        self.stdin_close = Some(close_tx);
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = &mut close_rx => break,
                    frame = rx.recv() => match frame {
                        Some(frame) => frame,
                        None => break,
                    },
                };
                // A full pipe (child not reading) must not block a close request.
                let wrote = tokio::select! {
                    biased;
                    _ = &mut close_rx => break,
                    res = pipe.write_all(&frame) => res.is_ok(),
                };
                if !wrote {
                    break;
                }
            }
            // Dropping `pipe` here closes the server end: the child reads EOF.
        });
        Some(StdinWriter { tx })
    }

    /// Closes stdin (EOF) whether it is still held here or owned by a [`StdinWriter`] task.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
        if let Some(close) = self.stdin_close.take() {
            let _ = close.send(());
        }
    }

    fn raw(&self) -> HANDLE {
        self.process.as_raw_handle() as HANDLE
    }

    fn exit_code(&self) -> Result<u32> {
        let mut code = 0u32;
        // SAFETY: valid process handle with PROCESS_QUERY_INFORMATION from CreateProcessW.
        if unsafe { GetExitCodeProcess(self.raw(), &mut code) } == 0 {
            return Err(Error::last_os("GetExitCodeProcess"));
        }
        Ok(code)
    }

    /// Returns the exit status if the process has exited, without blocking.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        // SAFETY: valid process handle.
        match unsafe { WaitForSingleObject(self.raw(), 0) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => return Ok(None),
            _ => return Err(Error::last_os("WaitForSingleObject")),
        }
        let status = ExitStatus(self.exit_code()?);
        self.status = Some(status);
        Ok(Some(status))
    }

    /// Waits for the process itself to exit (descendants may still be running in the job).
    ///
    /// Cancel-safe.
    pub async fn wait(&mut self) -> Result<ExitStatus> {
        if let Some(status) = self.try_wait()? {
            return Ok(status);
        }
        wait_for_handle(self.process.as_handle()).await?;
        Ok(self
            .try_wait()?
            .expect("process handle signaled, so the process has exited"))
    }

    /// Waits up to `timeout` for the process to exit.
    pub async fn wait_timeout(&mut self, timeout: Duration) -> Result<Option<ExitStatus>> {
        match tokio::time::timeout(timeout, self.wait()).await {
            Ok(status) => status.map(Some),
            Err(_) => Ok(None),
        }
    }

    /// `TerminateProcess` on this process **only**. Descendants keep running inside the job;
    /// use [`ManagedChild::kill_tree`] to kill the whole tree.
    pub fn kill(&mut self) -> Result<()> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        // SAFETY: valid process handle with PROCESS_TERMINATE from CreateProcessW.
        if unsafe { TerminateProcess(self.raw(), 1) } == 0 {
            // Capture before try_wait's own Win32 calls overwrite the thread's last error.
            let err = Error::last_os("TerminateProcess");
            // Only an already-exited process excuses the failure; report the original error
            // even if this follow-up query fails too.
            if !matches!(self.try_wait(), Ok(Some(_))) {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Terminates this child and all its descendants (its own job only; group siblings are
    /// unaffected).
    pub fn kill_tree(&self) -> Result<()> {
        self.job.terminate(1)
    }

    /// The graceful cancellation ladder (research doc §4.2), run *after* the caller has already
    /// sent its protocol-native cancel (`interrupt` / `turn/interrupt` / `session/cancel`):
    ///
    /// 1. wait `eof_after` for a clean exit;
    /// 2. close stdin (EOF) — including a [`StdinWriter`] task's pipe — and wait until
    ///    `kill_after` (measured from the start);
    /// 3. terminate this child's own job (its whole tree, never group siblings) and wait;
    /// 4. in every case, once the main process is gone, reap any descendants still alive in its
    ///    job and wait (up to a few seconds) for the job to empty.
    pub async fn shutdown(
        &mut self,
        eof_after: Duration,
        kill_after: Duration,
    ) -> Result<ExitStatus> {
        let result = self.stop_main_process(eof_after, kill_after).await;
        if let Err(e) = &result {
            // A wait failed mid-ladder (e.g. handle/thread-pool exhaustion): never return with
            // the agent possibly still alive. Force the tree down, then reap below.
            tracing::error!(pid = self.pid, error = %e, "shutdown ladder failed; forcing kill");
            if self.kill_tree().is_err() {
                let _ = self.kill();
            }
        }
        // The main process is gone, but helpers it started (MCP servers, dev servers) may still
        // run in its job, holding ports and files (which would block `git worktree remove`).
        // A finished shutdown means the whole tree is gone.
        // Reaping is best-effort: the main process's exit status is the result, and a reap
        // failure must not turn a clean shutdown into an error (the job still kills the rest
        // when its last handle closes).
        if self.job.active_processes().unwrap_or(1) > 0 {
            tracing::debug!(
                pid = self.pid,
                "shutdown ladder: reaping leftover descendants"
            );
            let reaped = match self.kill_tree() {
                Ok(()) => self.job.wait_until_empty(REAP_TIMEOUT).await,
                Err(e) => Err(e),
            };
            match reaped {
                Ok(true) => {}
                Ok(false) => tracing::warn!(pid = self.pid, "descendants alive after reaping"),
                Err(e) => tracing::warn!(pid = self.pid, error = %e, "failed to reap descendants"),
            }
        }
        result
    }

    async fn stop_main_process(
        &mut self,
        eof_after: Duration,
        kill_after: Duration,
    ) -> Result<ExitStatus> {
        if let Some(status) = self.wait_timeout(eof_after).await? {
            return Ok(status);
        }
        tracing::debug!(pid = self.pid, "shutdown ladder: closing stdin");
        self.close_stdin();
        if let Some(status) = self
            .wait_timeout(kill_after.saturating_sub(eof_after))
            .await?
        {
            return Ok(status);
        }
        tracing::warn!(pid = self.pid, "shutdown ladder: terminating process tree");
        if let Err(tree_err) = self.kill_tree() {
            // Never give up with the agent still alive: fall back to the process itself.
            tracing::error!(pid = self.pid, error = %tree_err, "TerminateJobObject failed");
            self.kill().map_err(|_| tree_err)?;
        }
        self.wait().await
    }
}

macro_rules! pipe_wrapper {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug)]
        pub struct $name(NamedPipeServer);
    };
}

pipe_wrapper!(ChildStdin, "Async writer for a child's stdin.");
pipe_wrapper!(ChildStdout, "Async reader for a child's stdout.");
pipe_wrapper!(ChildStderr, "Async reader for a child's stderr.");

impl AsyncWrite for ChildStdin {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl AsyncRead for ChildStdout {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncRead for ChildStderr {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

/// Compile-time guarantee that the async API can be driven from multi-threaded runtimes
/// (`tokio::spawn`, `tauri::async_runtime::spawn`).
#[cfg(test)]
#[allow(dead_code, clippy::let_underscore_future)]
fn async_api_is_send(child: &mut ManagedChild, job: &Job) {
    fn is_send<T: Send>(_: T) {}
    is_send(child.wait());
    is_send(child.wait_timeout(Duration::ZERO));
    is_send(child.shutdown(Duration::ZERO, Duration::ZERO));
    is_send(job.wait_until_empty(Duration::ZERO));
}
