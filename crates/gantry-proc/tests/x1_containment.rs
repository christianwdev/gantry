//! Spike X1 pass criteria (docs/ROADMAP.md): killing Gantry with `taskkill /F` while it hosts
//! process trees leaves **0 surviving processes** and **0 console windows**; plus the stdio,
//! quoting and shutdown-ladder behaviour the providers rely on.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use gantry_proc::io::{DEFAULT_MAX_LINE, Line, LineReader, StderrTail};
use gantry_proc::{Error, Job, ManagedCommand, Stdio};
use tokio::io::{AsyncWriteExt, BufReader};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

const FIXTURE: &str = env!("CARGO_BIN_EXE_gantry-proc-fixture");

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("gantry-proc-tests").join(format!(
        "{name}-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Process handles opened *before* the kill, so PID reuse cannot fool the survivor check.
struct Watched(Vec<(u32, HANDLE)>);

impl Watched {
    fn open(pids: impl IntoIterator<Item = u32>) -> Self {
        let mut v = Vec::new();
        for pid in pids {
            // SAFETY: plain OpenProcess; closed in Drop.
            let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            assert!(
                !h.is_null(),
                "pid {pid} should be alive and openable before the kill"
            );
            v.push((pid, h));
        }
        Watched(v)
    }

    /// PIDs still alive after `timeout`.
    fn survivors_after(&self, timeout: Duration) -> Vec<u32> {
        let deadline = Instant::now() + timeout;
        let mut alive = Vec::new();
        for &(pid, h) in &self.0 {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u32;
            // SAFETY: valid handle owned by self.
            if unsafe { WaitForSingleObject(h, left) } != WAIT_OBJECT_0 {
                alive.push(pid);
            }
        }
        alive
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        for &(_, h) in &self.0 {
            // SAFETY: each handle was opened by us and is closed exactly once.
            unsafe { CloseHandle(h) };
        }
    }
}

fn read_pid_list(path: &Path) -> Vec<u32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn tree_reports(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("tree-") && n.ends_with(".txt")
        })
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect()
}

async fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn taskkill_force(pid: u32) {
    let status = ManagedCommand::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .spawn()
        .expect("spawn taskkill")
        .wait()
        .await
        .unwrap();
    assert!(status.success(), "taskkill /F failed: {status}");
}

/// Kills a "Gantry" host with `taskkill /F` and proves its whole hosted tree dies with it.
async fn assert_host_kill_leaves_no_survivors(dir: &Path, hosted: &[&str], min_pids: usize) {
    let mut args = vec!["host".to_owned(), dir.to_string_lossy().into_owned()];
    args.extend(hosted.iter().map(|s| (*s).to_owned()));
    let mut host = ManagedCommand::new(FIXTURE).args(&args).spawn().unwrap();
    let job_file = dir.join("host-job.txt");

    // Wait until the hosted tree has fully started and the PID set is stable.
    // "Stable" = unchanged for 1.5 s, so late helpers (Edge's GPU/network/renderer processes,
    // Vite's esbuild service) are included in the survivor check.
    let mut last = BTreeSet::new();
    let mut since = Instant::now();
    let pids = wait_for("hosted process tree", Duration::from_secs(60), || {
        let now: BTreeSet<u32> = read_pid_list(&job_file).into_iter().collect();
        if now != last {
            last = now;
            since = Instant::now();
            return None;
        }
        (last.len() >= min_pids && since.elapsed() >= Duration::from_millis(1500))
            .then(|| last.clone())
    })
    .await;

    eprintln!(
        "host {} is containing {} processes; taskkill /F the host",
        host.id(),
        pids.len()
    );
    let watched = Watched::open(pids.iter().copied().chain([host.id()]));
    taskkill_force(host.id()).await;
    let survivors = watched.survivors_after(Duration::from_secs(5));
    assert!(
        survivors.is_empty(),
        "processes survived taskkill /F of the host: {survivors:?}"
    );
    assert!(
        host.wait_timeout(Duration::from_secs(1))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn x1_taskkill_host_kills_entire_tree_without_consoles_or_breakaway() {
    let dir = scratch_dir("x1-tree");
    let depth = 3; // 4 tree processes; 3 breakaway attempts
    assert_host_kill_leaves_no_survivors(
        &dir,
        &[FIXTURE, "tree", &dir.to_string_lossy(), &depth.to_string()],
        depth + 1,
    )
    .await;

    let reports = tree_reports(&dir);
    assert_eq!(reports.len(), depth + 1, "{reports:?}");
    for r in &reports {
        assert!(
            r.contains("console=null"),
            "a descendant got a console window: {r}"
        );
        assert!(
            !r.contains("escaped"),
            "a descendant broke away from the job: {r}"
        );
    }
    assert_eq!(
        reports
            .iter()
            .filter(|r| r.contains("breakaway=denied"))
            .count(),
        depth
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_last_job_reference_kills_the_tree() {
    let dir = scratch_dir("drop");
    let child = ManagedCommand::new(FIXTURE)
        .args(["tree", &dir.to_string_lossy(), "2"])
        .spawn()
        .unwrap();
    let job = Arc::clone(child.job());
    let pids = wait_for("tree to start", Duration::from_secs(30), || {
        let p = job.process_ids().unwrap();
        (p.len() >= 3 && tree_reports(&dir).len() == 3).then_some(p)
    })
    .await;
    let watched = Watched::open(pids);
    drop(job);
    drop(child);
    assert_eq!(
        watched.survivors_after(Duration::from_secs(5)),
        Vec::<u32>::new()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_job_terminates_and_reports_empty() {
    let job = Arc::new(Job::new().unwrap());
    let mut a = ManagedCommand::new(FIXTURE)
        .arg("sleep")
        .job(Arc::clone(&job))
        .spawn()
        .unwrap();
    let mut b = ManagedCommand::new(FIXTURE)
        .arg("sleep")
        .job(Arc::clone(&job))
        .spawn()
        .unwrap();
    // Windowless console children each get a conhost.exe that Windows adds to the job shortly
    // after launch, so the job holds 2..=4 processes here.
    let active = job.active_processes().unwrap();
    assert!((2..=4).contains(&active), "{active}");
    let ids = job.process_ids().unwrap();
    assert!(ids.contains(&a.id()) && ids.contains(&b.id()), "{ids:?}");
    job.terminate(7).unwrap();
    assert!(job.wait_until_empty(Duration::from_secs(5)).await.unwrap());
    assert_eq!(a.wait().await.unwrap().code(), 7);
    assert_eq!(b.wait().await.unwrap().code(), 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn handle_list_prevents_unrelated_handle_inheritance() {
    // An inheritable pipe that has nothing to do with the child. Without
    // PROC_THREAD_ATTRIBUTE_HANDLE_LIST the child would inherit the write end and the reader
    // would never see EOF while the child lives.
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: out-pointers are valid; handles are closed below.
    assert_ne!(unsafe { CreatePipe(&mut read, &mut write, &sa, 0) }, 0);

    let child = ManagedCommand::new(FIXTURE).arg("sleep").spawn().unwrap();
    // SAFETY: closing our only write handle.
    unsafe { CloseHandle(write) };

    let read_addr = read as usize;
    let eof = tokio::task::spawn_blocking(move || {
        let mut byte = 0u8;
        let mut n = 0u32;
        // SAFETY: valid read handle and 1-byte buffer.
        let ok = unsafe {
            ReadFile(
                read_addr as HANDLE,
                (&raw mut byte).cast::<u8>(),
                1,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: trivial.
        ok == 0 && unsafe { GetLastError() } == ERROR_BROKEN_PIPE
    });
    let got_eof = tokio::time::timeout(Duration::from_secs(3), eof).await;
    drop(child); // kills the sleeper (and would unblock the reader if it had leaked)
    assert!(
        matches!(got_eof, Ok(Ok(true))),
        "unrelated inheritable handle leaked into the child: {got_eof:?}"
    );
    // SAFETY: closing the read end exactly once.
    unsafe { CloseHandle(read) };
}

#[tokio::test(flavor = "multi_thread")]
async fn piped_stdio_survives_heavy_bidirectional_traffic() {
    let mut child = ManagedCommand::new(FIXTURE)
        .arg("echo")
        .stdin(Stdio::Piped)
        .stdout(Stdio::Piped)
        .stderr(Stdio::Piped)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr_lines = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&stderr_lines);
    let tail = StderrTail::spawn(child.stderr.take().unwrap(), 10, move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
    });

    const LINES: usize = 4000; // ~2.4 MB in, ~2 MB of stderr noise out
    let payload = "x".repeat(600);
    let writer = tokio::spawn(async move {
        for i in 0..LINES {
            stdin
                .write_all(format!("{i}:{payload}\n").as_bytes())
                .await
                .unwrap();
        }
        stdin.shutdown().await.unwrap();
        drop(stdin);
    });

    let reader = tokio::spawn(async move {
        let mut r = LineReader::new(stdout, DEFAULT_MAX_LINE);
        let mut n = 0;
        while let Some(line) = r.next_line().await.unwrap() {
            match line {
                Line::Text(t) => assert!(t.starts_with(&format!("echo:{n}:")), "{t:.40}"),
                other => panic!("{other:?}"),
            }
            n += 1;
        }
        n
    });

    let result = tokio::time::timeout(Duration::from_secs(60), async {
        writer.await.unwrap();
        let n = reader.await.unwrap();
        let status = child.wait().await.unwrap();
        (n, status, tail.finished().await)
    })
    .await
    .expect("stdio deadlocked");
    assert_eq!(result.0, LINES);
    assert!(result.1.success());
    assert_eq!(stderr_lines.load(Ordering::Relaxed), LINES);
    assert_eq!(result.2.len(), 10);
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_megabyte_lines_are_framed_or_capped() {
    for (max, expect_text) in [(DEFAULT_MAX_LINE, true), (1024 * 1024, false)] {
        let mut child = ManagedCommand::new(FIXTURE)
            .args(["big", "20000000"])
            .stdout(Stdio::Piped)
            .spawn()
            .unwrap();
        let mut r = LineReader::from_buf_read(BufReader::new(child.stdout.take().unwrap()), max);
        match r.next_line().await.unwrap().unwrap() {
            Line::Text(t) => {
                assert!(expect_text);
                assert_eq!(t.len(), 20_000_000);
            }
            Line::Oversized { len } => {
                assert!(!expect_text);
                assert_eq!(len, 20_000_001);
            }
        }
        assert_eq!(
            r.next_line().await.unwrap(),
            Some(Line::Text("after".into()))
        );
        assert_eq!(r.next_line().await.unwrap(), None);
        assert!(child.wait().await.unwrap().success());
    }
}

async fn output_of(cmd: &mut ManagedCommand) -> String {
    let mut child = cmd.stdout(Stdio::Piped).spawn().unwrap();
    let mut out = String::new();
    tokio::io::AsyncReadExt::read_to_string(child.stdout.as_mut().unwrap(), &mut out)
        .await
        .unwrap();
    child.wait().await.unwrap();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn arguments_round_trip_exactly() {
    let tricky = [
        "",
        "plain",
        "with space",
        r#"quote"inside"#,
        r#"back\"slash-quote"#,
        r"trailing\",
        r"trailing space\ ",
        r"\\server\share\dir\",
        "unicode héllo ✓ 漢字",
        "%PATH%",
        "a\tb",
        "&|<>^",
    ];
    let out = output_of(ManagedCommand::new(FIXTURE).arg("args").args(tricky)).await;
    let expected: String = tricky.iter().map(|a| format!("{a:?}\n")).collect();
    assert_eq!(out.replace("\r\n", "\n"), expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn environment_and_exit_codes() {
    let out = output_of(
        ManagedCommand::new(FIXTURE)
            .args(["env", "GANTRY_X1"])
            .env("gantry_x1", "hi"),
    )
    .await;
    assert_eq!(out.trim(), "hi");
    let out = output_of(
        ManagedCommand::new(FIXTURE)
            .args(["env", "USERPROFILE"])
            .env_remove("UserProfile"),
    )
    .await;
    assert_eq!(out.trim(), "<unset>");
    let out = output_of(
        ManagedCommand::new(FIXTURE)
            .args(["env", "GANTRY_X1"])
            .env_clear(),
    )
    .await;
    assert_eq!(out.trim(), "<unset>");

    let mut child = ManagedCommand::new(FIXTURE)
        .args(["exit", "42"])
        .spawn()
        .unwrap();
    assert_eq!(child.wait().await.unwrap().code(), 42);
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_child_has_no_console_window() {
    let out = output_of(ManagedCommand::new(FIXTURE).arg("console")).await;
    assert_eq!(out.trim(), "null");
}

/// Review fix: shutting down one member of a group job must not kill its siblings, while
/// terminating the group still kills everyone (each child's job is nested under the group).
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_of_one_group_member_spares_its_siblings() {
    let dir = scratch_dir("group");
    let group = Arc::new(Job::new().unwrap());
    let mut victim = ManagedCommand::new(FIXTURE)
        .args(["tree", &dir.to_string_lossy(), "1"])
        .stdin(Stdio::Piped)
        .job(Arc::clone(&group))
        .spawn()
        .unwrap();
    let mut sibling = ManagedCommand::new(FIXTURE)
        .arg("sleep")
        .job(Arc::clone(&group))
        .spawn()
        .unwrap();
    let victim_tree = wait_for("victim tree", Duration::from_secs(30), || {
        let p = victim.job().process_ids().unwrap();
        (tree_reports(&dir).len() == 2).then_some(p)
    })
    .await;
    let watched = Watched::open(victim_tree.iter().copied().filter(|&p| p != sibling.id()));

    let status = victim
        .shutdown(Duration::from_millis(50), Duration::from_millis(150))
        .await
        .unwrap();
    assert_eq!(status.code(), 1);
    assert_eq!(
        watched.survivors_after(Duration::from_secs(5)),
        Vec::<u32>::new()
    );
    assert!(
        sibling.try_wait().unwrap().is_none(),
        "sibling in the shared group was killed by another member's shutdown"
    );

    group.terminate(9).unwrap();
    assert_eq!(sibling.wait().await.unwrap().code(), 9);
    assert!(
        group
            .wait_until_empty(Duration::from_secs(5))
            .await
            .unwrap()
    );
}

/// Review fix: the shutdown ladder must deliver EOF even when stdin was handed to a writer
/// task (and a `StdinWriter` clone is still alive).
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_closes_stdin_owned_by_writer_task() {
    let mut child = ManagedCommand::new(FIXTURE)
        .arg("echo")
        .stdin(Stdio::Piped)
        .stdout(Stdio::Piped)
        .spawn()
        .unwrap();
    let writer = child.stdin_writer(8).unwrap();
    assert!(child.stdin.is_none());
    let mut out = LineReader::new(child.stdout.take().unwrap(), DEFAULT_MAX_LINE);
    writer.send_line("hello").await.unwrap();
    assert_eq!(
        out.next_line().await.unwrap(),
        Some(Line::Text("echo:hello".into()))
    );

    let t = Instant::now();
    let status = child
        .shutdown(Duration::from_millis(100), Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        status.success(),
        "echo should exit cleanly on EOF, got {status}"
    );
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(writer.send_line("too late").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_ladder_prefers_eof_then_kills() {
    // Exits cleanly once stdin closes (step 2).
    let mut echo = ManagedCommand::new(FIXTURE)
        .arg("echo")
        .stdin(Stdio::Piped)
        .spawn()
        .unwrap();
    let t = Instant::now();
    let status = echo
        .shutdown(Duration::from_millis(200), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(status.success());
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());

    // Ignores EOF, so the job is terminated (step 3).
    let mut sleeper = ManagedCommand::new(FIXTURE)
        .arg("sleep")
        .stdin(Stdio::Piped)
        .spawn()
        .unwrap();
    let t = Instant::now();
    let status = sleeper
        .shutdown(Duration::from_millis(100), Duration::from_millis(300))
        .await
        .unwrap();
    assert_eq!(status.code(), 1);
    assert!(t.elapsed() >= Duration::from_millis(300));
}

#[test]
fn refuses_script_shims() {
    let err = ManagedCommand::new("something.cmd").spawn().unwrap_err();
    assert!(
        matches!(err, Error::UnsupportedExecutable { .. }),
        "{err:?}"
    );
    let err = ManagedCommand::new("definitely-not-a-real-program-xyz")
        .spawn()
        .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err:?}");
}

/// The real-world half of X1: Node with detached grandchildren, headless system Edge (whose
/// sandbox creates nested jobs), and the Vite dev server from apps/desktop when installed.
/// Run with `cargo test -p gantry-proc -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches real node / msedge / vite; run explicitly"]
async fn x1_real_world_tree_dies_with_host() {
    let dir = scratch_dir("x1-real");
    let node_script = "const {spawn}=require('child_process');\
        for (let i=0;i<2;i++) spawn(process.execPath,['-e','setInterval(()=>{},1e9)'],{detached:true,stdio:'ignore'}).unref();\
        setInterval(()=>{},1e9);";
    let edge = [
        std::env::var("ProgramFiles(x86)").unwrap_or_default(),
        std::env::var("ProgramFiles").unwrap_or_default(),
    ]
    .iter()
    .map(|p| PathBuf::from(p).join(r"Microsoft\Edge\Application\msedge.exe"))
    .find(|p| p.is_file())
    .expect("system Edge not found");
    let edge_profile = dir.join("edge-profile");
    let edge_profile_arg = format!("--user-data-dir={}", edge_profile.display());
    let edge = edge.to_string_lossy().into_owned();

    let mut hosted: Vec<String> = vec![
        "node".into(),
        "-e".into(),
        node_script.into(),
        ";;".into(),
        edge,
        "--headless=new".into(),
        "--remote-debugging-port=0".into(),
        edge_profile_arg,
        "about:blank".into(),
    ];
    let mut min = 3 + 3; // node ×3, Edge browser + at least 2 helpers
    let vite = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(r"..\..\apps\desktop\node_modules\vite\bin\vite.js");
    if vite.is_file() {
        let app = vite.parent().unwrap().join(r"..\..\..");
        hosted.extend([
            ";;".into(),
            "node".into(),
            vite.to_string_lossy().into_owned(),
            app.to_string_lossy().into_owned(),
            "--port".into(),
            "0".into(),
        ]);
        min += 1;
    } else {
        eprintln!("vite not installed in apps/desktop; skipping the Vite part");
    }
    let hosted: Vec<&str> = hosted.iter().map(String::as_str).collect();
    assert_host_kill_leaves_no_survivors(&dir, &hosted, min).await;
}
