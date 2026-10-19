//! The project repository through the `git` command line.

use std::path::{Path, PathBuf};
use std::process::Command;

use tenaz_core::ports::{Vcs, VcsError};
use tenaz_core::util::sha256_hex;

/// A git repository at `root`.
#[derive(Clone, Debug)]
pub struct GitRepo {
    root: PathBuf,
}

const IDENTITY: [&str; 6] = [
    "-c",
    "user.name=Tenaz",
    "-c",
    "user.email=tenaz@localhost",
    "-c",
    "commit.gpgsign=false",
];

impl GitRepo {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    /// Runs git and returns stdout; a non-zero status is an error with stderr.
    fn git(&self, args: &[&str]) -> Result<String, VcsError> {
        let output = self
            .command()
            .args(args)
            .output()
            .map_err(|e| VcsError(format!("git {}: {e}", args.join(" "))))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            Err(VcsError(format!(
                "git {} failed: {}",
                args.join(" "),
                if stderr.trim().is_empty() {
                    stdout.trim()
                } else {
                    stderr.trim()
                }
            )))
        }
    }

    fn ok(&self, args: &[&str]) -> bool {
        self.command()
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn commit_staged(&self, message: &str) -> Result<(), VcsError> {
        if self
            .git(&["diff", "--cached", "--name-only"])?
            .trim()
            .is_empty()
        {
            return Ok(());
        }
        let mut args: Vec<&str> = IDENTITY.to_vec();
        args.extend(["commit", "-q", "-m", message]);
        self.git(&args).map(|_| ())
    }
}

fn diff_args<'a>(
    from: &'a str,
    to: Option<&'a str>,
    stat: bool,
    exclude_agent: bool,
) -> Vec<&'a str> {
    let mut args = vec!["diff"];
    if stat {
        args.push("--stat");
    }
    args.push(from);
    if let Some(to) = to {
        args.push(to);
    }
    if exclude_agent {
        args.extend(["--", ".", ":(exclude).tenaz"]);
    }
    args
}

impl Vcs for GitRepo {
    fn init(&self) -> Result<(), VcsError> {
        self.git(&["init", "-q"]).map(|_| ())
    }

    fn head(&self) -> Result<String, VcsError> {
        self.git(&["rev-parse", "HEAD"])
            .map(|s| s.trim().to_owned())
    }

    fn commit_all(&self, message: &str) -> Result<String, VcsError> {
        self.git(&["add", "-A"])?;
        self.commit_staged(message)?;
        self.head()
    }

    fn commit_paths(&self, paths: &[&str], message: &str) -> Result<(), VcsError> {
        let mut args = vec!["add", "--"];
        args.extend_from_slice(paths);
        self.git(&args)?;
        self.commit_staged(message)
    }

    fn changed_paths(&self) -> Result<Vec<String>, VcsError> {
        let out = self.git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
        let mut paths = Vec::new();
        let mut entries = out.split('\0');
        while let Some(entry) = entries.next() {
            if entry.len() < 4 {
                continue;
            }
            paths.push(entry[3..].to_owned());
            if entry.starts_with('R') || entry.starts_with('C') {
                entries.next(); // the rename source follows
            }
        }
        Ok(paths)
    }

    fn in_head(&self, path: &str) -> bool {
        self.ok(&["cat-file", "-e", &format!("HEAD:{path}")])
    }

    fn head_content(&self, path: &str) -> Option<String> {
        self.git(&["show", &format!("HEAD:{path}")]).ok()
    }

    fn revert_path(&self, path: &str) -> Result<(), VcsError> {
        if self.in_head(path) {
            self.git(&["checkout", "HEAD", "--", path]).map(|_| ())
        } else {
            let target = self.root.join(path);
            let result = if target.is_dir() {
                std::fs::remove_dir_all(&target)
            } else {
                std::fs::remove_file(&target)
            };
            match result {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(VcsError(format!("remove {path}: {e}"))),
            }
        }
    }

    fn reset_hard(&self, sha: &str) -> Result<(), VcsError> {
        self.git(&["reset", "-q", "--hard", sha])?;
        self.git(&["clean", "-q", "-fd"]).map(|_| ())
    }

    fn restore_paths(&self, sha: &str, paths: &[&str]) -> Result<(), VcsError> {
        let mut args = vec!["checkout", sha, "--"];
        args.extend_from_slice(paths);
        self.git(&args)?;
        let mut clean = vec!["clean", "-q", "-fd", "--"];
        clean.extend_from_slice(paths);
        self.git(&clean).map(|_| ())
    }

    fn worktree_fingerprint(&self) -> Result<String, VcsError> {
        let status = self.git(&["status", "--porcelain=v1", "--untracked-files=all"])?;
        let diff = self.git(&["diff", "HEAD"])?;
        Ok(sha256_hex(status + &diff))
    }

    fn diff(&self, from: &str, to: &str, exclude_agent: bool) -> Result<String, VcsError> {
        self.git(&diff_args(from, Some(to), false, exclude_agent))
    }

    fn diff_stat(
        &self,
        from: &str,
        to: Option<&str>,
        exclude_agent: bool,
    ) -> Result<String, VcsError> {
        self.git(&diff_args(from, to, true, exclude_agent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, GitRepo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepo::new(dir.path());
        repo.init().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        repo.commit_all("first").unwrap();
        (dir, repo)
    }

    #[test]
    fn commits_only_when_something_is_staged() {
        let (dir, repo) = repo();
        let head = repo.head().unwrap();
        assert_eq!(repo.commit_all("nothing").unwrap(), head);
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        let next = repo.commit_all("second").unwrap();
        assert_ne!(next, head);
        assert!(repo.in_head("b.txt"));
        assert_eq!(repo.head_content("b.txt").as_deref(), Some("b\n"));
        assert!(!repo.in_head("c.txt"));
    }

    #[test]
    fn changed_paths_and_reverts() {
        let (dir, repo) = repo();
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        std::fs::create_dir_all(dir.path().join("new/dir")).unwrap();
        std::fs::write(dir.path().join("new/dir/c.txt"), "c\n").unwrap();
        let mut changed = repo.changed_paths().unwrap();
        changed.sort();
        assert_eq!(changed, vec!["a.txt", "new/dir/c.txt"]);
        repo.revert_path("a.txt").unwrap();
        repo.revert_path("new/dir/c.txt").unwrap();
        repo.revert_path("missing.txt").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a\n"
        );
        assert!(!dir.path().join("new/dir/c.txt").exists());
        assert_eq!(repo.changed_paths().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn fingerprint_reset_restore_and_diffs() {
        let (dir, repo) = repo();
        let base = repo.head().unwrap();
        let clean = repo.worktree_fingerprint().unwrap();
        std::fs::write(dir.path().join("a.txt"), "edited\n").unwrap();
        assert_ne!(repo.worktree_fingerprint().unwrap(), clean);
        std::fs::create_dir_all(dir.path().join(".tenaz/tasks")).unwrap();
        std::fs::write(dir.path().join(".tenaz/tasks/T001.md"), "card\n").unwrap();
        let second = repo.commit_all("edit").unwrap();
        assert!(repo.diff(&base, &second, true).unwrap().contains("edited"));
        assert!(!repo.diff(&base, &second, true).unwrap().contains("T001"));
        assert!(repo.diff(&base, &second, false).unwrap().contains("T001"));
        assert!(
            repo.diff_stat(&base, Some(&second), true)
                .unwrap()
                .contains("a.txt")
        );
        std::fs::write(dir.path().join(".tenaz/tasks/T002.md"), "new\n").unwrap();
        std::fs::write(dir.path().join(".tenaz/tasks/T001.md"), "changed\n").unwrap();
        repo.restore_paths(&second, &[".tenaz/tasks"]).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".tenaz/tasks/T001.md")).unwrap(),
            "card\n"
        );
        assert!(!dir.path().join(".tenaz/tasks/T002.md").exists());
        std::fs::write(dir.path().join("junk.txt"), "x\n").unwrap();
        repo.reset_hard(&base).unwrap();
        assert_eq!(repo.head().unwrap(), base);
        assert!(!dir.path().join("junk.txt").exists());
        assert_eq!(repo.worktree_fingerprint().unwrap(), clean);
        repo.commit_paths(&["a.txt"], "nothing staged").unwrap();
    }
}
