//! Helpers for tests of the adapters and the crates above them.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes `text` to `path` as an executable, from a child `sh`, for a test
/// that runs it.
///
/// Linux refuses to exec a file that any process holds open for writing
/// ("Text file busy"). Written here with `std::fs::write`, the file is open
/// in the test process for a moment, and a child that another test thread
/// forks in that moment keeps the descriptor until it execs. Written by
/// `sh`, the descriptor is never in the test process.
pub fn write_executable(path: &Path, text: &str) {
    let mut sh = Command::new("sh")
        .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    // A `sh` that failed closes the pipe early: its status says why.
    // Dropped at the end of the statement, so `cat` reads to the end.
    let _ = sh.stdin.take().unwrap().write_all(text.as_bytes());
    assert!(
        sh.wait().unwrap().success(),
        "cannot write {}",
        path.display()
    );
}

/// Whether process `pid` is gone within `limit`: not running, or a zombie
/// only waiting to be reaped (`kill -0` still succeeds on one of those).
/// A number, so that a pid a test failed to read cannot count as gone.
pub fn gone_within(pid: u32, limit: std::time::Duration) -> bool {
    let pid = pid.to_string();
    let started = std::time::Instant::now();
    loop {
        let state = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .expect("ps runs");
        if state.is_empty() || state.starts_with('Z') {
            return true;
        }
        if started.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    use super::*;

    /// Threads that fork without pause until dropped.
    struct Forkers {
        stop: Arc<AtomicBool>,
        threads: Vec<JoinHandle<()>>,
    }

    impl Forkers {
        fn start(count: usize) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let threads = (0..count)
                .map(|_| {
                    let stop = stop.clone();
                    std::thread::spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            let _ = Command::new("true").status();
                        }
                    })
                })
                .collect();
            Self { stop, threads }
        }
    }

    impl Drop for Forkers {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            for thread in self.threads.drain(..) {
                let _ = thread.join();
            }
        }
    }

    /// Other tests fork now and then while one writes its script; here
    /// threads fork without pause. Written with `std::fs::write` instead,
    /// this failed on Linux with "Text file busy" in every run. macOS
    /// does not refuse the exec, so there it passes either way.
    #[test]
    fn an_executable_it_writes_runs_while_other_threads_fork() {
        let dir = tempfile::tempdir().unwrap();
        let _forkers = Forkers::start(4);
        let mut failures = Vec::new();
        for i in 0..200 {
            let path = dir.path().join(format!("script{i}"));
            write_executable(&path, "#!/bin/sh\nexit 0\n");
            match Command::new(&path).status() {
                Ok(status) if status.success() => {}
                other => failures.push(format!("{other:?}")),
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }
}
