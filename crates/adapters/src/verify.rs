//! Runs the project's `verify` executable with a time limit.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tenaz_core::ports::VerifyRunner;
use tenaz_core::verify::RawVerifyOutput;
use tokio::io::AsyncReadExt;

/// The project's `verify` (`./verify` unless set with [`Self::at`]), run
/// from the project root with `CI=1`, in a process group of its own that is
/// killed when it ends: a server a check started and left running would
/// hold its port, or verify's output, until the next run.
#[derive(Clone, Debug)]
pub struct VerifyProcess {
    root: PathBuf,
    path: String,
    wrapper: Vec<String>,
}

impl VerifyProcess {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            path: "verify".to_owned(),
            wrapper: Vec::new(),
        }
    }

    /// Starts `verify` through `wrapper` (the sandbox launcher).
    #[must_use]
    pub fn wrapped(mut self, wrapper: Vec<String>) -> Self {
        self.wrapper = wrapper;
        self
    }

    /// Runs the executable at `path`, relative to the root, instead.
    #[must_use]
    pub fn at(mut self, path: &str) -> Self {
        path.clone_into(&mut self.path);
        self
    }
}

#[async_trait]
impl VerifyRunner for VerifyProcess {
    async fn run(&self, args: &[&str], timeout: Duration) -> RawVerifyOutput {
        let path = self.root.join(&self.path);
        if !path.exists() {
            return RawVerifyOutput {
                missing: true,
                ..Default::default()
            };
        }
        if let Ok(metadata) = std::fs::metadata(&path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(permissions.mode() | 0o111);
            let _ = std::fs::set_permissions(&path, permissions);
        }
        // `./verify` from the root, as the contract names it: a relative root
        // joined into the program path would be resolved from the root again.
        let spawned = crate::process::command(
            &self.root,
            &self.wrapper,
            std::path::Path::new(&format!("./{}", self.path)),
            args.iter().map(|a| (*a).to_owned()).collect(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                return RawVerifyOutput {
                    status: None,
                    stderr: format!("cannot run verify: {error}"),
                    ..Default::default()
                };
            }
        };
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let out = tokio::spawn(async move {
            let mut buffer = Vec::new();
            if let Some(mut s) = stdout.take() {
                let _ = s.read_to_end(&mut buffer).await;
            }
            buffer
        });
        let err = tokio::spawn(async move {
            let mut buffer = Vec::new();
            if let Some(mut s) = stderr.take() {
                let _ = s.read_to_end(&mut buffer).await;
            }
            buffer
        });
        let group = child.id();
        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => (status.code(), false),
            Ok(Err(_)) => (None, false),
            Err(_) => (None, true),
        };
        // Before reading to the end: what is left may hold the pipes open.
        crate::process::reap(&mut child, group).await;
        let stdout = String::from_utf8_lossy(&out.await.unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&err.await.unwrap_or_default()).into_owned();
        RawVerifyOutput {
            missing: false,
            timed_out,
            status,
            stdout,
            stderr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::write_executable;

    fn project(script: &str) -> (tempfile::TempDir, VerifyProcess) {
        let dir = tempfile::tempdir().unwrap();
        write_executable(&dir.path().join("verify"), script);
        let verify = VerifyProcess::new(dir.path());
        (dir, verify)
    }

    #[tokio::test]
    async fn runs_verify_with_arguments_and_captures_output() {
        let (_dir, verify) = project(
            "#!/bin/sh\necho \"args: $* CI=$CI NO_COLOR=$NO_COLOR\" >&2\necho progress\necho '{\"ok\":true,\"passed\":1,\"failed\":[]}'\nexit 0\n",
        );
        let raw = verify
            .run(&["--task", "T001"], Duration::from_secs(5))
            .await;
        assert_eq!(raw.status, Some(0));
        assert!(
            raw.stdout
                .ends_with("{\"ok\":true,\"passed\":1,\"failed\":[]}\n")
        );
        assert_eq!(raw.stderr, "args: --task T001 CI=1 NO_COLOR=1\n");
        assert!(!raw.timed_out && !raw.missing);
    }

    /// `tenaz run --project proj` from the directory above it.
    #[tokio::test]
    async fn runs_verify_under_a_relative_root() {
        let here = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&here).unwrap();
        write_executable(
            &dir.path().join("verify"),
            "#!/bin/sh\necho '{\"ok\":true,\"passed\":0,\"failed\":[]}'\n",
        );
        let relative = dir.path().strip_prefix(&here).unwrap();
        assert!(relative.is_relative());
        let raw = VerifyProcess::new(relative)
            .run(&["--all"], Duration::from_secs(5))
            .await;
        assert_eq!(raw.status, Some(0), "{}", raw.stderr);
        assert!(!raw.missing);
    }

    /// A check that leaves a server running, as `npm run dev` started and
    /// never stopped: one holding verify's stdout, one not.
    #[tokio::test]
    async fn kills_what_verify_leaves_running() {
        let (dir, verify) = project(
            "#!/bin/sh\nsleep 30 >/dev/null 2>&1 &\necho $! > quiet.pid\nsleep 30 &\necho $! > loud.pid\necho '{\"ok\":true,\"passed\":1,\"failed\":[]}'\n",
        );
        let started = std::time::Instant::now();
        let raw = verify.run(&["--all"], Duration::from_secs(20)).await;
        assert_eq!(raw.status, Some(0), "{}", raw.stderr);
        assert!(started.elapsed() < Duration::from_secs(10));
        for name in ["quiet.pid", "loud.pid"] {
            let pid = std::fs::read_to_string(dir.path().join(name)).unwrap();
            assert!(
                crate::testing::gone_within(pid.trim(), Duration::from_secs(5)),
                "{name} still running"
            );
        }
    }

    #[tokio::test]
    async fn times_out_and_reports_missing() {
        let (_dir, verify) = project("#!/bin/sh\nsleep 5\n");
        let raw = verify.run(&["--all"], Duration::from_millis(200)).await;
        assert!(raw.timed_out);
        assert_eq!(raw.status, None);
        let empty = tempfile::tempdir().unwrap();
        assert!(
            VerifyProcess::new(empty.path())
                .run(&["--all"], Duration::from_secs(1))
                .await
                .missing
        );
    }
}
