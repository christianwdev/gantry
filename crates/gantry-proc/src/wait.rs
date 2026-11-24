//! Async waiting on Win32 waitable handles via the system thread pool
//! (`RegisterWaitForSingleObject`), so waiting on N processes costs no dedicated threads.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::sync::Mutex;

use tokio::sync::oneshot;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, RegisterWaitForSingleObject, UnregisterWaitEx,
    WT_EXECUTEONLYONCE, WaitForSingleObject,
};

use crate::{Error, Result};

type Slot = Mutex<Option<oneshot::Sender<()>>>;

/// Owns a thread-pool wait registration and its callback context.
struct Registration {
    wait: HANDLE,
    ctx: *mut Slot,
}

// SAFETY: the wait handle and context pointer are only touched by the owner on drop and by the
// thread-pool callback through the Mutex, so moving the owner across threads is sound.
unsafe impl Send for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: `wait` came from RegisterWaitForSingleObject. INVALID_HANDLE_VALUE makes this
        // block until any in-flight callback has returned, after which `ctx` is unreachable
        // from the pool and can be freed.
        let unregistered = unsafe { UnregisterWaitEx(self.wait, INVALID_HANDLE_VALUE) } != 0;
        if unregistered {
            // SAFETY: the wait is gone and no callback can still be running, so we are the
            // only owner of `ctx`.
            drop(unsafe { Box::from_raw(self.ctx) });
        } else {
            // The pool may still hold `ctx` (and could still run the callback): freeing it
            // would be a use-after-free. Leak the few bytes instead.
            tracing::error!(error = %std::io::Error::last_os_error(), "UnregisterWaitEx failed");
        }
    }
}

unsafe extern "system" fn on_signaled(ctx: *mut c_void, _timed_out: bool) {
    // SAFETY: `ctx` is the `Box<Slot>` pointer kept alive by `Registration` until the wait is
    // unregistered (which waits for this callback to finish).
    let slot = unsafe { &*(ctx as *const Slot) };
    if let Some(tx) = slot.lock().ok().and_then(|mut g| g.take()) {
        let _ = tx.send(());
    }
}

/// Resolves once `handle` is signaled. Cancel-safe: dropping the future unregisters the wait.
///
/// Borrowing the handle keeps it open for the life of the future, and keeps the future `Send`
/// (no raw pointers are held across the `.await`).
pub(crate) async fn wait_for_handle(handle: BorrowedHandle<'_>) -> Result<()> {
    let Some((_registration, rx)) = register(handle)? else {
        return Ok(());
    };
    // The sender lives in the Box until the callback fires; it can only be dropped unsent when
    // the registration is torn down, which cannot happen while we await here.
    let _ = rx.await;
    Ok(())
}

/// Registers a one-shot thread-pool wait; `None` if the handle is already signaled.
fn register(handle: BorrowedHandle<'_>) -> Result<Option<(Registration, oneshot::Receiver<()>)>> {
    let raw = handle.as_raw_handle() as HANDLE;
    // SAFETY: `raw` is a valid waitable handle, borrowed for the duration of the wait.
    match unsafe { WaitForSingleObject(raw, 0) } {
        WAIT_OBJECT_0 => return Ok(None),
        WAIT_TIMEOUT => {}
        // WAIT_FAILED (bad handle / missing SYNCHRONIZE): report it rather than waiting forever.
        _ => return Err(Error::last_os("WaitForSingleObject")),
    }
    let (tx, rx) = oneshot::channel();
    let ctx = Box::into_raw(Box::new(Mutex::new(Some(tx))));
    let mut wait: HANDLE = std::ptr::null_mut();
    // SAFETY: `ctx` stays valid until `Registration::drop` unregisters the wait.
    let ok = unsafe {
        RegisterWaitForSingleObject(
            &mut wait,
            raw,
            Some(on_signaled),
            ctx.cast(),
            INFINITE,
            WT_EXECUTEONLYONCE,
        )
    };
    if ok == 0 {
        let err = Error::last_os("RegisterWaitForSingleObject");
        // SAFETY: registration failed, so the pool never saw `ctx`; we still own it.
        drop(unsafe { Box::from_raw(ctx) });
        return Err(err);
    }
    Ok(Some((Registration { wait, ctx }, rx)))
}

/// Returns `true` if a process with `pid` exists and has not exited.
///
/// Note that PIDs are recycled; prefer holding a process handle when identity matters.
pub fn is_process_alive(pid: u32) -> bool {
    // SAFETY: OpenProcess with a plain pid; the returned handle is closed below.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // ERROR_INVALID_PARAMETER means no such process. Anything else (typically
        // ERROR_ACCESS_DENIED for elevated/protected/other-user processes) means it exists.
        // SAFETY: trivial thread-local query, read immediately after the failing call.
        return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
    }
    // SAFETY: valid handle from OpenProcess, closed exactly once.
    unsafe {
        let alive = WaitForSingleObject(handle, 0) != WAIT_OBJECT_0;
        CloseHandle(handle);
        alive
    }
}
