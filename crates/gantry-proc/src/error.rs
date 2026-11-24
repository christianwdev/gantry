use std::ffi::OsString;
use std::path::PathBuf;

/// Errors produced by the process spine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The program could not be found (on the search path, or at the explicit path given).
    #[error("executable `{}` not found", program.display())]
    NotFound { program: OsString },

    /// Only a script shim (`.cmd`, `.bat`, `.ps1`, `.vbs`) was found. Gantry never launches through
    /// `cmd.exe`; callers must point at the real `.exe` (or at `node.exe` + the script).
    #[error("`{}` only resolves to a script shim ({}); a real .exe is required", program.display(), shim.display())]
    ShimOnly { program: OsString, shim: PathBuf },

    /// A drive-relative program path (`C:tool.exe`) was given; it would resolve against the
    /// process's per-drive current directory rather than the command's `current_dir`.
    #[error(
        "refusing drive-relative program path `{}`: use an absolute or plain relative path",
        path.display()
    )]
    DriveRelativePath { path: PathBuf },

    /// A relative program path was given without `current_dir`; it would otherwise resolve
    /// against Gantry's own arbitrary working directory.
    #[error(
        "relative program path `{}` needs an absolute current_dir (or use an absolute path)",
        path.display()
    )]
    RelativeWithoutCwd { path: PathBuf },

    /// `current_dir` was relative; it would resolve against Gantry's own working directory.
    #[error("current_dir `{}` must be an absolute path", path.display())]
    RelativeCurrentDir { path: PathBuf },

    /// An explicit path to a non-executable file type (e.g. `.cmd`, `.bat`, `.ps1`) was given.
    #[error("refusing to launch `{}`: only .exe/.com files may be spawned", path.display())]
    UnsupportedExecutable { path: PathBuf },

    /// An argument, environment entry or path contained an interior NUL.
    #[error("{what} contains an interior NUL character")]
    InteriorNul { what: &'static str },

    /// An environment variable name was empty or contained `=` (after its first character).
    #[error("invalid environment variable name `{}`", name.display())]
    InvalidEnvName { name: OsString },

    /// A process was terminated but did not report exit in time (typically stuck in
    /// uncancellable kernel I/O). It may still hold files open.
    #[error("process {pid} did not exit after being terminated")]
    NotTerminated { pid: u32 },

    /// A Win32 call failed.
    #[error("{op} failed: {source}")]
    Win32 {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },

    /// Other I/O errors.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Captures `GetLastError()` for the named Win32 operation.
    pub(crate) fn last_os(op: &'static str) -> Self {
        Error::Win32 {
            op,
            source: std::io::Error::last_os_error(),
        }
    }
}

impl From<Error> for std::io::Error {
    fn from(err: Error) -> Self {
        match err {
            Error::Io(e) => e,
            Error::Win32 { source, .. } => source,
            // "Only a shim exists" means the launchable executable is missing: callers that
            // check `NotFound` to mean "CLI not installed" should treat it the same way.
            Error::NotFound { .. } | Error::ShimOnly { .. } => {
                std::io::Error::new(std::io::ErrorKind::NotFound, err)
            }
            Error::NotTerminated { .. } => std::io::Error::new(std::io::ErrorKind::TimedOut, err),
            other => std::io::Error::new(std::io::ErrorKind::InvalidInput, other),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
