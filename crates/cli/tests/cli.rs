//! The binary end to end with the fake harness: params, init, run (through
//! the real wiring: proxy, git, verify), status.

use std::process::Command;

fn smr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_smr"))
}

#[test]
fn params_lists_the_defaults() {
    let out = smr().arg("params").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("maxAttempts=3\n"));
    assert!(text.contains("contextFloor=32768\n"));
}

#[test]
fn init_run_and_status_with_the_fake_harness() {
    let work = tempfile::tempdir().unwrap();
    let brief = work.path().join("brief.md");
    std::fs::write(&brief, "# Fake brief\n\nBuild four trivial files.\n").unwrap();
    let project = work.path().join("project");
    let init = smr()
        .args(["init", "--project"])
        .arg(&project)
        .arg("--brief")
        .arg(&brief)
        .args([
            "--harness",
            "fake",
            "--model",
            "fake",
            "--provider",
            "fake",
            "--set",
            "maxAttempts=2",
            "--set",
            "replanEvery=2",
            "--set",
            "engineRetryBaseSec=0",
            "--set",
            "notify=false",
        ])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(String::from_utf8_lossy(&init.stdout).contains("fake fake-pi 0.0.1"));
    let not_initialized = smr()
        .args(["status", "--project"])
        .arg(work.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&not_initialized.stdout),
        "Not initialized.\n"
    );

    let run = smr()
        .args(["run", "--project"])
        .arg(&project)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    assert!(stderr.contains("Run finished: COMPLETE"), "{stderr}");
    assert!(
        stderr.contains("engine proxy at http://127.0.0.1:"),
        "{stderr}"
    );

    let status = smr()
        .args(["status", "--project"])
        .arg(&project)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.starts_with("Phase: COMPLETE\n"), "{text}");
    assert!(text.contains("Tasks: done 5, split 1"), "{text}");
    assert!(
        text.contains("Model: fake/fake (thinking medium), fake fake-pi 0.0.1"),
        "{text}"
    );

    let again = smr()
        .args(["run", "--project"])
        .arg(&project)
        .output()
        .unwrap();
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("Run is COMPLETE"));
}
