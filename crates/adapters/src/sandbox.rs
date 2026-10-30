//! The optional OS sandbox (`sandbox` in run.json) around the coding agent
//! and `verify`: they may write only to the project, its git directory, the
//! temporary directories, on macOS the per-user cache, and the configured
//! caches, and never to the files Tenaz itself runs or trusts outside the
//! sandbox (the git hooks and config, the `.git` file of a worktree,
//! `.tenaz/run.json`).
//!
//! A sandboxed program is started as `tenaz sandbox-exec <policy> -- <program>`
//! ([`Policy::wrapper`]), which applies the policy to itself and execs the
//! program, so everything the program starts inherits it. On macOS that is
//! Seatbelt (`sandbox-exec` with a generated profile), on Linux Landlock.
//!
//! Landlock cannot deny a path inside a directory it lets a process write
//! to, so on Linux the protected paths are kept by [`Guard`] instead: Tenaz
//! puts them back after every session and `verify` run, before it runs git
//! again. The guard runs on macOS too, where Seatbelt already denies those
//! writes.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tenaz_core::ports::{
    Harness, HarnessError, HarnessInfo, JobOutcome, JobRunner, JobSpec, PermissionDecision,
    Session, SessionExit, SessionSpec, VerifyRunner,
};
use tenaz_core::project::{LEDGER, Project};
use tenaz_core::session::SessionEvent;
use tenaz_core::verify::RawVerifyOutput;
use tenaz_core::write_scope::WriteScope;

/// The hidden subcommand that applies a policy and execs a program.
pub const SUBCOMMAND: &str = "sandbox-exec";

/// What a sandboxed program may write and read. All paths are absolute and
/// canonical (Seatbelt matches resolved paths: `/tmp` is `/private/tmp`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// Directories it may write to, with everything below them.
    pub write: Vec<PathBuf>,
    /// Paths inside those it may not write, create, remove or rename.
    pub deny_write: Vec<PathBuf>,
    /// Directories it may not read.
    pub deny_read: Vec<PathBuf>,
}

impl Policy {
    /// The policy of a run in `root`: write to the project, its git
    /// directories, the temporary directories, on macOS the per-user cache,
    /// `/dev` and `allow_write`; the git hooks and config, a worktree's
    /// `.git` file and the ledger are protected; `deny_read` is unreadable.
    /// The two lists are comma-separated, with `~` for the home directory;
    /// missing directories are skipped.
    pub fn for_project(root: &Path, allow_write: &str, deny_read: &str) -> io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let mut policy = Self {
            write: vec![root.clone()],
            deny_write: vec![root.join(LEDGER)],
            deny_read: Vec::new(),
        };
        if let Some((git_dir, common_dir)) = git_dirs(&root) {
            policy.deny_write.push(common_dir.join("hooks"));
            policy.deny_write.push(common_dir.join("config"));
            if git_dir != common_dir {
                policy.deny_write.push(git_dir.join("config.worktree"));
            }
            if root.join(".git").is_file() {
                policy.deny_write.push(root.join(".git"));
            }
            policy.write.push(git_dir);
            policy.write.push(common_dir);
        }
        let temp = [std::env::temp_dir(), PathBuf::from("/tmp")];
        let cache = user_cache_dir();
        for dir in temp.iter().chain(&cache).chain([&PathBuf::from("/dev")]) {
            if let Ok(dir) = std::fs::canonicalize(dir) {
                policy.write.push(dir);
            }
        }
        policy.write.extend(existing_dirs(allow_write));
        policy.deny_read.extend(existing_dirs(deny_read));
        policy.write = outermost(policy.write);
        policy.deny_read = outermost(policy.deny_read);
        Ok(policy)
    }

    /// The arguments of `tenaz sandbox-exec` up to `--`.
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for (flag, paths) in [
            ("--write", &self.write),
            ("--deny-write", &self.deny_write),
            ("--deny-read", &self.deny_read),
        ] {
            for path in paths {
                args.push(flag.to_owned());
                args.push(path.display().to_string());
            }
        }
        args
    }

    /// `launcher sandbox-exec <policy> --`: the program and leading
    /// arguments that run a command under this policy, for
    /// [`tenaz_core::util::wrap_command`]. `launcher` is the Tenaz binary.
    pub fn wrapper(&self, launcher: &Path) -> Vec<String> {
        let mut wrapper = vec![launcher.display().to_string(), SUBCOMMAND.to_owned()];
        wrapper.extend(self.args());
        wrapper.push("--".to_owned());
        wrapper
    }

    /// Runs `true` under the policy through `launcher`, so a run that asks
    /// for the sandbox stops before its first session when this machine
    /// cannot give it one.
    pub fn probe(&self, launcher: &Path) -> io::Result<()> {
        let (program, args) =
            tenaz_core::util::wrap_command(&self.wrapper(launcher), Path::new("true"), Vec::new());
        let output = std::process::Command::new(program).args(args).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "the sandbox cannot start a program here: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    /// Applies the policy and replaces this process with `command`; returns
    /// only when that fails.
    pub fn exec(&self, command: &[OsString]) -> io::Error {
        use std::os::unix::process::CommandExt as _;
        let Some((program, args)) = command.split_first() else {
            return io::Error::other("sandbox-exec needs a program to run");
        };
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new(SANDBOX_EXEC)
                .arg("-p")
                .arg(self.seatbelt_profile())
                .arg(program)
                .args(args)
                .exec()
        }
        #[cfg(target_os = "linux")]
        {
            if let Err(error) = self.restrict_self() {
                return error;
            }
            std::process::Command::new(program).args(args).exec()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (program, args);
            io::Error::new(
                io::ErrorKind::Unsupported,
                "the sandbox needs macOS or Linux",
            )
        }
    }

    /// The Seatbelt profile: everything is allowed except writing outside
    /// `write`, writing the protected paths, removing or renaming the
    /// directories that hold them, and reading `deny_read`. A later rule
    /// overrides an earlier one.
    pub fn seatbelt_profile(&self) -> String {
        let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        profile.push_str(&seatbelt_rule("allow file-write*", "subpath", &self.write));
        profile.push_str(&seatbelt_rule(
            "deny file-write*",
            "subpath",
            &self.deny_write,
        ));
        // A protected path could otherwise be swapped by renaming a directory above it.
        let mut holders: Vec<PathBuf> = Vec::new();
        for path in &self.deny_write {
            for dir in path.ancestors().skip(1) {
                if self.write.iter().any(|w| dir.starts_with(w))
                    && !holders.iter().any(|h| h == dir)
                {
                    holders.push(dir.to_path_buf());
                }
            }
        }
        profile.push_str(&seatbelt_rule(
            "deny file-write-unlink",
            "literal",
            &holders,
        ));
        profile.push_str(&seatbelt_rule(
            "deny file-read*",
            "subpath",
            &self.deny_read,
        ));
        profile
    }

    /// Restricts this process (and what it starts) with Landlock: read
    /// everything but `deny_read`, write only `write`. The protected paths
    /// are left to [`Guard`]. Fails on a kernel without Landlock.
    #[cfg(target_os = "linux")]
    fn restrict_self(&self) -> io::Result<()> {
        use landlock::{
            ABI, Access as _, AccessFs, Ruleset, RulesetAttr as _, RulesetCreatedAttr as _,
            RulesetStatus, path_beneath_rules,
        };
        let abi = ABI::V5;
        let (readable, listable) = read_rules(Path::new("/"), &self.deny_read);
        let status = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .and_then(landlock::Ruleset::create)
            .and_then(|r| r.add_rules(path_beneath_rules(&readable, AccessFs::from_read(abi))))
            .and_then(|r| r.add_rules(path_beneath_rules(&listable, AccessFs::ReadDir)))
            .and_then(|r| r.add_rules(path_beneath_rules(&self.write, AccessFs::from_all(abi))))
            .and_then(landlock::RulesetCreated::restrict_self)
            .map_err(|e| io::Error::other(format!("cannot set up Landlock: {e}")))?;
        if status.ruleset == RulesetStatus::NotEnforced {
            return Err(io::Error::other(
                "this kernel does not enforce Landlock (Linux 5.13 or later with Landlock enabled)",
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

fn seatbelt_rule(rule: &str, filter: &str, paths: &[PathBuf]) -> String {
    use std::fmt::Write as _;
    if paths.is_empty() {
        return String::new();
    }
    let mut text = format!("({rule}");
    for path in paths {
        let quoted = path
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let _ = write!(text, "\n  ({filter} \"{quoted}\")");
    }
    text.push_str(")\n");
    text
}

/// What Landlock lets a process read when `deny` must stay unreadable:
/// whole trees that hold no denied path, and the directories on the way to
/// one, which are only listed (Landlock grants by tree, so a denied
/// directory's names stay visible, not its files).
#[cfg(any(test, target_os = "linux"))]
fn read_rules(top: &Path, deny: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    fn walk(
        dir: &Path,
        deny: &[PathBuf],
        readable: &mut Vec<PathBuf>,
        listable: &mut Vec<PathBuf>,
    ) {
        if deny.iter().any(|d| d == dir) {
            return;
        }
        if !deny.iter().any(|d| d.starts_with(dir)) {
            readable.push(dir.to_path_buf());
            return;
        }
        listable.push(dir.to_path_buf());
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                walk(&entry.path(), deny, readable, listable);
            }
        }
    }
    let (mut readable, mut listable) = (Vec::new(), Vec::new());
    walk(top, deny, &mut readable, &mut listable);
    (readable, listable)
}

/// The per-user cache directory of macOS (`getconf DARWIN_USER_CACHE_DIR`,
/// the `C` beside the temporary `T`), where Metal keeps its shader cache.
/// Without it every program that draws with Metal compiles its shaders
/// again on each start: Blender took 11 s instead of 0.9 s for a small
/// render. `None` elsewhere, or when `getconf` does not answer.
#[cfg(target_os = "macos")]
fn user_cache_dir() -> Option<PathBuf> {
    // By absolute path: whatever it prints becomes a write root.
    let output = std::process::Command::new("/usr/bin/getconf")
        .arg("DARWIN_USER_CACHE_DIR")
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    cache_dir_from(&output.stdout)
}

#[cfg(not(target_os = "macos"))]
fn user_cache_dir() -> Option<PathBuf> {
    None
}

/// The cache directory `getconf` printed, when it has the shape macOS gives
/// it: `/var/folders/<xx>/<user>/C`, with plain names only (no `..`).
/// Anything else is not trusted as a write root.
#[cfg(any(target_os = "macos", test))]
fn cache_dir_from(stdout: &[u8]) -> Option<PathBuf> {
    let dir = PathBuf::from(std::str::from_utf8(stdout).ok()?.trim());
    let rest = ["/var/folders", "/private/var/folders"]
        .iter()
        .find_map(|base| dir.strip_prefix(base).ok())?;
    let names: Vec<_> = rest.components().collect();
    let plain = names
        .iter()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    (plain && names.len() == 3 && rest.ends_with("C")).then_some(dir)
}

/// Where a run of `root` without the sandbox may write, for the check of
/// its write tools (ADR 0008): what the sandbox would allow, resolved, and
/// the project, the temporary directories and the per-user cache as written
/// too, since a session names them as it is told them (`/tmp`, not
/// `/private/tmp`).
pub fn write_scope(root: &Path, allow_write: &str) -> io::Result<WriteScope> {
    let policy = Policy::for_project(root, allow_write, "")?;
    let mut others = policy.write;
    others.extend([std::env::temp_dir(), PathBuf::from("/tmp")]);
    others.extend(user_cache_dir());
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Ok(WriteScope::new(&std::path::absolute(root)?, home, others))
}

/// `(git dir, common dir)` of the repository at `root`, canonical; they
/// differ in a linked worktree.
fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines().map(|l| std::fs::canonicalize(l.trim()).ok());
    Some((lines.next()??, lines.next()??))
}

/// The existing directories of a comma-separated list, `~` expanded, canonical.
fn existing_dirs(list: &str) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    list.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .filter_map(|item| match (item.strip_prefix("~/"), &home) {
            (Some(rest), Some(home)) => Some(home.join(rest)),
            (Some(_), None) => None,
            (None, _) if item == "~" => home.clone(),
            (None, _) => Some(PathBuf::from(item)),
        })
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .filter(|path| path.is_dir())
        .collect()
}

/// `paths` without duplicates and without the ones inside another.
fn outermost(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let all = paths.clone();
    paths.retain(|p| !all.iter().any(|q| q != p && p.starts_with(q)));
    paths
}

// ---------- the guard ----------

/// One entry under a protected path, keyed by its path relative to it
/// (empty for the path itself).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    Dir,
    File { bytes: Vec<u8>, mode: u32 },
    Link(PathBuf),
}

/// A protected path as it was; empty when it did not exist.
type Snapshot = BTreeMap<PathBuf, Entry>;

/// Keeps the protected paths of a [`Policy`] as they were when the run
/// started, and `run.json` as Tenaz last saved it.
#[derive(Debug)]
pub struct Guard {
    watched: Vec<(PathBuf, Snapshot)>,
    ledger: PathBuf,
    project: Project,
}

impl Guard {
    pub fn new(policy: &Policy, project: &Project) -> Self {
        Self {
            watched: policy
                .deny_write
                .iter()
                .map(|path| (path.clone(), snapshot(path)))
                .collect(),
            ledger: std::fs::canonicalize(project.ledger_path())
                .unwrap_or_else(|_| project.ledger_path()),
            project: project.clone(),
        }
    }

    /// Puts back every protected path that `who` changed, logging each one;
    /// returns them.
    pub fn restore(&self, who: &str) -> Vec<PathBuf> {
        let mut restored = Vec::new();
        for (path, before) in &self.watched {
            let expected = match self.project.last_saved_ledger() {
                Some(text) if *path == self.ledger => {
                    let mode = match before.get(Path::new("")) {
                        Some(Entry::File { mode, .. }) => *mode,
                        _ => 0o644,
                    };
                    BTreeMap::from([(
                        PathBuf::new(),
                        Entry::File {
                            bytes: text.into_bytes(),
                            mode,
                        },
                    )])
                }
                _ => before.clone(),
            };
            if snapshot(path) == expected {
                continue;
            }
            match put_back(path, &expected) {
                Ok(()) => tracing::warn!(
                    "{who} changed {}, which the sandbox protects; Tenaz put it back",
                    path.display()
                ),
                Err(error) => tracing::error!(
                    "{who} changed {}, which the sandbox protects, and Tenaz cannot put it back: {error}",
                    path.display()
                ),
            }
            restored.push(path.clone());
        }
        restored
    }
}

fn snapshot(path: &Path) -> Snapshot {
    fn add(base: &Path, relative: &Path, out: &mut Snapshot) {
        use std::os::unix::fs::PermissionsExt as _;
        // `join("")` would add a trailing slash, which fails on a file.
        let path = if relative.as_os_str().is_empty() {
            base.to_path_buf()
        } else {
            base.join(relative)
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            return;
        };
        if meta.file_type().is_symlink() {
            if let Ok(target) = std::fs::read_link(&path) {
                out.insert(relative.to_path_buf(), Entry::Link(target));
            }
        } else if meta.is_dir() {
            out.insert(relative.to_path_buf(), Entry::Dir);
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    add(base, &relative.join(entry.file_name()), out);
                }
            }
        } else if let Ok(bytes) = std::fs::read(&path) {
            let mode = meta.permissions().mode() & 0o7777;
            out.insert(relative.to_path_buf(), Entry::File { bytes, mode });
        }
    }
    let mut out = Snapshot::new();
    add(path, Path::new(""), &mut out);
    out
}

/// Replaces whatever is at `path` with `snapshot`.
fn put_back(path: &Path, snapshot: &Snapshot) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // Sorted keys put a directory before what it holds.
    for (relative, entry) in snapshot {
        let target = if relative.as_os_str().is_empty() {
            path.to_path_buf()
        } else {
            path.join(relative)
        };
        match entry {
            Entry::Dir => std::fs::create_dir_all(&target)?,
            Entry::File { bytes, mode } => {
                std::fs::write(&target, bytes)?;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(*mode))?;
            }
            Entry::Link(to) => std::os::unix::fs::symlink(to, &target)?,
        }
    }
    Ok(())
}

/// A harness whose sessions end with [`Guard::restore`].
pub struct GuardedHarness {
    inner: Arc<dyn Harness>,
    guard: Arc<Guard>,
}

impl GuardedHarness {
    pub fn new(inner: Arc<dyn Harness>, guard: Arc<Guard>) -> Self {
        Self { inner, guard }
    }
}

impl std::fmt::Debug for GuardedHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedHarness")
            .field("info", &self.inner.info())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Harness for GuardedHarness {
    fn info(&self) -> HarnessInfo {
        self.inner.info()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        let label = spec.label.clone();
        let inner = self.inner.open(spec).await?;
        Ok(Box::new(GuardedSession {
            inner,
            guard: self.guard.clone(),
            label,
        }))
    }
}

struct GuardedSession {
    inner: Box<dyn Session>,
    guard: Arc<Guard>,
    label: String,
}

#[async_trait]
impl Session for GuardedSession {
    async fn prompt(&mut self, text: &str) -> Result<(), HarnessError> {
        self.inner.prompt(text).await
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        self.inner.next_event().await
    }

    async fn steer(&mut self, text: &str) -> Result<(), HarnessError> {
        self.inner.steer(text).await
    }

    async fn abort(&mut self) -> Result<(), HarnessError> {
        self.inner.abort().await
    }

    async fn answer_permission(
        &mut self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        self.inner.answer_permission(request_id, decision).await
    }

    async fn close(&mut self, grace: Duration) -> SessionExit {
        let exit = self.inner.close(grace).await;
        self.guard.restore(&format!("session {}", self.label));
        exit
    }
}

/// A `verify` runner followed by [`Guard::restore`].
pub struct GuardedVerify {
    inner: Arc<dyn VerifyRunner>,
    guard: Arc<Guard>,
}

impl GuardedVerify {
    pub fn new(inner: Arc<dyn VerifyRunner>, guard: Arc<Guard>) -> Self {
        Self { inner, guard }
    }
}

impl std::fmt::Debug for GuardedVerify {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedVerify").finish_non_exhaustive()
    }
}

#[async_trait]
impl VerifyRunner for GuardedVerify {
    async fn run(&self, args: &[&str], timeout: Duration) -> RawVerifyOutput {
        let output = self.inner.run(args, timeout).await;
        self.guard.restore("verify");
        output
    }
}

/// A job runner whose every job is followed by [`Guard::restore`], as
/// `verify` is.
pub struct GuardedJobs {
    inner: Arc<dyn JobRunner>,
    guard: Arc<Guard>,
}

impl GuardedJobs {
    pub fn new(inner: Arc<dyn JobRunner>, guard: Arc<Guard>) -> Self {
        Self { inner, guard }
    }
}

impl std::fmt::Debug for GuardedJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedJobs").finish_non_exhaustive()
    }
}

#[async_trait]
impl JobRunner for GuardedJobs {
    async fn run(&self, job: &JobSpec, stop: &tenaz_core::ports::CancellationToken) -> JobOutcome {
        let outcome = self.inner.run(job, stop).await;
        self.guard.restore("a job");
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::create_dir_all(root.join(".tenaz")).unwrap();
        std::fs::write(root.join(LEDGER), "{}\n").unwrap();
        (dir, root)
    }

    #[test]
    fn a_project_policy_protects_what_tenaz_runs_outside_the_sandbox() {
        let (_dir, root) = repo();
        let policy = Policy::for_project(&root, "", "").unwrap();
        assert!(
            policy.write.iter().any(|w| root.starts_with(w)),
            "{:?}",
            policy.write
        );
        for protected in [".git/hooks", ".git/config", ".tenaz/run.json"] {
            assert!(
                policy.deny_write.contains(&root.join(protected)),
                "{protected} in {:?}",
                policy.deny_write
            );
        }
        assert!(policy.write.iter().any(|w| w.ends_with("dev")));
        #[cfg(target_os = "macos")]
        {
            let cache = std::fs::canonicalize(user_cache_dir().unwrap()).unwrap();
            assert!(
                policy.write.iter().any(|w| cache.starts_with(w)),
                "Metal's cache, {} in {:?}",
                cache.display(),
                policy.write
            );
        }
        // The git dir is inside the project: it adds no write root.
        assert!(!policy.write.contains(&root.join(".git")));
    }

    #[test]
    fn the_scope_without_the_sandbox_allows_what_the_sandbox_would() {
        let (dir, root) = repo();
        let scope = write_scope(dir.path(), "").unwrap();
        let outside = |path: &Path| scope.outside(&serde_json::json!({ "path": path }));
        for inside in [
            dir.path().join(".tenaz/spec.md"),
            root.join(".tenaz/spec.md"),
            PathBuf::from("/tmp/x"),
            std::env::temp_dir().join("x"),
        ] {
            assert_eq!(outside(&inside), None, "{}", inside.display());
        }
        assert!(outside(Path::new("/elsewhere/.tenaz/spec.md")).is_some());
    }

    #[test]
    fn listed_directories_are_expanded_and_missing_ones_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let list = format!("{}, /no/such/dir,,", dir.path().display());
        assert_eq!(existing_dirs(&list), vec![real]);
    }

    #[test]
    fn nested_write_roots_collapse_into_the_outer_one() {
        let paths = outermost(vec![
            PathBuf::from("/a/b"),
            PathBuf::from("/a"),
            PathBuf::from("/c"),
            PathBuf::from("/a"),
        ]);
        assert_eq!(paths, [PathBuf::from("/a"), PathBuf::from("/c")]);
    }

    #[test]
    fn the_wrapper_passes_the_policy_before_the_program() {
        let policy = Policy {
            write: vec![PathBuf::from("/p")],
            deny_write: vec![PathBuf::from("/p/.git/hooks")],
            deny_read: vec![],
        };
        let (program, args) = tenaz_core::util::wrap_command(
            &policy.wrapper(Path::new("/bin/tenaz")),
            Path::new("fx"),
            vec!["acp".to_owned()],
        );
        assert_eq!(program, PathBuf::from("/bin/tenaz"));
        assert_eq!(
            args,
            [
                "sandbox-exec",
                "--write",
                "/p",
                "--deny-write",
                "/p/.git/hooks",
                "--",
                "fx",
                "acp"
            ]
        );
    }

    #[test]
    fn the_profile_denies_renaming_the_directories_above_a_protected_path() {
        let policy = Policy {
            write: vec![PathBuf::from("/p")],
            deny_write: vec![PathBuf::from("/p/.git/hooks")],
            deny_read: vec![PathBuf::from("/home/u/.ssh")],
        };
        let profile = policy.seatbelt_profile();
        assert!(profile.contains("(allow file-write*\n  (subpath \"/p\"))"));
        assert!(profile.contains("(deny file-write*\n  (subpath \"/p/.git/hooks\"))"));
        assert!(profile.contains("(literal \"/p/.git\")"));
        assert!(profile.contains("(literal \"/p\")"));
        assert!(!profile.contains("(literal \"/\")"));
        assert!(profile.contains("(deny file-read*\n  (subpath \"/home/u/.ssh\"))"));
    }

    #[test]
    fn landlock_reads_skip_only_the_denied_trees() {
        let dir = tempfile::tempdir().unwrap();
        let top = std::fs::canonicalize(dir.path()).unwrap();
        for sub in ["home/u/.ssh", "home/u/src", "etc"] {
            std::fs::create_dir_all(top.join(sub)).unwrap();
        }
        std::fs::write(top.join("home/u/.profile"), "").unwrap();
        let (mut readable, mut listable) = read_rules(&top, &[top.join("home/u/.ssh")]);
        readable.sort();
        listable.sort();
        assert_eq!(
            readable,
            [
                top.join("etc"),
                top.join("home/u/.profile"),
                top.join("home/u/src")
            ]
        );
        assert_eq!(
            listable,
            [top.clone(), top.join("home"), top.join("home/u")]
        );
        let (readable, listable) = read_rules(&top, &[]);
        assert_eq!((readable, listable), (vec![top], vec![]));
    }

    #[test]
    fn the_guard_puts_back_hooks_config_and_the_saved_ledger() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, root) = repo();
        let project = Project::new(&root);
        let hook = root.join(".git/hooks/pre-commit");
        let policy = Policy::for_project(&root, "", "").unwrap();
        let config = std::fs::read(root.join(".git/config")).unwrap();
        let guard = Guard::new(&policy, &project);
        assert!(guard.restore("nobody").is_empty());

        std::fs::write(&hook, "#!/bin/sh\necho owned\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n\tfsmonitor = evil\n").unwrap();
        std::fs::write(root.join(LEDGER), "{\"engineStart\":\"evil\"}\n").unwrap();
        let mut restored = guard.restore("a session");
        restored.sort();
        assert_eq!(restored.len(), 3, "{restored:?}");
        assert!(!hook.exists());
        assert_eq!(std::fs::read(root.join(".git/config")).unwrap(), config);
        assert_eq!(std::fs::read_to_string(root.join(LEDGER)).unwrap(), "{}\n");

        // A ledger Tenaz saves while the run works is the one kept.
        let config = tenaz_core::ledger::ModelConfig {
            harness: tenaz_core::ledger::HarnessKind::Fake,
            harness_version: "0".into(),
            model: "fake".into(),
            provider: None,
            id: "fake".into(),
            base_url: None,
            context_window: 8192,
            input: vec![],
            thinking: None,
            role_models: BTreeMap::new(),
        };
        let ledger = tenaz_core::ledger::Ledger::new(
            "t".into(),
            "b".into(),
            "s".into(),
            "0".into(),
            config,
            tenaz_core::params::RunParams::default(),
            vec![],
        );
        project.save_ledger(&ledger).unwrap();
        let saved = std::fs::read_to_string(root.join(LEDGER)).unwrap();
        assert!(guard.restore("a session").is_empty());
        std::fs::write(root.join(LEDGER), "tampered").unwrap();
        guard.restore("a session");
        assert_eq!(std::fs::read_to_string(root.join(LEDGER)).unwrap(), saved);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_keeps_writes_inside_the_project() {
        let (_dir, root) = repo();
        let outside = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let mut policy = Policy::for_project(&root, "", "").unwrap();
        // The project is in the temp dir, a write root itself: keep only the project.
        policy.write = vec![root.clone(), PathBuf::from("/dev")];
        let script = format!(
            "echo ok > {root}/made && \
             ! echo x > {outside}/escaped 2>/dev/null && \
             ! echo x > {root}/.git/hooks/pre-commit 2>/dev/null && \
             ! echo x >> {root}/.git/config 2>/dev/null && \
             ! mv {root}/.git {root}/moved 2>/dev/null && \
             ! echo x > {root}/.tenaz/run.json 2>/dev/null",
            root = root.display(),
            outside = outside.path().display()
        );
        let status = std::process::Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(policy.seatbelt_profile())
            .args(["/bin/sh", "-c", &script])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(root.join("made").exists());
        assert!(!outside.path().join("escaped").exists());
    }

    /// Only a `C` directory under `/var/folders` is taken from `getconf`.
    #[test]
    fn only_a_cache_directory_under_var_folders_is_trusted() {
        for (printed, trusted) in [
            ("/var/folders/zb/ys04/C/\n", Some("/var/folders/zb/ys04/C")),
            (
                "/private/var/folders/zb/ys04/C",
                Some("/private/var/folders/zb/ys04/C"),
            ),
            ("", None),
            ("\n", None),
            ("/", None),
            ("/var/folders/", None),
            ("/Users/me/C", None),
            ("/var/folders/zb/ys04/T/", None),
            ("/var/folders/../../Users/me/C", None),
            ("/var/folders/C", None),
            ("/var/folders/zb/ys04/x/C", None),
        ] {
            assert_eq!(
                cache_dir_from(printed.as_bytes()),
                trusted.map(PathBuf::from),
                "{printed:?}"
            );
        }
    }

    /// A sandboxed program writes the per-user cache, where Metal keeps
    /// its compiled shaders.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_lets_a_program_write_the_user_cache() {
        let (_dir, root) = repo();
        let cache = tempfile::tempdir_in(user_cache_dir().unwrap()).unwrap();
        let policy = Policy::for_project(&root, "", "").unwrap();
        let script = format!("echo ok > {}/shader", cache.path().display());
        let status = std::process::Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(policy.seatbelt_profile())
            .args(["/bin/sh", "-c", &script])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(cache.path().join("shader").exists());
    }
}
