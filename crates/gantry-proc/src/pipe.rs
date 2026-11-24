//! Child stdio plumbing.
//!
//! Anonymous pipes (`CreatePipe`) cannot do overlapped I/O, so Tokio could only drive them from
//! blocking threads. Instead — like Rust's std does internally — each piped stream is a
//! single-instance named pipe: Gantry keeps the overlapped *server* end (registered with Tokio's
//! IOCP reactor) and the child receives a synchronous, inheritable *client* end.

use std::ffi::{OsStr, c_void};
use std::hash::{BuildHasher, RandomState};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::PWSTR;

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
    // The random suffix only avoids accidental collisions: pipe names are enumerable. What
    // actually keeps other users out is the owner-only DACL below, plus first_pipe_instance
    // (fails if someone pre-created the name). Processes running as the *same* user could
    // still race to connect, but they can already debug Gantry itself.
    let nonce = RandomState::new().hash_one((pid, seq));
    let name = format!(r"\\.\pipe\gantry-{pid}-{seq}-{nonce:016x}");

    let mut sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: owner_only_security_descriptor()?,
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .max_instances(1)
        .access_inbound(direction == Direction::ChildWrites)
        .access_outbound(direction == Direction::ChildReads)
        .in_buffer_size(PIPE_BUFFER)
        .out_buffer_size(PIPE_BUFFER);
    // SAFETY: `sa` is a valid SECURITY_ATTRIBUTES whose descriptor is a process-lifetime static.
    let server =
        unsafe { options.create_with_security_attributes_raw(&name, (&raw mut sa).cast()) }?;

    let access = match direction {
        Direction::ChildReads => GENERIC_READ,
        Direction::ChildWrites => GENERIC_WRITE,
    };
    let client = open_child_end(OsStr::new(&name), access, 0)?;
    // max_instances(1): now that our client holds the only instance, nobody else can attach.
    complete_connect(&server)?;
    Ok((server, client))
}

/// A self-relative security descriptor whose protected DACL grants `GENERIC_ALL` to the current
/// user's SID only. The default named-pipe DACL grants *Everyone* read access, which would let
/// another local user connect to a stdin pipe and read agent prompts.
///
/// Built once and kept for the life of the process (never freed).
fn owner_only_security_descriptor() -> Result<*mut c_void> {
    static SD: OnceLock<std::result::Result<usize, i32>> = OnceLock::new();
    match SD.get_or_init(|| build_owner_only_sd().map_err(|e| e.raw_os_error().unwrap_or(0))) {
        Ok(sd) => Ok(*sd as *mut c_void),
        Err(code) => Err(Error::Win32 {
            op: "build owner-only pipe security descriptor",
            source: std::io::Error::from_raw_os_error(*code),
        }),
    }
}

/// SDDL for the owner-only descriptor: protected (`P`), one allow-ACE, generic all.
pub(crate) fn owner_only_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;{user_sid})")
}

fn build_owner_only_sd() -> std::io::Result<usize> {
    let sid = current_user_sid_string()?;
    let sddl = wide_nul(OsStr::new(&owner_only_sddl(&sid)));
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; on success `sd` receives a LocalAlloc'd descriptor that
    // we intentionally keep for the process lifetime.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(sd as usize)
}

/// The current process token's user SID as a string (`S-1-5-21-...`).
pub(crate) fn current_user_sid_string() -> std::io::Result<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: pseudo-handle for the current process; `token` is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `token` is a valid handle we own.
    let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };
    let raw = token.as_raw_handle() as HANDLE;
    let mut len = 0u32;
    // SAFETY: size query with a null buffer.
    unsafe { GetTokenInformation(raw, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    // u64 storage keeps the TOKEN_USER (which holds pointers) suitably aligned.
    let mut buf: Vec<u64> = vec![0; (len as usize).div_ceil(8).max(1)];
    // SAFETY: `buf` is writable and aligned for at least `len` bytes.
    if unsafe { GetTokenInformation(raw, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation(TokenUser) filled `buf` with a TOKEN_USER whose SID pointer
    // points into the same buffer, which outlives this use.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: PWSTR = std::ptr::null_mut();
    // SAFETY: valid SID; on success `text` is a LocalAlloc'd NUL-terminated string freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `text` is NUL-terminated per the API contract; freed exactly once.
    let s = unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        s
    };
    Ok(s)
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

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    use super::*;

    /// Reads the live DACL of a created pipe back from the kernel: protected, a single ACE for
    /// the current user, and in particular no Everyone (WD) / anonymous (AN) access.
    #[tokio::test]
    async fn stdio_pipes_are_owner_only() {
        let (server, _client) = pipe_pair(Direction::ChildReads).unwrap();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: valid pipe handle; on success `sd` is LocalAlloc'd and freed below.
        let rc = unsafe {
            GetSecurityInfo(
                server.as_raw_handle() as HANDLE,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(rc, 0, "GetSecurityInfo failed");
        let mut text: PWSTR = std::ptr::null_mut();
        // SAFETY: valid descriptor; `text` is LocalAlloc'd and freed below.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0);
        // SAFETY: NUL-terminated string from the API; both allocations freed exactly once.
        let sddl = unsafe {
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            LocalFree(sd.cast());
            s
        };
        let sid = current_user_sid_string().unwrap();
        assert!(sddl.starts_with("D:P("), "{sddl}");
        assert_eq!(sddl.matches("(A;").count(), 1, "{sddl}");
        assert!(sddl.ends_with(&format!(";;;{sid})")), "{sddl}");
        assert!(!sddl.contains(";WD)") && !sddl.contains(";AN)"), "{sddl}");
    }
}
