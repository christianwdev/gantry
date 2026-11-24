use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};

use crate::{Error, Result};

/// A Win32 Job Object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and **no** breakaway
/// permission.
///
/// Every process created inside the job — and every descendant those processes create, since
/// breakaway is not allowed — is terminated when the last handle to the job closes. That happens
/// when this value is dropped *or* when the Gantry process dies for any reason (`taskkill /F`,
/// crash, debugger stop), because the kernel closes the handle on process exit.
///
/// A single `Job` may contain many processes; share it with `Arc<Job>` to group e.g. all of a
/// workspace's agents and Stage Runners for ordered teardown.
#[derive(Debug)]
pub struct Job {
    handle: OwnedHandle,
}

impl Job {
    /// Creates an anonymous kill-on-close job.
    pub fn new() -> Result<Self> {
        // SAFETY: null security attributes and name create an anonymous, non-inheritable job.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(Error::last_os("CreateJobObjectW"));
        }
        // SAFETY: `raw` is a freshly created, owned, valid handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
        let job = Job { handle };

        // SAFETY: zero is a valid bit pattern for this plain-old-data struct.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // Deliberately NOT setting JOB_OBJECT_LIMIT_BREAKAWAY_OK / SILENT_BREAKAWAY_OK.
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` is a valid JOBOBJECT_EXTENDED_LIMIT_INFORMATION of the stated size.
        let ok = unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(Error::last_os("SetInformationJobObject"));
        }
        Ok(job)
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }

    /// Terminates every process currently in the job with `exit_code`.
    ///
    /// The job stays usable afterwards (new processes may be added).
    pub fn terminate(&self, exit_code: u32) -> Result<()> {
        // SAFETY: valid job handle.
        if unsafe { TerminateJobObject(self.raw(), exit_code) } == 0 {
            return Err(Error::last_os("TerminateJobObject"));
        }
        Ok(())
    }

    /// Number of processes currently alive in the job (including nested descendants).
    ///
    /// Note: every console program started with `CREATE_NO_WINDOW` gets its own `conhost.exe`,
    /// which Windows places in the same job shortly after launch. A single idle agent therefore
    /// usually counts as 2, and the Resource Governor's "has live background children" check
    /// must compare against a baseline sampled after startup, not against 1.
    pub fn active_processes(&self) -> Result<u32> {
        // SAFETY: zero is a valid bit pattern for this plain-old-data struct.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is writable and correctly sized for this information class.
        let ok = unsafe {
            QueryInformationJobObject(
                self.raw(),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::last_os("QueryInformationJobObject"));
        }
        Ok(info.ActiveProcesses)
    }

    /// PIDs of every process currently alive in the job (including nested descendants).
    pub fn process_ids(&self) -> Result<Vec<u32>> {
        // Layout of JOBOBJECT_BASIC_PROCESS_ID_LIST: two u32 counters, then ULONG_PTR ids.
        let mut capacity = 64usize;
        loop {
            let header_words = 8 / size_of::<usize>(); // the two u32 counters, in usize words
            let mut buf: Vec<usize> = vec![0; header_words + capacity];
            let bytes = (buf.len() * size_of::<usize>()) as u32;
            // SAFETY: `buf` is writable for `bytes` bytes and usize-aligned as the struct requires.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.raw(),
                    JobObjectBasicProcessIdList,
                    buf.as_mut_ptr().cast(),
                    bytes,
                    std::ptr::null_mut(),
                )
            };
            // SAFETY: the buffer starts with two u32 counters that the call (even on
            // ERROR_MORE_DATA) fills in; reading them as u32 is in-bounds and aligned.
            let (assigned, in_list) = unsafe {
                let p = buf.as_ptr().cast::<u32>();
                (*p as usize, *p.add(1) as usize)
            };
            if ok == 0 && assigned <= in_list {
                return Err(Error::last_os("QueryInformationJobObject"));
            }
            if assigned > in_list {
                capacity = assigned + 16;
                continue;
            }
            let ids = &buf[header_words..header_words + in_list];
            return Ok(ids.iter().map(|&id| id as u32).collect());
        }
    }

    /// Polls until the job has no live processes, or `timeout` elapses.
    ///
    /// Returns `true` if the job emptied. Used by ordered workspace teardown before
    /// `git worktree remove` so no process still holds files open.
    pub async fn wait_until_empty(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.active_processes()? == 0 {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
