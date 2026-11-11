//! A bootstrap written before the first run uses the same validation as
//! one written by the local model. No phase or journal edits are needed.

use super::*;
use crate::testing::VERIFY;

fn prepare(project: &Project) {
    for (path, text) in [
        (".tenaz/spec.md", "- R1: Create src/T001.txt.\n"),
        (".tenaz/map.md", "- src/T001.txt: the task's output\n"),
        ("verify", VERIFY),
        // This is a fixture understood by ScriptedVerify, not a real check.
        ("checks/T000.smoke.sh", "exit 0\n"),
        (".tenaz/notes/BOOTSTRAP.md", "One file per task.\n"),
        (
            ".tenaz/tasks/T001.md",
            "---\nid: T001\ntitle: Create the output\nstatus: todo\nrequires: [R1]\ndepends: []\nfiles: [src/T001.txt, checks/T001.exists.sh]\n---\n## Goal\nCreate src/T001.txt.\n## Acceptance\n- check T001.exists passes\n## Context\nWrite the output and its check in the mapped paths.\n",
        ),
    ] {
        project.write(path, text).unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_prepared_bootstrap_builds_without_bootstrap_sessions() {
    let work = tempfile::tempdir().unwrap();
    let (project, vcs) = fake_project(work.path(), &[]);
    prepare(&project);
    assert!(!project.exists("src/T001.txt"));
    assert!(!project.exists("checks/T001.exists.sh"));

    run(
        &project,
        deps(
            &project,
            vcs,
            Arc::new(ScriptedHarness::default()),
            Arc::new(VirtualClock::new()),
        ),
        CancellationToken::new(),
    )
    .await;

    assert_eq!(project.load_ledger().unwrap().phase, Phase::Complete);
    assert!(project.exists("src/T001.txt"));
    assert!(project.exists("checks/T001.exists.sh"));
    assert!(
        project
            .read(".tenaz/tasks/T001.md")
            .unwrap()
            .contains("status: done")
    );
    assert!(sessions(&project, Role::Builder).count() > 0);
    assert!(project.journal().iter().all(|entry| !matches!(
        &entry.record,
        JournalRecord::Session(session) if session.role == Role::Bootstrap
    )));
}

#[tokio::test(start_paused = true)]
async fn invalid_prepared_files_need_bootstrap_repair_before_building() {
    // Exercise the gate at each step: spec/map, verify, and task coverage.
    for (path, invalid) in [
        (".tenaz/map.md", None),
        ("checks/T000.smoke.sh", Some("exit 1\n")),
        (".tenaz/tasks/T001.md", None),
    ] {
        let work = tempfile::tempdir().unwrap();
        let (project, vcs) = fake_project(work.path(), &[]);
        prepare(&project);
        if let Some(text) = invalid {
            project.write(path, text).unwrap();
        } else {
            project.remove(path).unwrap();
        }
        run(
            &project,
            deps(
                &project,
                vcs,
                Arc::new(ScriptedHarness::default()),
                Arc::new(VirtualClock::new()),
            ),
            CancellationToken::new(),
        )
        .await;

        let journal = project.journal();
        let first_work = journal
            .iter()
            .find_map(|entry| match &entry.record {
                JournalRecord::Session(session)
                    if matches!(session.role, Role::Bootstrap | Role::Builder) =>
                {
                    Some(session)
                }
                _ => None,
            })
            .expect("a work session");
        assert_eq!(first_work.role, Role::Bootstrap, "invalid {path}");
        let repair = journal
            .iter()
            .position(|entry| {
                matches!(
                    &entry.record,
                    JournalRecord::BootstrapValidation { errors, .. } if errors.is_empty()
                )
            })
            .expect("the local bootstrap repaired the files");
        let build = journal
            .iter()
            .position(|entry| {
                matches!(
                    &entry.record,
                    JournalRecord::Session(session) if session.role == Role::Builder
                )
            })
            .expect("building follows repair");
        assert!(repair < build, "invalid {path}");
        assert_eq!(
            project.load_ledger().unwrap().phase,
            Phase::Complete,
            "invalid {path}"
        );
    }
}
