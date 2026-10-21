//! `init` on an existing repository works on a new branch in a worktree,
//! refuses a dirty tree, keeps the old way with `--in-place`, and `finish`
//! turns the run into one commit without Tenaz's files.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tenaz() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tenaz"))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Someone",
            "-c",
            "user.email=someone@localhost",
        ])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository `app` with one commit on `main`, inside a fresh directory,
/// and a brief outside it.
fn repository() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let work = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(work.path()).unwrap();
    let repo = root.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "# App\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    let brief = root.join("brief.md");
    std::fs::write(&brief, "# Fake brief\n\nBuild four trivial files.\n").unwrap();
    (work, repo, brief)
}

const FAKE: [&str; 14] = [
    "--harness",
    "fake",
    "--model",
    "fake",
    "--provider",
    "fake",
    "--set",
    "maxAttempts=2",
    "--set",
    "engineRetryBaseSec=0",
    "--set",
    "notify=false",
    "--set",
    "replanEvery=2",
];

fn init(repo: &Path, brief: Option<&Path>, extra: &[&str]) -> Output {
    let mut command = tenaz();
    command
        .args(["init", "--project"])
        .arg(repo)
        .args(FAKE)
        .args(extra);
    if let Some(brief) = brief {
        command.arg("--brief").arg(brief);
    }
    command.output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn ledger(project: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(project.join(".tenaz/run.json")).unwrap())
        .unwrap()
}

#[test]
fn init_on_a_repository_works_in_a_worktree_and_finish_squashes_the_run() {
    let (_work, repo, brief) = repository();
    let base = git(&repo, &["rev-parse", "HEAD"]);

    let out = init(&repo, Some(&brief), &[]);
    assert!(out.status.success(), "{}", text(&out));
    let worktree = repo.with_file_name("app-tenaz-fake-brief");
    assert!(text(&out).contains("tenaz/fake-brief"), "{}", text(&out));
    assert!(worktree.join(".tenaz/run.json").exists());
    // The user's checkout is as it was.
    assert!(!repo.join(".tenaz").exists());
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
    assert_eq!(
        git(&worktree, &["branch", "--show-current"]),
        "tenaz/fake-brief"
    );

    let origin = &ledger(&worktree)["origin"];
    assert_eq!(origin["base_branch"], "main");
    assert_eq!(origin["base_commit"], base.as_str());
    assert_eq!(origin["branch"], "tenaz/fake-brief");
    assert_eq!(origin["worktree"], worktree.to_str().unwrap());
    assert_eq!(origin["repo"], repo.to_str().unwrap());
    assert_eq!(
        origin["init_commit"],
        git(&worktree, &["rev-parse", "HEAD"]).as_str()
    );

    // From the user's checkout, run and status point at the worktree.
    let elsewhere = tenaz()
        .args(["run", "--project"])
        .arg(&repo)
        .output()
        .unwrap();
    assert!(!elsewhere.status.success());
    assert!(
        text(&elsewhere).contains("A Tenaz run works in the worktree"),
        "{}",
        text(&elsewhere)
    );
    let status = tenaz()
        .args(["status", "--project"])
        .arg(&repo)
        .output()
        .unwrap();
    assert!(
        text(&status).contains("app-tenaz-fake-brief"),
        "{}",
        text(&status)
    );

    let run = tenaz()
        .args(["run", "--project"])
        .arg(&worktree)
        .output()
        .unwrap();
    assert!(run.status.success(), "{}", text(&run));
    assert!(
        text(&run).contains("Run finished: COMPLETE"),
        "{}",
        text(&run)
    );
    assert!(text(&run).contains("Worktree: "), "{}", text(&run));
    let status = tenaz()
        .args(["status", "--project"])
        .arg(&worktree)
        .output()
        .unwrap();
    assert!(
        text(&status).contains("(branch tenaz/fake-brief, from main at "),
        "{}",
        text(&status)
    );

    let finish = tenaz()
        .args(["finish", "--project"])
        .arg(&worktree)
        .output()
        .unwrap();
    assert!(finish.status.success(), "{}", text(&finish));
    assert!(
        text(&finish).contains("Created branch fake-brief"),
        "{}",
        text(&finish)
    );
    assert!(
        text(&finish).contains("Nothing was pushed"),
        "{}",
        text(&finish)
    );
    assert_eq!(git(&repo, &["rev-parse", "fake-brief^"]), base);
    let files = git(&repo, &["ls-tree", "-r", "--name-only", "fake-brief"]);
    assert!(!files.contains(".tenaz"), "{files}");
    assert!(
        !files.lines().any(|f| f == "BRIEF.md" || f == "AGENTS.md"),
        "{files}"
    );
    assert!(files.lines().any(|f| f == "README.md"), "{files}");
    // In an existing project verify is .tenaz/verify, so it does not ship.
    assert!(!files.lines().any(|f| f == "verify"), "{files}");
    assert!(
        files.lines().count() > 2,
        "the run's files are there: {files}"
    );
    assert_eq!(git(&repo, &["show", "fake-brief:.gitignore"]), "target/");
    let message = git(&repo, &["log", "-1", "--format=%B", "fake-brief"]);
    assert!(message.starts_with("Fake brief\n"), "{message}");
    assert!(message.contains("on branch tenaz/fake-brief"), "{message}");
    // The run's history is still there; the user's branch did not move.
    assert!(
        git(&repo, &["rev-list", "--count", "tenaz/fake-brief"])
            .parse::<u32>()
            .unwrap()
            > 2
    );
    assert_eq!(git(&repo, &["rev-parse", "main"]), base);

    let again = tenaz()
        .args(["finish", "--project"])
        .arg(&worktree)
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(
        text(&again).contains("Branch fake-brief exists"),
        "{}",
        text(&again)
    );

    let remove = tenaz()
        .args(["finish", "--force", "--remove-worktree", "--project"])
        .arg(&worktree)
        .output()
        .unwrap();
    assert!(remove.status.success(), "{}", text(&remove));
    assert!(!worktree.exists());
    assert!(git(&repo, &["branch", "--list", "tenaz/fake-brief"]).contains("tenaz/fake-brief"));
}

#[test]
fn a_dirty_tree_is_refused_and_nothing_is_created() {
    let (_work, repo, brief) = repository();
    std::fs::write(repo.join("README.md"), "# App, edited\n").unwrap();
    std::fs::write(repo.join("notes.txt"), "mine\n").unwrap();
    let out = init(&repo, Some(&brief), &[]);
    assert!(!out.status.success());
    let message = text(&out);
    assert!(message.contains("uncommitted changes"), "{message}");
    assert!(message.contains(" M README.md"), "{message}");
    assert!(message.contains("?? notes.txt"), "{message}");
    assert!(message.contains("--in-place"), "{message}");
    assert!(!repo.with_file_name("app-tenaz-fake-brief").exists());
    assert_eq!(git(&repo, &["branch", "--list", "tenaz/*"]), "");
    assert!(!repo.join(".tenaz").exists());
}

#[test]
fn in_place_keeps_working_on_the_current_branch() {
    let (_work, repo, brief) = repository();
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("notes.txt"), "mine\n").unwrap();
    let out = init(&repo, Some(&brief), &["--in-place"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(repo.join(".tenaz/run.json").exists());
    assert!(!repo.with_file_name("app-tenaz-fake-brief").exists());
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
    assert_eq!(git(&repo, &["log", "-1", "--format=%s"]), "tenaz: init");
    // As before, the first commit takes what was uncommitted.
    assert!(git(&repo, &["show", "--name-only", "--format=", "HEAD"]).contains("notes.txt"));
    let origin = &ledger(&repo)["origin"];
    assert_eq!(origin["base_commit"], base.as_str());
    assert_eq!(origin["branch"], "main");
    assert!(origin.get("worktree").is_none(), "{origin}");

    let finish = tenaz()
        .args(["finish", "--branch", "result", "--project"])
        .arg(&repo)
        .output()
        .unwrap();
    assert!(finish.status.success(), "{}", text(&finish));
    assert_eq!(git(&repo, &["rev-parse", "result^"]), base);
    let files = git(&repo, &["ls-tree", "-r", "--name-only", "result"]);
    assert!(
        files.contains("notes.txt") && !files.contains(".tenaz"),
        "{files}"
    );
}

#[test]
fn an_untracked_brief_in_the_checkout_becomes_the_runs_brief() {
    let (_work, repo, _brief) = repository();
    std::fs::create_dir_all(repo.join(".tenaz")).unwrap();
    std::fs::write(
        repo.join(".tenaz/brief.md"),
        "# Goal\n\nAdd a login page.\n",
    )
    .unwrap();
    let out = init(&repo, None, &[]);
    assert!(out.status.success(), "{}", text(&out));
    let worktree = repo.with_file_name("app-tenaz-add-a-login-page");
    assert_eq!(
        std::fs::read_to_string(worktree.join(".tenaz/brief.md")).unwrap(),
        "# Goal\n\nAdd a login page.\n"
    );
    // Left where the user put it.
    assert!(repo.join(".tenaz/brief.md").exists());
    assert!(!repo.join(".tenaz/run.json").exists());
}

#[test]
fn new_directories_and_subdirectories_of_a_repository() {
    let (_work, repo, brief) = repository();
    // A new directory inside the repository: a new project, as before.
    let fresh = repo.join("fresh");
    let out = init(&fresh, Some(&brief), &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(fresh.join(".git").exists());
    assert!(ledger(&fresh).get("origin").is_none());

    // Seeded directories the repository tracks nothing in, ignored or not
    // added yet: new projects too, with the seed in the first commit.
    std::fs::write(repo.join(".gitignore"), "target/\nruns/\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "ignore runs"]);
    for seeded in [repo.join("runs/one"), repo.join("draft")] {
        std::fs::create_dir_all(seeded.join("vendor")).unwrap();
        std::fs::write(seeded.join("vendor/lib.js"), "\n").unwrap();
        let out = init(&seeded, Some(&brief), &[]);
        assert!(out.status.success(), "{}", text(&out));
        assert!(seeded.join(".git").exists());
        assert!(ledger(&seeded).get("origin").is_none());
        assert_eq!(git(&seeded, &["ls-files", "vendor"]), "vendor/lib.js");
    }

    // A directory with files of the repository: refused.
    let sub = repo.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("lib.rs"), "\n").unwrap();
    git(&repo, &["add", "sub"]);
    git(&repo, &["commit", "-q", "-m", "sub"]);
    let out = init(&sub, Some(&brief), &[]);
    assert!(!out.status.success());
    assert!(text(&out).contains("not at its top"), "{}", text(&out));
}

#[test]
fn an_untracked_brief_can_also_be_named_with_brief() {
    let (_work, repo, _brief) = repository();
    std::fs::create_dir_all(repo.join(".tenaz")).unwrap();
    std::fs::write(repo.join(".tenaz/brief.md"), "# Add a login page\n").unwrap();
    let out = init(&repo, Some(&repo.join(".tenaz/brief.md")), &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        repo.with_file_name("app-tenaz-add-a-login-page")
            .join(".tenaz/brief.md")
            .exists()
    );
}
