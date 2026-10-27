//! The scripted harness's `verify`, judged without a shell.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;

use super::VERIFY;
use crate::ports::VerifyRunner;
use crate::verify::RawVerifyOutput;

/// Answers as the `verify` the scripted harness writes would, by reading
/// its checks instead of running them. A check is `exit 0`, `exit 1` or
/// `test -f <path>`; any other check, or any other `verify`, is a test bug
/// and panics.
#[derive(Clone, Debug)]
pub struct ScriptedVerify {
    root: PathBuf,
    path: String,
}

impl ScriptedVerify {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            path: "verify".to_owned(),
        }
    }

    /// Reads the `verify` at `path`, relative to the root, instead.
    #[must_use]
    pub fn at(mut self, path: &str) -> Self {
        path.clone_into(&mut self.path);
        self
    }

    fn passes(&self, check: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(check) else {
            return false;
        };
        match text.trim() {
            "exit 0" => true,
            "exit 1" => false,
            line => match line.strip_prefix("test -f ") {
                Some(path) => self.root.join(path).is_file(),
                None => panic!("ScriptedVerify cannot judge {}: {line}", check.display()),
            },
        }
    }

    /// The checks `verify` would run, in the shell's glob order. A glob that
    /// matches nothing stays as it is, so the shell runs `checks/*.sh` once,
    /// as the check `*`, and it fails.
    fn checks(&self, prefix: &str) -> Vec<(String, PathBuf)> {
        let dir = self.root.join("checks");
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "sh"))
            .collect();
        if paths.is_empty() {
            paths.push(dir.join("*.sh"));
        }
        paths.sort();
        paths
            .into_iter()
            .filter_map(|path| {
                let id = path.file_name()?.to_str()?.strip_suffix(".sh")?.to_owned();
                id.starts_with(prefix).then_some((id, path))
            })
            .collect()
    }
}

#[async_trait]
impl VerifyRunner for ScriptedVerify {
    async fn run(&self, args: &[&str], _timeout: Duration) -> RawVerifyOutput {
        let Ok(script) = std::fs::read_to_string(self.root.join(&self.path)) else {
            return RawVerifyOutput {
                missing: true,
                ..RawVerifyOutput::default()
            };
        };
        assert_eq!(
            script, VERIFY,
            "ScriptedVerify reads only the scripted verify"
        );
        let prefix = match args {
            ["--task", task, ..] => format!("{task}."),
            _ => String::new(),
        };
        let mut passed = 0;
        let mut failed = Vec::new();
        for (id, check) in self.checks(&prefix) {
            if self.passes(&check) {
                passed += 1;
            } else {
                failed.push(serde_json::json!({ "id": id, "msg": "failed" }));
            }
        }
        let ok = failed.is_empty();
        let report = serde_json::json!({ "ok": ok, "passed": passed, "failed": failed });
        RawVerifyOutput {
            status: Some(i32::from(!ok)),
            stdout: format!("{report}\n"),
            ..RawVerifyOutput::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(checks: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("verify"), VERIFY).unwrap();
        std::fs::create_dir_all(dir.path().join("checks")).unwrap();
        for (name, body) in checks {
            std::fs::write(dir.path().join("checks").join(name), body).unwrap();
        }
        dir
    }

    async fn report(dir: &Path, args: &[&str]) -> serde_json::Value {
        let raw = ScriptedVerify::new(dir)
            .run(args, Duration::from_secs(1))
            .await;
        serde_json::from_str(raw.stdout.trim()).unwrap()
    }

    #[tokio::test]
    async fn checks_pass_as_the_files_they_test_exist() {
        let dir = project(&[
            ("T000.smoke.sh", "exit 0\n"),
            ("T001.exists.sh", "test -f src/T001.txt\n"),
        ]);
        assert_eq!(report(dir.path(), &["--all"]).await["ok"], false);
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/T001.txt"), "").unwrap();
        let all = report(dir.path(), &["--all"]).await;
        assert_eq!(all["ok"], true);
        assert_eq!(all["passed"], 2);
    }

    #[tokio::test]
    async fn a_task_runs_only_its_own_checks() {
        let dir = project(&[
            ("T001.exists.sh", "exit 1\n"),
            ("T002.exists.sh", "exit 0\n"),
        ]);
        assert_eq!(report(dir.path(), &["--task", "T002"]).await["passed"], 1);
        let own = report(dir.path(), &["--task", "T001"]).await;
        assert_eq!(own["failed"][0]["id"], "T001.exists");
    }

    #[tokio::test]
    async fn without_checks_the_glob_itself_fails_as_in_the_shell() {
        let dir = project(&[]);
        let all = report(dir.path(), &["--all"]).await;
        assert_eq!(all["failed"][0]["id"], "*");
        assert_eq!(report(dir.path(), &["--task", "T001"]).await["ok"], true);
    }

    #[tokio::test]
    #[should_panic(expected = "cannot judge")]
    async fn a_check_it_cannot_read_is_a_test_bug() {
        let dir = project(&[("T001.echo.sh", "echo hi\n")]);
        report(dir.path(), &["--all"]).await;
    }

    #[tokio::test]
    async fn without_verify_it_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let raw = ScriptedVerify::new(dir.path())
            .run(&["--all"], Duration::from_secs(1))
            .await;
        assert!(raw.missing);
    }
}
