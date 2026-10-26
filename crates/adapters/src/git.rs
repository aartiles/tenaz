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
        self.git_with(args, &[], None)
    }

    /// [`Self::git`] with extra environment and, optionally, stdin.
    fn git_with(
        &self,
        args: &[&str],
        env: &[(&str, &Path)],
        stdin: Option<&str>,
    ) -> Result<String, VcsError> {
        use std::io::Write as _;
        let mut command = self.command();
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        let output = match stdin {
            None => command.output(),
            Some(text) => command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    if let Some(mut pipe) = child.stdin.take() {
                        pipe.write_all(text.as_bytes())?;
                    }
                    child.wait_with_output()
                }),
        }
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

/// One entry of `git worktree list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The branch checked out there, without `refs/heads/`.
    pub branch: Option<String>,
}

/// What to change in a tree before committing it: paths to drop and files to
/// replace with new text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeEdits {
    pub remove: Vec<String>,
    pub replace: Vec<(String, String)>,
}

/// Repository-level operations `tenaz init` and `tenaz finish` use around a
/// run: worktrees, branches and building a commit without a checkout.
impl GitRepo {
    /// The top of the working tree `root` is in, or `None` outside one.
    pub fn toplevel(&self) -> Option<PathBuf> {
        self.git(&["rev-parse", "--show-toplevel"])
            .ok()
            .map(|s| PathBuf::from(s.trim()))
    }

    /// HEAD names a commit.
    pub fn has_commits(&self) -> bool {
        self.ok(&["rev-parse", "--verify", "-q", "HEAD^{commit}"])
    }

    /// The index holds some file under `root`: the directory is part of the
    /// repository, not just inside its working tree (ignored or untracked).
    pub fn tracks_any(&self) -> bool {
        self.git(&["ls-files", "--", "."])
            .is_ok_and(|s| !s.trim().is_empty())
    }

    /// The branch HEAD is on; `None` when detached.
    pub fn current_branch(&self) -> Option<String> {
        self.git(&["symbolic-ref", "--short", "-q", "HEAD"])
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    /// `git status --porcelain=v1` lines (`XY path`), untracked files listed
    /// one by one.
    pub fn status_lines(&self) -> Result<Vec<String>, VcsError> {
        Ok(self
            .git(&["status", "--porcelain=v1", "--untracked-files=all"])?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    pub fn branch_exists(&self, name: &str) -> bool {
        self.ok(&["show-ref", "--verify", "-q", &format!("refs/heads/{name}")])
    }

    /// The full commit id `rev` names.
    pub fn resolve(&self, rev: &str) -> Result<String, VcsError> {
        self.git(&["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])
            .map(|s| s.trim().to_owned())
    }

    /// A new branch `branch` at `base`, checked out in a new worktree at `path`.
    pub fn add_worktree(&self, path: &Path, branch: &str, base: &str) -> Result<(), VcsError> {
        let path = path.to_string_lossy();
        self.git(&["worktree", "add", "-q", "-b", branch, &path, base])
            .map(|_| ())
    }

    /// Removes the worktree at `path`; git refuses when it has changes
    /// unless `force`.
    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<(), VcsError> {
        let path = path.to_string_lossy();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path);
        self.git(&args).map(|_| ())
    }

    pub fn delete_branch(&self, name: &str) -> Result<(), VcsError> {
        self.git(&["branch", "-q", "-D", name]).map(|_| ())
    }

    /// Every worktree of the repository, the main one first.
    pub fn worktrees(&self) -> Result<Vec<Worktree>, VcsError> {
        let out = self.git(&["worktree", "list", "--porcelain"])?;
        let mut list: Vec<Worktree> = Vec::new();
        for line in out.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                list.push(Worktree {
                    path: PathBuf::from(path),
                    branch: None,
                });
            } else if let (Some(branch), Some(last)) =
                (line.strip_prefix("branch "), list.last_mut())
            {
                last.branch = Some(branch.trim_start_matches("refs/heads/").to_owned());
            }
        }
        Ok(list)
    }

    /// The text of `path` at `rev`, or `None` when it has no such file.
    pub fn file_at(&self, rev: &str, path: &str) -> Option<String> {
        self.git(&["show", &format!("{rev}:{path}")]).ok()
    }

    /// The tree id of a commit.
    pub fn tree_of(&self, rev: &str) -> Result<String, VcsError> {
        self.git(&["rev-parse", "--verify", "-q", &format!("{rev}^{{tree}}")])
            .map(|s| s.trim().to_owned())
    }

    /// Commits reachable from `to` and not from `from`.
    pub fn count_commits(&self, from: Option<&str>, to: &str) -> Result<u64, VcsError> {
        let range = from.map_or_else(|| to.to_owned(), |from| format!("{from}..{to}"));
        self.git(&["rev-list", "--count", &range])?
            .trim()
            .parse()
            .map_err(|e| VcsError(format!("git rev-list --count: {e}")))
    }

    /// The oldest commit reachable from `rev` whose message is exactly
    /// `message`, following first parents.
    pub fn find_commit(&self, rev: &str, message: &str) -> Option<String> {
        let out = self
            .git(&["log", "--first-parent", "--format=%H%x00%s", rev])
            .ok()?;
        out.lines()
            .rev()
            .filter_map(|l| l.split_once('\0'))
            .find(|(_, subject)| *subject == message)
            .map(|(sha, _)| sha.to_owned())
    }

    /// The first parent of `rev`, or `None` for a root commit.
    pub fn parent_of(&self, rev: &str) -> Option<String> {
        self.resolve(&format!("{rev}^")).ok()
    }

    /// The tree of `source` with `edits` applied, written through a scratch
    /// index so neither the working tree nor the real index is touched.
    pub fn edited_tree(&self, source: &str, edits: &TreeEdits) -> Result<String, VcsError> {
        let index = std::env::temp_dir().join(format!(
            "tenaz-index-{}-{}",
            std::process::id(),
            sha256_hex(format!("{}{source}", self.root.display()))
        ));
        let env = [("GIT_INDEX_FILE", index.as_path())];
        let result = (|| {
            self.git_with(&["read-tree", source], &env, None)?;
            for path in &edits.remove {
                self.git_with(
                    &["rm", "-r", "-q", "--cached", "--ignore-unmatch", "--", path],
                    &env,
                    None,
                )?;
            }
            for (path, text) in &edits.replace {
                let blob = self.git_with(&["hash-object", "-w", "--stdin"], &env, Some(text))?;
                let info = format!("100644,{},{path}", blob.trim());
                self.git_with(&["update-index", "--add", "--cacheinfo", &info], &env, None)?;
            }
            self.git_with(&["write-tree"], &env, None)
                .map(|s| s.trim().to_owned())
        })();
        let _ = std::fs::remove_file(&index);
        result
    }

    /// A commit of `tree` on `parent` (a root commit without one), made as
    /// Tenaz like the run's commits. Returns its id; no ref moves.
    pub fn commit_tree(
        &self,
        tree: &str,
        parent: Option<&str>,
        message: &str,
    ) -> Result<String, VcsError> {
        let mut args: Vec<&str> = IDENTITY.to_vec();
        args.extend(["commit-tree", tree]);
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        args.extend(["-F", "-"]);
        self.git_with(&args, &[], Some(message))
            .map(|s| s.trim().to_owned())
    }

    /// Points branch `name` at `sha`; an existing branch is moved only
    /// with `force`.
    pub fn set_branch(&self, name: &str, sha: &str, force: bool) -> Result<(), VcsError> {
        let mut args = vec!["branch", "-q"];
        if force {
            args.push("-f");
        }
        args.extend([name, sha]);
        self.git(&args).map(|_| ())
    }
}

impl GitRepo {
    /// A diff that, when it leaves out `.tenaz/`, still shows
    /// `.tenaz/verify`: in an existing project that is the project's verify,
    /// and a reviewer must see a change to it. Git cannot exclude a folder
    /// but one file in it, so the file's diff is appended.
    fn diff_with_verify(
        &self,
        from: &str,
        to: Option<&str>,
        stat: bool,
        exclude_agent: bool,
    ) -> Result<String, VcsError> {
        let mut out = self.git(&diff_args(from, to, stat, exclude_agent))?;
        if exclude_agent {
            let mut args = diff_args(from, to, stat, false);
            args.extend(["--", ".tenaz/verify"]);
            out.push_str(&self.git(&args)?);
        }
        Ok(out)
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
        self.diff_with_verify(from, Some(to), false, exclude_agent)
    }

    fn diff_stat(
        &self,
        from: &str,
        to: Option<&str>,
        exclude_agent: bool,
    ) -> Result<String, VcsError> {
        self.diff_with_verify(from, to, true, exclude_agent)
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
        // An existing project's verify lives under .tenaz/ and stays visible.
        std::fs::write(dir.path().join(".tenaz/verify"), "#!/bin/sh\nexit 0\n").unwrap();
        let with_verify = repo.commit_all("verify").unwrap();
        let shown = repo.diff(&second, &with_verify, true).unwrap();
        assert!(shown.contains("+++ b/.tenaz/verify"), "{shown}");
        assert!(
            repo.diff_stat(&second, Some(&with_verify), true)
                .unwrap()
                .contains(".tenaz/verify")
        );
        assert!(
            !repo
                .diff(&base, &with_verify, true)
                .unwrap()
                .contains("T001")
        );
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

    #[test]
    fn worktrees_branches_and_history() {
        let (dir, repo) = repo();
        let base = repo.head().unwrap();
        assert!(repo.has_commits());
        assert!(repo.current_branch().is_some());
        assert!(repo.status_lines().unwrap().is_empty());
        let path = dir.path().join("wt");
        repo.add_worktree(&path, "tenaz/x", &base).unwrap();
        assert!(repo.branch_exists("tenaz/x"));
        let other = GitRepo::new(&path);
        assert_eq!(other.current_branch().as_deref(), Some("tenaz/x"));
        let listed = repo.worktrees().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[1].branch.as_deref(), Some("tenaz/x"));
        std::fs::write(path.join("b.txt"), "b\n").unwrap();
        let init = other.commit_all("tenaz: init").unwrap();
        std::fs::write(path.join("c.txt"), "c\n").unwrap();
        let last = other.commit_all("tenaz: task").unwrap();
        assert_eq!(repo.find_commit(&last, "tenaz: init"), Some(init.clone()));
        assert_eq!(repo.parent_of(&init), Some(base.clone()));
        assert_eq!(repo.parent_of(&base), None);
        assert_eq!(repo.count_commits(Some(&base), &last).unwrap(), 2);
        assert_eq!(repo.count_commits(None, &last).unwrap(), 3);
        assert_eq!(repo.file_at(&last, "c.txt").as_deref(), Some("c\n"));
        assert_eq!(repo.file_at(&base, "c.txt"), None);
        repo.remove_worktree(&path, false).unwrap();
        assert!(!path.exists());
        repo.delete_branch("tenaz/x").unwrap();
        assert!(!repo.branch_exists("tenaz/x"));
        // The commits are still there for a tree to be built from them.
        assert_eq!(repo.resolve(&last).unwrap(), last);
    }

    #[test]
    fn a_tree_is_edited_and_committed_without_a_checkout() {
        let (dir, repo) = repo();
        let base = repo.head().unwrap();
        std::fs::create_dir_all(dir.path().join(".tenaz")).unwrap();
        std::fs::write(dir.path().join(".tenaz/spec.md"), "spec\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "x\n.tenaz/run.json\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        let run = repo.commit_all("run").unwrap();
        let edits = TreeEdits {
            remove: vec![".tenaz".to_owned(), "missing".to_owned()],
            replace: vec![(".gitignore".to_owned(), "x\n".to_owned())],
        };
        let tree = repo.edited_tree(&run, &edits).unwrap();
        let commit = repo.commit_tree(&tree, Some(&base), "result\n").unwrap();
        assert_eq!(repo.parent_of(&commit), Some(base.clone()));
        assert_eq!(repo.tree_of(&commit).unwrap(), tree);
        assert_eq!(repo.file_at(&commit, ".gitignore").as_deref(), Some("x\n"));
        assert_eq!(repo.file_at(&commit, ".tenaz/spec.md"), None);
        assert_eq!(repo.file_at(&commit, "b.txt").as_deref(), Some("b\n"));
        // Neither HEAD, the index nor the working tree moved.
        assert_eq!(repo.head().unwrap(), run);
        assert!(repo.status_lines().unwrap().is_empty());
        repo.set_branch("result", &commit, false).unwrap();
        assert!(repo.set_branch("result", &base, false).is_err());
        repo.set_branch("result", &base, true).unwrap();
        assert_eq!(repo.resolve("result").unwrap(), base);
        let root = repo.commit_tree(&tree, None, "root\n").unwrap();
        assert_eq!(repo.parent_of(&root), None);
    }
}
