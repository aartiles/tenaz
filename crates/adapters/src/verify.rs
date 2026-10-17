//! Runs the project's `verify` executable with a time limit.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use smr_core::ports::VerifyRunner;
use smr_core::verify::RawVerifyOutput;
use tokio::io::AsyncReadExt;

/// `./verify` at the project root, run with `CI=1`.
#[derive(Clone, Debug)]
pub struct VerifyProcess {
    root: PathBuf,
}

impl VerifyProcess {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait]
impl VerifyRunner for VerifyProcess {
    async fn run(&self, args: &[&str], timeout: Duration) -> RawVerifyOutput {
        let path = self.root.join("verify");
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
        let spawned = tokio::process::Command::new(&path)
            .args(args)
            .current_dir(&self.root)
            .env("CI", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
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
        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => (status.code(), false),
            Ok(Err(_)) => (None, false),
            Err(_) => {
                let _ = child.kill().await;
                (None, true)
            }
        };
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

    fn project(script: &str) -> (tempfile::TempDir, VerifyProcess) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("verify"), script).unwrap();
        let verify = VerifyProcess::new(dir.path());
        (dir, verify)
    }

    #[tokio::test]
    async fn runs_verify_with_arguments_and_captures_output() {
        let (_dir, verify) = project(
            "#!/bin/sh\necho \"args: $* CI=$CI\" >&2\necho progress\necho '{\"ok\":true,\"passed\":1,\"failed\":[]}'\nexit 0\n",
        );
        let raw = verify
            .run(&["--task", "T001"], Duration::from_secs(5))
            .await;
        assert_eq!(raw.status, Some(0));
        assert!(
            raw.stdout
                .ends_with("{\"ok\":true,\"passed\":1,\"failed\":[]}\n")
        );
        assert_eq!(raw.stderr, "args: --task T001 CI=1\n");
        assert!(!raw.timed_out && !raw.missing);
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
