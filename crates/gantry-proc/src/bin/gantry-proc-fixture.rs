//! Test fixture for gantry-proc's integration tests (spike X1). Not part of the product.
//!
//! It deliberately behaves like arbitrary third-party programs: it spawns its own descendants
//! with plain `std::process::Command` (outside Gantry's control), tries to break away from its
//! job, and reports what it observed to files the tests read.

// Foreign-program behaviour on purpose: raw std spawns, never waited on.
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::zombie_processes
)]

use std::io::{BufRead, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gantry_proc::{Job, ManagedCommand};
use windows_sys::Win32::System::Console::GetConsoleWindow;
use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("");
    match mode {
        "sleep" => sleep_forever(),
        "tree" => tree(Path::new(&args[1]), args[2].parse().expect("depth")),
        "host" => host(Path::new(&args[1]), &args[2..]),
        "echo" => echo(),
        "args" => {
            for a in &args[1..] {
                println!("{a:?}");
            }
        }
        "env" => println!(
            "{}",
            std::env::var(&args[1]).unwrap_or_else(|_| "<unset>".into())
        ),
        "exit" => std::process::exit(args[1].parse().expect("code")),
        "big" => {
            let n: usize = args[1].parse().expect("bytes");
            let mut out = std::io::stdout().lock();
            out.write_all(&vec![b'b'; n]).unwrap();
            out.write_all(b"\nafter\n").unwrap();
        }
        "console" => println!(
            "{}",
            if has_console_window() {
                "window"
            } else {
                "null"
            }
        ),
        other => {
            eprintln!("unknown fixture mode {other:?}");
            std::process::exit(2);
        }
    }
}

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn has_console_window() -> bool {
    // SAFETY: trivial query with no arguments.
    !unsafe { GetConsoleWindow() }.is_null()
}

fn write_atomic(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// A process tree node: reports, spawns a child node like any foreign program would, and
/// attempts to escape its job with CREATE_BREAKAWAY_FROM_JOB.
fn tree(dir: &Path, depth: u32) -> ! {
    let exe = std::env::current_exe().unwrap();
    let console = if has_console_window() {
        "window"
    } else {
        "null"
    };
    let mut breakaway = String::from("n/a");
    if depth > 0 {
        std::process::Command::new(&exe)
            .args(["tree", &dir.to_string_lossy(), &(depth - 1).to_string()])
            .spawn()
            .expect("spawn tree child");
        breakaway = match std::process::Command::new(&exe)
            .arg("sleep")
            .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
        {
            Ok(child) => format!("escaped:{}", child.id()),
            Err(_) => "denied".into(),
        };
    }
    write_atomic(
        &dir.join(format!("tree-{}.txt", std::process::id())),
        &format!("console={console}\nbreakaway={breakaway}\n"),
    );
    sleep_forever()
}

/// Plays the role of Gantry: spawns each `;;`-separated command through ManagedCommand (one job
/// each), then keeps publishing the union of PIDs in its jobs until it is killed.
fn host(dir: &Path, commands: &[String]) -> ! {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = rt.enter();
    let mut children = Vec::new();
    for cmd in commands.split(|a| a == ";;") {
        let mut c = ManagedCommand::new(&cmd[0]);
        c.args(&cmd[1..]);
        children.push(c.spawn().unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}")));
    }
    let jobs: Vec<Arc<Job>> = children.iter().map(|c| Arc::clone(c.job())).collect();
    let out: PathBuf = dir.join("host-job.txt");
    loop {
        let mut pids: Vec<u32> = jobs.iter().flat_map(|j| j.process_ids().unwrap()).collect();
        pids.sort_unstable();
        pids.dedup();
        let text: Vec<String> = pids.iter().map(u32::to_string).collect();
        write_atomic(&out, &text.join("\n"));
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Echoes stdin lines to stdout while spamming stderr, to provoke pipe-buffer deadlocks.
fn echo() {
    let stdin = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    let noise = "e".repeat(512);
    for line in stdin.lines() {
        let line = line.unwrap();
        writeln!(out, "echo:{line}").unwrap();
        writeln!(err, "{noise}").unwrap();
    }
    out.flush().unwrap();
}
