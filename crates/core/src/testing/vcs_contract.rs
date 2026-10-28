//! What the orchestrator needs from a `Vcs`, as checks any implementation
//! can run: `GitRepo` runs them against git, `SnapshotVcs` against itself,
//! so the fake cannot drift from git unnoticed.

use std::path::Path;

use crate::ports::Vcs;

/// Makes the repository under test at a folder.
pub type MakeVcs<'a> = &'a dyn Fn(&Path) -> Box<dyn Vcs>;

/// One `#[test]` per check, for the `Vcs` the closure `$make` builds at a
/// folder (`|root: &Path| -> Box<dyn Vcs> { ... }`).
#[macro_export]
macro_rules! vcs_contract_tests {
    ($make:expr) => {
        $crate::vcs_contract_tests!(@each $make;
            an_empty_repository_has_no_head,
            commits_only_when_something_changed,
            changed_paths_and_reverts,
            fingerprint_reset_and_restore,
            diffs_leave_out_tenaz_but_its_verify,
            diffs_mark_added_and_deleted_files,
            a_reset_to_head_keeps_head_a_commit,
            ignored_files_are_left_alone);
    };
    (@each $make:expr; $($check:ident),*) => {
        $(
            #[test]
            fn $check() {
                $crate::testing::vcs_contract::$check(&$make);
            }
        )*
    };
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn read(root: &Path, path: &str) -> Option<String> {
    std::fs::read_to_string(root.join(path)).ok()
}

/// A repository with `a.txt` committed.
fn repo(make: MakeVcs<'_>) -> (tempfile::TempDir, Box<dyn Vcs>) {
    let dir = tempfile::tempdir().unwrap();
    let vcs = make(dir.path());
    vcs.init().unwrap();
    write(dir.path(), "a.txt", "a\n");
    vcs.commit_all("first").unwrap();
    (dir, vcs)
}

pub fn an_empty_repository_has_no_head(make: MakeVcs<'_>) {
    let dir = tempfile::tempdir().unwrap();
    let vcs = make(dir.path());
    vcs.init().unwrap();
    assert!(dir.path().join(".git").exists());
    assert!(vcs.head().is_err());
    assert!(vcs.commit_all("nothing").is_err());
}

pub fn commits_only_when_something_changed(make: MakeVcs<'_>) {
    let (dir, vcs) = repo(make);
    let head = vcs.head().unwrap();
    assert_eq!(vcs.commit_all("nothing").unwrap(), head);
    write(dir.path(), "b.txt", "b\n");
    let next = vcs.commit_all("second").unwrap();
    assert_ne!(next, head);
    assert_eq!(vcs.head_content("b.txt").as_deref(), Some("b\n"));
    assert_eq!((vcs.in_head("b.txt"), vcs.in_head("c.txt")), (true, false));
    write(dir.path(), "a.txt", "changed\n");
    write(dir.path(), "d/e.txt", "e\n");
    vcs.commit_paths(&["d"], "only d").unwrap();
    assert!(vcs.in_head("d"));
    assert_eq!(vcs.head_content("a.txt").as_deref(), Some("a\n"));
    assert_eq!(vcs.changed_paths().unwrap(), ["a.txt"]);
    std::fs::remove_file(dir.path().join("b.txt")).unwrap();
    vcs.commit_all("drop b").unwrap();
    assert!(!vcs.in_head("b.txt"), "a deletion is committed");
}

pub fn changed_paths_and_reverts(make: MakeVcs<'_>) {
    let (dir, vcs) = repo(make);
    write(dir.path(), "a.txt", "changed\n");
    write(dir.path(), "new/dir/c.txt", "c\n");
    let mut changed = vcs.changed_paths().unwrap();
    changed.sort();
    assert_eq!(changed, ["a.txt", "new/dir/c.txt"]);
    vcs.revert_path("a.txt").unwrap();
    vcs.revert_path("new/dir/c.txt").unwrap();
    vcs.revert_path("missing.txt").unwrap();
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("a\n"));
    assert!(!dir.path().join("new/dir/c.txt").exists());
    assert_eq!(vcs.changed_paths().unwrap(), Vec::<String>::new());
    std::fs::remove_file(dir.path().join("a.txt")).unwrap();
    assert_eq!(vcs.changed_paths().unwrap(), ["a.txt"]);
}

pub fn fingerprint_reset_and_restore(make: MakeVcs<'_>) {
    let (dir, vcs) = repo(make);
    let base = vcs.head().unwrap();
    let clean = vcs.worktree_fingerprint().unwrap();
    write(dir.path(), "a.txt", "edited\n");
    let edited = vcs.worktree_fingerprint().unwrap();
    assert_ne!(edited, clean);
    write(dir.path(), "a.txt", "edited again\n");
    assert_ne!(vcs.worktree_fingerprint().unwrap(), edited);
    // An untracked file counts by its name, as in git status.
    write(dir.path(), "new.txt", "x\n");
    let untracked = vcs.worktree_fingerprint().unwrap();
    write(dir.path(), "new.txt", "y\n");
    assert_eq!(vcs.worktree_fingerprint().unwrap(), untracked);
    std::fs::remove_file(dir.path().join("new.txt")).unwrap();
    write(dir.path(), ".tenaz/tasks/T001.md", "card\n");
    let second = vcs.commit_all("edit").unwrap();
    write(dir.path(), ".tenaz/tasks/T003.md", "later\n");
    vcs.commit_all("later").unwrap();
    write(dir.path(), ".tenaz/tasks/T002.md", "new\n");
    write(dir.path(), ".tenaz/tasks/T001.md", "changed\n");
    std::fs::create_dir_all(dir.path().join("elsewhere/empty")).unwrap();
    vcs.restore_paths(&second, &[".tenaz/tasks"]).unwrap();
    let task = |name: &str| dir.path().join(".tenaz/tasks").join(name);
    assert_eq!(std::fs::read_to_string(task("T001.md")).unwrap(), "card\n");
    // T002 was untracked; T003 is HEAD's own; only the paths are cleaned.
    let left = (
        task("T002.md").exists(),
        task("T003.md").exists(),
        dir.path().join("elsewhere/empty").exists(),
    );
    assert_eq!(left, (false, true, true));
    assert!(vcs.restore_paths(&base, &[".tenaz/tasks"]).is_err());
    write(dir.path(), "junk/x.txt", "x\n");
    vcs.reset_hard(&base).unwrap();
    assert_eq!(vcs.head().unwrap(), base);
    let gone = ["junk", ".tenaz/tasks"].map(|p| !dir.path().join(p).exists());
    assert_eq!(gone, [true, true], "untracked files and their folders go");
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("a\n"));
    assert_eq!(vcs.worktree_fingerprint().unwrap(), clean);
}

pub fn diffs_leave_out_tenaz_but_its_verify(make: MakeVcs<'_>) {
    let (dir, vcs) = repo(make);
    let base = vcs.head().unwrap();
    write(dir.path(), "a.txt", "edited\n");
    write(dir.path(), ".tenaz/tasks/T001.md", "card\n");
    let second = vcs.commit_all("edit").unwrap();
    let without = vcs.diff(&base, &second, true).unwrap();
    assert!(
        without.contains("edited") && !without.contains("T001"),
        "{without}"
    );
    let with = [&second, "HEAD"].map(|to| vcs.diff(&base, to, false).unwrap());
    assert!(with.iter().all(|d| d.contains("T001")), "{with:?}");
    // An existing project's verify lives under .tenaz/ and stays visible.
    write(dir.path(), ".tenaz/verify", "#!/bin/sh\nexit 0\n");
    let with_verify = vcs.commit_all("verify").unwrap();
    let shown = vcs.diff(&second, &with_verify, true).unwrap();
    assert!(shown.contains("+++ b/.tenaz/verify"), "{shown}");
    let stat = vcs.diff_stat(&second, Some(&with_verify), true).unwrap();
    assert!(stat.contains(".tenaz/verify"), "{stat}");
    let stat = vcs.diff_stat(&base, Some(&second), true).unwrap();
    assert!(stat.contains("a.txt") && !stat.contains("T001"), "{stat}");
    // Against the working tree: tracked files only, as git diff shows them.
    write(dir.path(), "a.txt", "edited again\n");
    write(dir.path(), "b.txt", "b\n");
    let stat = vcs.diff_stat(&with_verify, None, true).unwrap();
    assert!(stat.contains("a.txt") && !stat.contains("b.txt"), "{stat}");
}

/// The reviewer is shown the images a change adds, not the ones it deletes.
pub fn diffs_mark_added_and_deleted_files(make: MakeVcs<'_>) {
    let (dir, vcs) = repo(make);
    write(dir.path(), "assets/old.png", "old\n");
    let base = vcs.commit_all("old image").unwrap();
    std::fs::remove_file(dir.path().join("assets/old.png")).unwrap();
    write(dir.path(), "assets/new.png", "new\n");
    let next = vcs.commit_all("new image").unwrap();
    let diff = vcs.diff(&base, &next, true).unwrap();
    assert!(diff.contains("deleted file mode"), "{diff}");
    assert!(diff.contains("new file mode"), "{diff}");
    let images = crate::briefing::changed_images(&diff);
    assert_eq!(images, ["assets/new.png"], "{diff}");
}

pub fn a_reset_to_head_keeps_head_a_commit(make: MakeVcs<'_>) {
    let (dir, vcs) = repo(make);
    let head = vcs.head().unwrap();
    write(dir.path(), "a.txt", "edited\n");
    vcs.reset_hard("HEAD").unwrap();
    assert_eq!(vcs.head().unwrap(), head);
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("a\n"));
}

pub fn ignored_files_are_left_alone(make: MakeVcs<'_>) {
    let (dir, vcs) = repo(make);
    write(dir.path(), ".tenaz/.gitignore", "/private/\n/run.json\n");
    write(dir.path(), ".gitignore", "/target/\nbuild/\n");
    let base = vcs.commit_all("ignores").unwrap();
    write(dir.path(), ".tenaz/run.json", "{}\n");
    write(dir.path(), ".tenaz/private/log.txt", "log\n");
    std::fs::create_dir_all(dir.path().join(".tenaz/private/sessions")).unwrap();
    write(dir.path(), "target/debug/x", "x\n");
    write(dir.path(), "src/build/z", "z\n");
    // Anchored at the root only: this one is not ignored.
    write(dir.path(), "src/target/y", "y\n");
    assert_eq!(vcs.changed_paths().unwrap(), ["src/target/y"]);
    std::fs::remove_dir_all(dir.path().join("src/target")).unwrap();
    assert_eq!(vcs.commit_all("nothing").unwrap(), base);
    vcs.reset_hard(&base).unwrap();
    assert_eq!(read(dir.path(), ".tenaz/run.json").as_deref(), Some("{}\n"));
    assert!(dir.path().join(".tenaz/private/log.txt").exists());
    let kept = dir.path().join(".tenaz/private/sessions");
    assert!(kept.exists(), "an empty ignored folder stays");
    assert!(dir.path().join("src/build/z").exists());
}
