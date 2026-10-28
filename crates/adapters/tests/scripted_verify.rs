//! `ScriptedVerify` answers as the scripted harness's real `verify` does:
//! the same tree, run through `sh` and read by the fake, gives the same
//! report.

use std::time::Duration;

use tenaz_adapters::testing::write_executable;
use tenaz_adapters::verify::VerifyProcess;
use tenaz_core::ports::VerifyRunner;
use tenaz_core::testing::{ScriptedVerify, VERIFY};

fn report(stdout: &str) -> serde_json::Value {
    serde_json::from_str(stdout.trim()).unwrap()
}

/// Runs `args` through the real `verify` and the fake, and compares them.
async fn same_answers(root: &std::path::Path, cases: &[&[&str]]) {
    let real = VerifyProcess::new(root);
    let fake = ScriptedVerify::new(root);
    for args in cases {
        let want = real.run(args, Duration::from_secs(10)).await;
        let got = fake.run(args, Duration::from_secs(10)).await;
        assert_eq!(got.status, want.status, "{args:?}");
        assert_eq!(report(&got.stdout), report(&want.stdout), "{args:?}");
    }
}

#[tokio::test]
async fn without_checks_both_fail_on_the_bare_glob() {
    let dir = tempfile::tempdir().unwrap();
    write_executable(&dir.path().join("verify"), VERIFY);
    same_answers(dir.path(), &[&["--all"], &["--task", "T001"]]).await;
}

#[tokio::test]
async fn the_fake_reports_what_the_real_verify_reports() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_executable(&root.join("verify"), VERIFY);
    std::fs::create_dir_all(root.join("checks")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/T001.txt"), "").unwrap();
    for (name, body) in [
        ("T000.smoke.sh", "exit 0\n"),
        ("T001.exists.sh", "test -f src/T001.txt\n"),
        ("T002.exists.sh", "test -f src/T002.txt\n"),
        ("T003.exists.sh", "exit 1\n"),
    ] {
        std::fs::write(root.join("checks").join(name), body).unwrap();
    }
    same_answers(
        root,
        &[
            &["--all"],
            &["--task", "T001"],
            &["--task", "T002"],
            &["--task", "T009"],
        ],
    )
    .await;
}
