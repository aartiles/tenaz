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

/// Shell lines that start `sleep 30` in a session of its own, as Node's
/// `spawn(..., { detached: true })` does, and wait until it has written
/// its pid to `pidfile`, which it does once it has left the caller's
/// process group. They exit 1 after about 5 s if it never does. A test
/// that leaves one behind runs in a temporary directory of its own: the
/// tests of one binary share Tenaz's pid, and only the project's
/// directory keeps one test's sweep from stopping another's.
pub fn detached_sleep(pidfile: &Path) -> String {
    let pidfile = pidfile.display();
    format!(
        "perl -MPOSIX -e 'POSIX::setsid() or die; open my $f, \">\", shift or die; print $f $$; close $f; exec @ARGV' '{pidfile}' sleep 30 </dev/null >/dev/null 2>&1 &\n\
         tries=0; while [ ! -s '{pidfile}' ]; do tries=$((tries+1)); [ $tries -gt 100 ] && {{ echo 'the sleep did not detach' >&2; exit 1; }}; sleep 0.05; done\n"
    )
}

/// The pid in `pidfile`, killed when dropped, so a failed test leaves
/// nothing running; `None` when there is no pid to read.
#[derive(Debug)]
pub struct KillOnDrop(Option<u32>);

impl KillOnDrop {
    pub fn read(pidfile: &Path) -> Self {
        let pid = std::fs::read_to_string(pidfile)
            .ok()
            .and_then(|t| t.trim().parse().ok());
        Self(pid)
    }

    /// The pid; a test that needs one fails here without it.
    pub fn pid(&self) -> u32 {
        self.0.expect("a pid was written")
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .stderr(Stdio::null())
                .status();
        }
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
