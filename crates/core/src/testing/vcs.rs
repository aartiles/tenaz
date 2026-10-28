//! A repository kept as snapshots of the files, in memory: `Vcs` without
//! git. `vcs_contract` holds it to what `GitRepo` does.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::ports::{Vcs, VcsError};
use crate::util::sha256_hex;

/// Path to contents, relative to the root, with `/` separators.
type Tree = BTreeMap<String, Vec<u8>>;

#[derive(Debug, Default)]
struct State {
    commits: BTreeMap<String, Tree>,
    head: Option<String>,
}

/// The project's files committed as snapshots. It honours the `.gitignore`
/// files it finds, for the patterns Tenaz writes (`/run.json`, `private/`,
/// plain names); a wildcard pattern or a symlink is a test bug and panics.
/// File modes are not kept, so a restored `verify` is not executable: pair
/// it with `ScriptedVerify`, not the real `verify`.
#[derive(Debug)]
pub struct SnapshotVcs {
    root: PathBuf,
    state: Mutex<State>,
}

impl SnapshotVcs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The files git would see: everything under the root but `.git` and
    /// what a `.gitignore` leaves out.
    fn worktree(&self) -> Tree {
        self.listing().files
    }

    fn listing(&self) -> Listing {
        let mut listing = Listing::default();
        walk(&self.root, "", &mut Vec::new(), &mut listing);
        listing
    }

    /// The commit id `rev` names: `HEAD` or an id.
    fn resolve_id(state: &State, rev: &str) -> Result<String, VcsError> {
        let id = if rev == "HEAD" {
            state.head.clone()
        } else {
            Some(rev.to_owned())
        };
        id.filter(|id| state.commits.contains_key(id))
            .ok_or_else(|| VcsError(format!("unknown revision {rev}")))
    }

    fn resolve(state: &State, rev: &str) -> Result<Tree, VcsError> {
        let id = Self::resolve_id(state, rev)?;
        Ok(state.commits[&id].clone())
    }

    fn head_tree(state: &State) -> Tree {
        state
            .head
            .as_ref()
            .and_then(|id| state.commits.get(id))
            .cloned()
            .unwrap_or_default()
    }

    fn commit(state: &mut State, tree: Tree, message: &str) -> String {
        let id = sha256_hex(format!("{}{message}", state.commits.len()));
        state.commits.insert(id.clone(), tree);
        state.head = Some(id.clone());
        id
    }

    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), VcsError> {
        let target = self.root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| VcsError(format!("{path}: {e}")))?;
        }
        std::fs::write(&target, bytes).map_err(|e| VcsError(format!("{path}: {e}")))
    }

    /// Deletes the file or folder at `path`; a missing one is fine.
    fn remove(&self, path: &str) -> Result<(), VcsError> {
        let target = self.root.join(path);
        let result = if target.is_dir() {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_file(&target)
        };
        match result {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(VcsError(format!("remove {path}: {e}")))
            }
            _ => Ok(()),
        }
    }

    /// Makes the files under `paths` (everything when empty) those of
    /// `target`, deleting the ones `keep` does not hold either.
    fn check_out(&self, target: &Tree, paths: &[&str], keep: &Tree) -> Result<(), VcsError> {
        let selected = |path: &str| paths.is_empty() || paths.iter().any(|p| under(path, p));
        let listing = self.listing();
        for path in listing.files.keys().filter(|p| selected(p)) {
            if !target.contains_key(path) && !keep.contains_key(path) {
                self.remove(path)?;
            }
        }
        for (path, bytes) in target.iter().filter(|(p, _)| selected(p)) {
            self.write(path, bytes)?;
        }
        // As `git clean -d`: the folders left empty go, ignored ones stay.
        for dir in listing.dirs.iter().filter(|d| selected(d)) {
            let _ = std::fs::remove_dir(self.root.join(dir));
        }
        Ok(())
    }
}

/// `path` is `dir` or inside it.
fn under(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// One `.gitignore` line, relative to the folder it was found in.
#[derive(Debug)]
struct Rule {
    base: String,
    pattern: String,
    anchored: bool,
    dir_only: bool,
}

impl Rule {
    fn parse(base: &str, line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        assert!(
            !line.contains(['*', '?', '[', '!']),
            "SnapshotVcs does not understand the .gitignore pattern {line}"
        );
        let dir_only = line.ends_with('/');
        let body = line.trim_end_matches('/');
        Some(Self {
            base: base.to_owned(),
            pattern: body.trim_start_matches('/').to_owned(),
            anchored: body.contains('/'),
            dir_only,
        })
    }

    /// Whether `path` (a file when `is_dir` is false) is left out.
    fn ignores(&self, path: &str, is_dir: bool) -> bool {
        let rel = if self.base.is_empty() {
            path
        } else {
            match path.strip_prefix(&format!("{}/", self.base)) {
                Some(rel) => rel,
                None => return false,
            }
        };
        let hit = if self.anchored {
            rel == self.pattern
        } else {
            rel.rsplit('/').next() == Some(self.pattern.as_str())
        };
        hit && (is_dir || !self.dir_only)
    }
}

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_owned()
    } else {
        format!("{rel}/{name}")
    }
}

/// `.git` at the root, and whatever a rule in force leaves out.
fn skipped(path: &str, is_dir: bool, rules: &[Rule]) -> bool {
    path == ".git" || rules.iter().any(|r| r.ignores(path, is_dir))
}

/// What git sees of the working tree: the files, and the folders it would
/// clean, deepest first.
#[derive(Debug, Default)]
struct Listing {
    files: Tree,
    dirs: Vec<String>,
}

fn walk(dir: &Path, rel: &str, rules: &mut Vec<Rule>, listing: &mut Listing) {
    let depth = rules.len();
    if let Ok(text) = std::fs::read_to_string(dir.join(".gitignore")) {
        rules.extend(text.lines().filter_map(|line| Rule::parse(rel, line)));
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = join(rel, &entry.file_name().to_string_lossy());
        let kind = entry.file_type().ok();
        assert!(
            !kind.is_some_and(|k| k.is_symlink()),
            "SnapshotVcs does not keep symlinks: {path}"
        );
        let is_dir = kind.is_some_and(|k| k.is_dir());
        if skipped(&path, is_dir, rules) {
            continue;
        }
        if is_dir {
            walk(&entry.path(), &path, rules, listing);
            listing.dirs.push(path);
        } else if let Ok(bytes) = std::fs::read(entry.path()) {
            listing.files.insert(path, bytes);
        }
    }
    rules.truncate(depth);
}

/// The paths whose contents differ between `a` and `b`.
fn changed<'a>(a: &'a Tree, b: &'a Tree) -> Vec<&'a str> {
    let mut paths: Vec<&str> = a
        .keys()
        .chain(b.keys())
        .map(String::as_str)
        .filter(|p| a.get(*p) != b.get(*p))
        .collect();
    paths.sort_unstable();
    paths.dedup();
    paths
}

/// Leaves out `.tenaz/` but `.tenaz/verify` when `exclude_agent`.
fn shown(path: &str, exclude_agent: bool) -> bool {
    !exclude_agent || !under(path, ".tenaz") || path == ".tenaz/verify"
}

fn text(bytes: Option<&Vec<u8>>) -> String {
    bytes
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}

/// Git's header for one changed path, which marks a file added or deleted.
fn header(path: &str, before: bool, after: bool) -> String {
    let (mode, old, new) = match (before, after) {
        (false, _) => (
            "new file mode 100644\n",
            "/dev/null".to_owned(),
            format!("b/{path}"),
        ),
        (_, false) => (
            "deleted file mode 100644\n",
            format!("a/{path}"),
            "/dev/null".to_owned(),
        ),
        _ => ("", format!("a/{path}"), format!("b/{path}")),
    };
    format!("diff --git a/{path} b/{path}\n{mode}--- {old}\n+++ {new}\n")
}

/// A whole-file diff of each changed path: every old line out, every new
/// line in, under git's headers.
fn patch(a: &Tree, b: &Tree, exclude_agent: bool) -> String {
    let mut out = String::new();
    for path in changed(a, b)
        .into_iter()
        .filter(|p| shown(p, exclude_agent))
    {
        out.push_str(&header(path, a.contains_key(path), b.contains_key(path)));
        for line in text(a.get(path)).lines() {
            let _ = writeln!(out, "-{line}");
        }
        for line in text(b.get(path)).lines() {
            let _ = writeln!(out, "+{line}");
        }
    }
    out
}

impl Vcs for SnapshotVcs {
    fn init(&self) -> Result<(), VcsError> {
        std::fs::create_dir_all(self.root.join(".git")).map_err(|e| VcsError(e.to_string()))?;
        *self.state() = State::default();
        Ok(())
    }

    fn head(&self) -> Result<String, VcsError> {
        self.state()
            .head
            .clone()
            .ok_or_else(|| VcsError("no commits yet".to_owned()))
    }

    fn commit_all(&self, message: &str) -> Result<String, VcsError> {
        let tree = self.worktree();
        let mut state = self.state();
        if tree != Self::head_tree(&state) {
            Self::commit(&mut state, tree, message);
        }
        state
            .head
            .clone()
            .ok_or_else(|| VcsError("no commits yet".to_owned()))
    }

    fn commit_paths(&self, paths: &[&str], message: &str) -> Result<(), VcsError> {
        let worktree = self.worktree();
        let mut state = self.state();
        let head = Self::head_tree(&state);
        let mut tree: Tree = head
            .iter()
            .filter(|(p, _)| !paths.iter().any(|d| under(p, d)))
            .map(|(p, b)| (p.clone(), b.clone()))
            .collect();
        tree.extend(
            worktree
                .into_iter()
                .filter(|(p, _)| paths.iter().any(|d| under(p, d))),
        );
        if tree != head {
            Self::commit(&mut state, tree, message);
        }
        Ok(())
    }

    fn changed_paths(&self) -> Result<Vec<String>, VcsError> {
        let worktree = self.worktree();
        let head = Self::head_tree(&self.state());
        Ok(changed(&head, &worktree)
            .into_iter()
            .map(str::to_owned)
            .collect())
    }

    fn in_head(&self, path: &str) -> bool {
        Self::head_tree(&self.state())
            .keys()
            .any(|p| under(p, path))
    }

    fn head_content(&self, path: &str) -> Option<String> {
        Self::head_tree(&self.state())
            .get(path)
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    fn revert_path(&self, path: &str) -> Result<(), VcsError> {
        let head = Self::head_tree(&self.state());
        if !head.keys().any(|p| under(p, path)) {
            return self.remove(path);
        }
        for (p, bytes) in head.iter().filter(|(p, _)| under(p, path)) {
            self.write(p, bytes)?;
        }
        Ok(())
    }

    fn reset_hard(&self, sha: &str) -> Result<(), VcsError> {
        let (id, target) = {
            let state = self.state();
            let id = Self::resolve_id(&state, sha)?;
            let target = state.commits[&id].clone();
            (id, target)
        };
        self.check_out(&target, &[], &Tree::new())?;
        self.state().head = Some(id);
        Ok(())
    }

    fn restore_paths(&self, sha: &str, paths: &[&str]) -> Result<(), VcsError> {
        let (target, head) = {
            let state = self.state();
            (Self::resolve(&state, sha)?, Self::head_tree(&state))
        };
        if let Some(missing) = paths.iter().find(|d| !target.keys().any(|p| under(p, d))) {
            return Err(VcsError(format!("pathspec {missing} did not match")));
        }
        self.check_out(&target, paths, &head)
    }

    /// As git's status and `diff HEAD`: tracked files by their contents,
    /// untracked ones by name only.
    fn worktree_fingerprint(&self) -> Result<String, VcsError> {
        let worktree = self.worktree();
        let head = Self::head_tree(&self.state());
        let (tracked, untracked): (Tree, Tree) = worktree
            .into_iter()
            .partition(|(p, _)| head.contains_key(p));
        let names: Vec<&String> = untracked.keys().collect();
        Ok(sha256_hex(format!(
            "{names:?}{}",
            patch(&head, &tracked, false)
        )))
    }

    fn diff(&self, from: &str, to: &str, exclude_agent: bool) -> Result<String, VcsError> {
        let state = self.state();
        let (a, b) = (Self::resolve(&state, from)?, Self::resolve(&state, to)?);
        Ok(patch(&a, &b, exclude_agent))
    }

    fn diff_stat(
        &self,
        from: &str,
        to: Option<&str>,
        exclude_agent: bool,
    ) -> Result<String, VcsError> {
        let state = self.state();
        let a = Self::resolve(&state, from)?;
        let b = if let Some(to) = to {
            Self::resolve(&state, to)?
        } else {
            // Untracked files are not in a diff against the working tree.
            let head = Self::head_tree(&state);
            self.worktree()
                .into_iter()
                .filter(|(p, _)| head.contains_key(p))
                .collect()
        };
        let mut out = String::new();
        for path in changed(&a, &b)
            .into_iter()
            .filter(|p| shown(p, exclude_agent))
        {
            let _ = writeln!(out, " {path} | changed");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::vcs_contract_tests!(|root: &Path| -> Box<dyn Vcs> { Box::new(SnapshotVcs::new(root)) });
}
