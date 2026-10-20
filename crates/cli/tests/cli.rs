//! The binary end to end with the fake harness: params, init, run (through
//! the real wiring: proxy, git, verify), status.

use std::process::Command;

fn tenaz() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tenaz"))
}

#[test]
fn params_lists_the_defaults() {
    let out = tenaz().arg("params").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("maxAttempts=3\n"));
    assert!(text.contains("contextFloor=32768\n"));
}

/// `guide` prints the skill shipped with the binary, frontmatter first.
#[test]
fn guide_prints_the_skill() {
    let out = tenaz().arg("guide").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("---\nname: tenaz\ndescription: "),
        "{text}"
    );
    assert_eq!(text, include_str!("../skill/SKILL.md"));
}

#[test]
fn init_run_and_status_with_the_fake_harness() {
    let work = tempfile::tempdir().unwrap();
    let brief = work.path().join("brief.md");
    std::fs::write(&brief, "# Fake brief\n\nBuild four trivial files.\n").unwrap();
    let project = work.path().join("project");
    // Relative to the runner's directory, as typed: harnesses and `verify`
    // run inside the project, so the runner must not pass it on as is.
    let init = tenaz()
        .current_dir(work.path())
        .args(["init", "--project", "project"])
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
    let not_initialized = tenaz()
        .args(["status", "--project"])
        .arg(work.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&not_initialized.stdout),
        "Not initialized.\n"
    );

    let run = tenaz()
        .current_dir(work.path())
        .args(["run", "--project", "project"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    assert!(stderr.contains("Run finished: COMPLETE"), "{stderr}");
    assert!(
        stderr.contains("engine proxy at http://127.0.0.1:"),
        "{stderr}"
    );

    let status = tenaz()
        .args(["status", "--project"])
        .arg(&project)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        text.starts_with("Phase: COMPLETE\nState: complete\n"),
        "{text}"
    );
    assert!(text.contains("Tasks: done 5, split 1"), "{text}");
    assert!(
        text.contains("Model: fake/fake (thinking medium), fake fake-pi 0.0.1"),
        "{text}"
    );

    let again = tenaz()
        .args(["run", "--project"])
        .arg(&project)
        .output()
        .unwrap();
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("Run is COMPLETE"));

    let json = status_json(&project);
    assert_eq!(json["initialized"], true);
    assert_eq!(json["phase"], "COMPLETE");
    assert_eq!(json["state"], "complete");
    assert_eq!(json["pid"], serde_json::Value::Null);
    assert_eq!(json["tasks"]["done"], 5);
    assert_eq!(json["tasks"]["split"], 1);
    assert_eq!(json["current"], serde_json::Value::Null);
    assert_eq!(json["model"], "fake/fake");
    assert_eq!(status_json(work.path())["initialized"], false);

    // Each way a run stops short has its own exit code, also when it had
    // stopped before this call.
    let ledger_path = project.join(".tenaz/run.json");
    let ledger = std::fs::read_to_string(&ledger_path).unwrap();
    for (phase, reason, code, state) in [
        ("COMPLETE_WITH_GAPS", None, 3, "complete_with_gaps"),
        ("BLOCKED", Some("engine unavailable"), 4, "blocked"),
    ] {
        let mut value: serde_json::Value = serde_json::from_str(&ledger).unwrap();
        value["phase"] = phase.into();
        if let Some(reason) = reason {
            value["blocked_reason"] = reason.into();
        }
        std::fs::write(&ledger_path, value.to_string()).unwrap();
        let run = tenaz()
            .args(["run", "--project"])
            .arg(&project)
            .output()
            .unwrap();
        assert_eq!(
            run.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let json = status_json(&project);
        assert_eq!(json["state"], state);
        assert_eq!(
            json["blocked_reason"],
            reason.map_or(serde_json::Value::Null, Into::into)
        );
    }
}

fn status_json(project: &std::path::Path) -> serde_json::Value {
    let out = tenaz()
        .args(["status", "--json", "--project"])
        .arg(project)
        .output()
        .unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `init --harness pi` pins `pi --version` and reads the provider's base URL,
/// context window and inputs from `~/.pi/agent/models.json`.
#[test]
fn init_with_the_pi_harness_reads_models_json() {
    let work = tempfile::tempdir().unwrap();
    let home = work.path().join("home");
    std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
    std::fs::write(
        home.join(".pi/agent/models.json"),
        r#"{"providers": {"sushi": {"baseUrl": "http://127.0.0.1:4321/v1", "models": [
            {"id": "Qwen3.8", "contextWindow": 65536, "input": ["text", "image"]}]}}}"#,
    )
    .unwrap();
    // A stand-in for the pi executable: only `--version` is needed at init.
    let pi = work.path().join("pi");
    std::fs::write(
        &pi,
        "#!/bin/sh\n[ \"$1\" = --version ] && echo 9.9.9-test && exit 0\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let brief = work.path().join("brief.md");
    std::fs::write(&brief, "# Brief\n").unwrap();
    let project = work.path().join("project");

    let unknown = tenaz()
        .env("HOME", &home)
        .args(["init", "--project"])
        .arg(&project)
        .arg("--brief")
        .arg(&brief)
        .arg("--pi-bin")
        .arg(&pi)
        .args([
            "--harness",
            "pi",
            "--model",
            "Missing",
            "--provider",
            "sushi",
        ])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    assert!(
        String::from_utf8_lossy(&unknown.stderr)
            .contains("Model Missing is not listed under provider sushi"),
        "{}",
        String::from_utf8_lossy(&unknown.stderr)
    );

    let init = tenaz()
        .env("HOME", &home)
        .args(["init", "--project"])
        .arg(&project)
        .arg("--brief")
        .arg(&brief)
        .arg("--pi-bin")
        .arg(&pi)
        .args([
            "--harness",
            "pi",
            "--model",
            "Qwen3.8",
            "--provider",
            "sushi",
        ])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(
        String::from_utf8_lossy(&init.stdout).contains("sushi/Qwen3.8 (pi 9.9.9-test)"),
        "{}",
        String::from_utf8_lossy(&init.stdout)
    );
    let ledger: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".tenaz/run.json")).unwrap())
            .unwrap();
    assert_eq!(ledger["config"]["harness"], "pi");
    assert_eq!(ledger["config"]["harnessVersion"], "9.9.9-test");
    assert_eq!(ledger["config"]["baseUrl"], "http://127.0.0.1:4321/v1");
    assert_eq!(ledger["config"]["contextWindow"], 65536);
    assert_eq!(
        ledger["config"]["input"],
        serde_json::json!(["text", "image"])
    );

    let status = tenaz()
        .args(["status", "--project"])
        .arg(&project)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        text.contains("Model: sushi/Qwen3.8 (thinking medium), pi 9.9.9-test"),
        "{text}"
    );
}

/// A project in the `.agent/` layout of earlier builds is not run, and both
/// `run` and `status` say how to move it.
#[test]
fn an_old_agent_layout_is_refused_with_the_way_to_move_it() {
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join(".agent")).unwrap();
    std::fs::write(work.path().join(".agent/run.json"), "{}").unwrap();
    let run = tenaz()
        .args(["run", "--project"])
        .arg(work.path())
        .output()
        .unwrap();
    assert!(!run.status.success());
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("keeps its Tenaz state in .agent/"),
        "{stderr}"
    );
    assert!(stderr.contains("--refresh-templates"), "{stderr}");
    let status = tenaz()
        .args(["status", "--project"])
        .arg(work.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains("mv '"));
}
