//! The setup wizard end to end: `tenaz init` answered through a piped
//! stdin against a stand-in engine and `ComfyUI`, then `tenaz run` with no
//! launch flags, reading everything from `run.json`.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

fn tenaz() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tenaz"))
}

/// A tiny HTTP server: an OpenAI-compatible model list with two models and
/// `ComfyUI`'s `/system_stats`. Returns its origin.
fn stand_in_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or_default();
            let (status, body) = match path {
                "/v1/models" => (
                    "200 OK",
                    r#"{"data": [
                        {"id": "alpha", "owned_by": "someone", "max_model_len": 65536},
                        {"id": "beta", "owned_by": "someone"}
                    ]}"#,
                ),
                "/system_stats" => ("200 OK", "{}"),
                _ => ("404 Not Found", "{}"),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    origin
}

#[test]
fn the_wizard_pins_everything_run_needs() {
    let origin = stand_in_server();
    let work = tempfile::tempdir().unwrap();
    let workflow = work.path().join("txt2img.json");
    std::fs::write(&workflow, r#"{"1": {"inputs": {"text": "{{prompt}}"}}}"#).unwrap();

    let mut child = tenaz()
        .current_dir(work.path())
        .args(["init", "--interactive", "--harness", "fake"])
        .args(["--base-url", &format!("{origin}/v1")])
        .args(["--comfyui", &origin])
        .args(["--set", "engineRetryBaseSec=0", "--set", "notify=false"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let answers = [
        "Build four trivial files.", // goal
        "2",                         // where: a new folder, not this one
        "project",                   // project directory
        "2",                         // model: beta
        "40000",                     // context window, not reported for beta
        "n",                         // reads images
        "missing.json",              // workflow: refused
        workflow.to_str().unwrap(),  // workflow
        "",                          // stop command: keep the model loaded
        "",                          // start command
        "",                          // create: yes
    ];
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", answers.join("\n")).as_bytes())
        .unwrap();
    let init = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&init.stderr);
    assert!(init.status.success(), "{stderr}");
    assert!(stderr.contains("Which model?"), "{stderr}");
    assert!(stderr.contains("Cannot use it"), "{stderr}");
    assert!(stderr.contains("The same without questions:"), "{stderr}");
    assert!(stderr.contains("--model beta"), "{stderr}");
    assert!(stderr.contains("--engine generic"), "{stderr}");
    let stdout = String::from_utf8_lossy(&init.stdout);
    assert!(stdout.contains("Next: tenaz run --project"), "{stdout}");

    let project = work.path().join("project");
    assert_eq!(
        std::fs::read_to_string(project.join(".tenaz/brief.md")).unwrap(),
        "# Goal\n\nBuild four trivial files.\n"
    );
    let ledger: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".tenaz/run.json")).unwrap())
            .unwrap();
    assert_eq!(ledger["config"]["model"], "local/beta");
    assert_eq!(ledger["config"]["contextWindow"], 40000);
    assert_eq!(ledger["config"]["baseUrl"], format!("{origin}/v1"));
    let launch = &ledger["launch"];
    assert_eq!(launch["engine"], "generic");
    assert_eq!(launch["comfyui"], origin);
    assert_eq!(launch["workflows"]["default"], workflow.to_str().unwrap());
    assert!(launch.get("engineStop").is_none(), "{launch}");

    // No launch flags: run reads them from run.json, and a flag given now
    // is saved there. Started from the folder init ran in, run finds the
    // project in its subfolder.
    let run = tenaz()
        .current_dir(work.path())
        .args(["run", "--media-timeout", "99"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    assert!(stderr.contains("Using the project in"), "{stderr}");
    assert!(stderr.contains("Run finished: COMPLETE"), "{stderr}");
    let ledger: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".tenaz/run.json")).unwrap())
            .unwrap();
    assert_eq!(ledger["launch"]["mediaTimeoutSec"], 99);
    assert_eq!(ledger["launch"]["comfyui"], origin);
}

/// A stand-in for the pi executable: only `--version` is needed at init.
fn stand_in_pi(dir: &std::path::Path) -> std::path::PathBuf {
    let pi = dir.join("pi");
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
    pi
}

/// Pi needs nothing in `~/.pi/agent`: with no `models.json` the wizard finds
/// the engine and its models as it does for fx. With one, its models are
/// offered first, and the last choice goes to the engine instead.
#[test]
fn the_wizard_sets_up_pi_without_its_models_json() {
    let origin = stand_in_server();
    let work = tempfile::tempdir().unwrap();
    let pi = stand_in_pi(work.path());
    let home = work.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let init = |project: &str, answers: &[&str]| {
        let mut child = tenaz()
            .current_dir(work.path())
            .env("HOME", &home)
            .args(["init", "--interactive", "--harness", "pi"])
            .arg("--pi-bin")
            .arg(&pi)
            .args(["--base-url", &format!("{origin}/v1"), "--project", project])
            .args(["--set", "notify=false"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{}\n", answers.join("\n")).as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{stderr}");
        let ledger: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(work.path().join(project).join(".tenaz/run.json")).unwrap(),
        )
        .unwrap();
        (stderr, ledger)
    };
    let answers = [
        "Build four trivial files.", // goal
        "1",                         // model: alpha, 65536 tokens reported
        "n",                         // reads images
        "",                          // create: yes
    ];

    let (stderr, ledger) = init("fresh", &answers);
    assert!(stderr.contains("Which model?"), "{stderr}");
    assert!(stderr.contains("Context window: 65536 tokens"), "{stderr}");
    assert_eq!(ledger["config"]["harness"], "pi");
    assert_eq!(ledger["config"]["model"], "local/alpha");
    assert_eq!(ledger["config"]["baseUrl"], format!("{origin}/v1"));
    assert_eq!(ledger["config"]["contextWindow"], 65536);

    std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
    std::fs::write(
        home.join(".pi/agent/models.json"),
        format!(
            r#"{{"providers": {{
                "mine": {{"baseUrl": "{origin}/v1", "models": [{{"id": "beta", "contextWindow": 50000}}]}},
                "elsewhere": {{"baseUrl": "http://127.0.0.1:9/v1", "models": [{{"id": "far"}}]}}
            }}}}"#
        ),
    )
    .unwrap();
    let (stderr, ledger) = init("listed", &["Build four trivial files.", "", ""]);
    assert!(stderr.contains("mine/beta, 50000 tokens"), "{stderr}");
    assert!(!stderr.contains("elsewhere/far"), "{stderr}");
    assert_eq!(ledger["config"]["model"], "mine/beta");
    assert_eq!(ledger["config"]["contextWindow"], 50000);

    let mut answers = answers.to_vec();
    answers.insert(1, "2"); // another model, from the engine
    let (_, ledger) = init("engine", &answers);
    assert_eq!(ledger["config"]["model"], "local/alpha");
}

#[test]
fn without_a_terminal_init_needs_the_model() {
    let work = tempfile::tempdir().unwrap();
    let out = tenaz()
        .args(["init", "--project"])
        .arg(work.path().join("p"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--model is required"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// On a repository with commits the wizard asks where to work, and the
/// default is a new branch in a worktree next to it.
#[test]
fn the_wizard_offers_a_worktree_on_an_existing_repository() {
    let origin = stand_in_server();
    let work = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(work.path()).unwrap();
    let repo = root.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(&repo)
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
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "# App\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "first"]);
    let workflow = root.join("txt2img.json");
    std::fs::write(&workflow, r#"{"1": {"inputs": {"text": "{{prompt}}"}}}"#).unwrap();

    let mut child = tenaz()
        .args(["init", "--interactive", "--harness", "fake", "--project"])
        .arg(&repo)
        .args(["--base-url", &format!("{origin}/v1")])
        .args(["--comfyui", &origin])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let answers = [
        "Add a login page.",        // goal
        "",                         // where: a worktree
        "2",                        // model: beta
        "40000",                    // context window
        "n",                        // reads images
        workflow.to_str().unwrap(), // workflow
        "",                         // stop command
        "",                         // start command
        "",                         // create: yes
    ];
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", answers.join("\n")).as_bytes())
        .unwrap();
    let init = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&init.stderr);
    assert!(init.status.success(), "{stderr}");
    assert!(
        stderr.contains("is a git repository. Where should Tenaz work?"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Tenaz will work on tenaz/add-a-login-page"),
        "{stderr}"
    );
    let worktree = root.join("app-tenaz-add-a-login-page");
    assert_eq!(
        std::fs::read_to_string(worktree.join(".tenaz/brief.md")).unwrap(),
        "# Goal\n\nAdd a login page.\n"
    );
    assert!(!repo.join(".tenaz/brief.md").exists());
    assert!(!repo.join(".tenaz").exists());
    assert_eq!(git(&["status", "--porcelain"]), "");
    let stdout = String::from_utf8_lossy(&init.stdout);
    assert!(stdout.contains("app-tenaz-add-a-login-page"), "{stdout}");
}

/// Started in a folder that already holds files, the wizard offers to work
/// there, and those files become the run's starting point.
#[test]
fn the_wizard_offers_the_folder_it_runs_in() {
    let origin = stand_in_server();
    let work = tempfile::tempdir().unwrap();
    let here = std::fs::canonicalize(work.path()).unwrap();
    std::fs::write(
        here.join("extracto.csv"),
        "fecha,importe\n2026-07-01,-12.50\n",
    )
    .unwrap();

    let mut child = tenaz()
        .current_dir(&here)
        .args(["init", "--interactive", "--harness", "fake"])
        .args(["--base-url", &format!("{origin}/v1")])
        .args(["--set", "notify=false"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let answers = [
        "Sum the expenses.", // goal
        "",                  // where: here
        "1",                 // model: alpha
        "n",                 // reads images
        "",                  // ComfyUI: none
        "",                  // create: yes
    ];
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n", answers.join("\n")).as_bytes())
        .unwrap();
    let init = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&init.stderr);
    assert!(init.status.success(), "{stderr}");
    assert!(stderr.contains("Where should Tenaz work?"), "{stderr}");
    assert!(stderr.contains("(1 item)"), "{stderr}");
    assert!(stderr.contains("Tenaz makes it one"), "{stderr}");
    assert!(here.join(".tenaz/run.json").is_file());
    let tracked = Command::new("git")
        .current_dir(&here)
        .args(["ls-files"])
        .output()
        .unwrap();
    let tracked = String::from_utf8_lossy(&tracked.stdout);
    assert!(tracked.lines().any(|l| l == "extracto.csv"), "{tracked}");
}
