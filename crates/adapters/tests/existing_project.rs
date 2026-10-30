//! A run in a repository that already has code, its own agent instructions
//! and its own `.gitignore`: everything Tenaz adds goes under `.tenaz/`,
//! including `verify`, and the project's own files are left alone.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use tenaz_adapters::clock::SystemClock;
use tenaz_adapters::git::GitRepo;
use tenaz_adapters::verify::VerifyProcess;
use tenaz_core::ledger::{HarnessKind, JournalRecord, Launch, ModelConfig, Phase, TaskOutcome};
use tenaz_core::orchestrator::{Deps, InitOptions, RunOptions, Runner, Templates, init};
use tenaz_core::params::RunParams;
use tenaz_core::project::Project;
use tenaz_core::testing::{ScriptedHarness, SilentNotifier};
use tokio_util::sync::CancellationToken;

const OWN: [(&str, &str); 5] = [
    ("AGENTS.md", "# House rules\n\nUse pnpm.\n"),
    ("CLAUDE.md", "See AGENTS.md.\n"),
    (".gitignore", "node_modules\n"),
    ("BRIEF.md", "An unrelated document of the project.\n"),
    ("src/main.js", "console.log(1);\n"),
];

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_own_files_untouched(project: &Project) {
    for (path, text) in OWN {
        assert_eq!(project.read(path).unwrap(), text, "{path} changed");
    }
}

/// After the run: `verify` stayed under `.tenaz/`, the project's own files
/// are as they were, and on real git the failed T003 was split with its
/// work reset away and the brief a session edited was put back.
fn assert_after_the_run(project: &Project) {
    assert!(project.exists(".tenaz/verify"));
    assert!(!project.exists("verify"), "verify written at the root");
    assert_own_files_untouched(project);
    let records: Vec<JournalRecord> = project.journal().into_iter().map(|e| e.record).collect();
    let split = records.iter().any(|r| {
        matches!(r, JournalRecord::Task { task, outcome: TaskOutcome::Split, .. } if task.as_str() == "T003")
    });
    assert!(split, "T003 was not split");
    assert!(
        !project.exists("checks/T003.exists.sh"),
        "T003's work was not reset"
    );
    let reverted = records.iter().any(|r| {
        matches!(r, JournalRecord::Session(s) if s.reverted.iter().any(|p| p == ".tenaz/brief.md"))
    });
    assert!(reverted, "the edited brief was not reverted");
    assert_eq!(project.brief().unwrap(), "# Goal\n\nAdd a flag.\n");
}

/// Two attempts per task and a replan every two tasks, with no wait the
/// run would spend in real time: it runs on the system clock.
fn params() -> RunParams {
    let mut params = RunParams::default();
    for pair in [
        "maxAttempts=2",
        "replanEvery=2",
        "engineRetryBaseSec=0",
        "capacityRetryWaitSec=0",
        "notify=false",
    ] {
        params.apply_override(pair).unwrap();
    }
    params
}

/// The real git and verify in `root`, `harness` and no engine.
fn deps(harness: ScriptedHarness, root: &Path, verify_path: &str) -> Deps {
    Deps {
        harness: Arc::new(harness),
        vcs: Arc::new(GitRepo::new(root)),
        verify: Arc::new(VerifyProcess::new(root).at(verify_path)),
        jobs: Arc::new(tenaz_adapters::job::JobProcess::new(root)),
        clock: Arc::new(SystemClock::new()),
        notifier: Arc::new(SilentNotifier),
        engine: None,
        lease: None,
        engine_metrics: None,
        memory: None,
        effort: None,
        cache_report: None,
        write_scope: None,
    }
}

#[tokio::test]
async fn a_run_in_an_existing_project_stays_under_tenaz() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path().join("app");
    for (path, text) in OWN {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "app"]);

    let brief = work.path().join("goal.md");
    std::fs::write(&brief, "# Goal\n\nAdd a flag.\n").unwrap();
    let project = Project::new(&root);
    let templates =
        Templates::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/templates")).unwrap();
    let params = params();
    let harness = ScriptedHarness {
        verify_path: Some(".tenaz/verify".into()),
        ..ScriptedHarness::every_recovery()
    };
    let options = InitOptions {
        brief: Some(brief),
        config: ModelConfig {
            harness: HarnessKind::Fake,
            harness_version: harness.version(),
            model: "fake/fake".into(),
            provider: Some("fake".into()),
            id: "fake".into(),
            base_url: None,
            context_window: 131_072,
            input: vec!["text".into()],
            thinking: None,
        },
        launch: Launch::default(),
        params,
        skills: vec![],
        runner_version: "test".into(),
    };
    let ledger = init(
        &project,
        &GitRepo::new(&root),
        &SystemClock::new(),
        &templates,
        options,
    )
    .unwrap();

    assert_eq!(ledger.params.existing_project, Some(true));
    assert_eq!(ledger.params.verify_path(), ".tenaz/verify");
    assert_own_files_untouched(&project);
    assert_eq!(project.brief().unwrap(), "# Goal\n\nAdd a flag.\n");
    assert_eq!(project.protocol().unwrap(), templates.protocol);
    let added = git(&root, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(
        added.lines().all(|p| p.starts_with(".tenaz/")),
        "init committed outside .tenaz/: {added}"
    );
    assert!(added.contains(".tenaz/.gitignore"), "{added}");
    assert!(!added.contains(".tenaz/run.json"), "{added}");
    assert_eq!(git(&root, &["status", "--porcelain"]), "");

    let deps = deps(harness, &root, ledger.params.verify_path());
    let mut runner = Runner::open(project.clone(), deps, CancellationToken::new()).unwrap();
    runner
        .run(RunOptions {
            runner_version: "test".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let log = project.read(".tenaz/private/orchestrator.log").unwrap();
    assert_eq!(
        project.load_ledger().unwrap().phase,
        Phase::Complete,
        "run did not complete:\n{log}"
    );
    assert_after_the_run(&project);
}
