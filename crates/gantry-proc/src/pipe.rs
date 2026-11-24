//! Child stdio plumbing.
//!
//! Anonymous pipes (`CreatePipe`) cannot do overlapped I/O, so Tokio could only drive them from
//! blocking threads. Instead — like Rust's std does internally — each piped stream is a
//! single-instance named pipe: Gantry keeps the overlapped *server* end (registered with Tokio's
//! IOCP reactor) and the child receives a synchronous, inheritable *client* end.

use std::ffi::OsStr;
use std::hash::{BuildHasher, RandomState};
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use crate::resolve::wide_nul;
use crate::{Error, Result};

const PIPE_BUFFER: u32 = 64 * 1024;

/// Which way data flows, from the child's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// The child reads (stdin).
    ChildReads,
    /// The child writes (stdout / stderr).
    ChildWrites,
}

/// Creates a connected pipe pair: `(server end for Gantry, client end for the child)`.
///
/// Must be called within a Tokio runtime (the server end registers with the reactor).
pub(crate) fn pipe_pair(direction: Direction) -> Result<(NamedPipeServer, OwnedHandle)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Unpredictable suffix so another local process cannot pre-create or squat on the name.
    let nonce = RandomState::new().hash_one((pid, seq));
    let name = format!(r"\\.\pipe\gantry-{pid}-{seq}-{nonce:016x}");

    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .max_instances(1)
        .access_inbound(direction == Direction::ChildWrites)
        .access_outbound(direction == Direction::ChildReads)
        .in_buffer_size(PIPE_BUFFER)
        .out_buffer_size(PIPE_BUFFER)
        .create(&name)?;

    let access = match direction {
        Direction::ChildReads => GENERIC_READ,
        Direction::ChildWrites => GENERIC_WRITE,
    };
    let client = open_child_end(OsStr::new(&name), access, 0)?;
    // max_instances(1): now that our client holds the only instance, nobody else can attach.
    complete_connect(&server)?;
    Ok((server, client))
}

/// Tells the reactor the pipe is connected.
///
/// mio tries to start its background read when the server is registered, gets
/// `ERROR_PIPE_LISTENING` (no client yet) and only retries from `connect`. Because the client
/// is already attached, `ConnectNamedPipe` reports `ERROR_PIPE_CONNECTED` and Tokio's
/// `connect()` resolves on its first poll, so no async context is needed.
fn complete_connect(server: &NamedPipeServer) -> Result<()> {
    let fut = std::pin::pin!(server.connect());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.poll(&mut cx) {
        std::task::Poll::Ready(res) => Ok(res?),
        std::task::Poll::Pending => Err(Error::Io(std::io::Error::other(
            "named pipe was not connected after the client opened it",
        ))),
    }
}

/// Opens the `NUL` device for `Stdio::Null`.
pub(crate) fn null_handle(direction: Direction) -> Result<OwnedHandle> {
    let access = match direction {
        Direction::ChildReads => GENERIC_READ,
        Direction::ChildWrites => GENERIC_WRITE,
    };
    open_child_end(
        OsStr::new("NUL"),
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )
}

/// Opens `path` as a **non-inheritable** handle; `spawn` makes it inheritable only for the
/// instant around `CreateProcessW`, where the handle list limits it to the intended child.
fn open_child_end(path: &OsStr, access: u32, share: u32) -> Result<OwnedHandle> {
    let wide = wide_nul(path);
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 0,
    };
    // SAFETY: `wide` is NUL-terminated and `sa` is a valid SECURITY_ATTRIBUTES.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            share,
            &sa,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(Error::last_os("CreateFileW"));
    }
    // SAFETY: `raw` is a valid handle we exclusively own.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) })
}
