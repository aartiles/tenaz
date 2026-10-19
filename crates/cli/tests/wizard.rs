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
        std::fs::read_to_string(project.join("BRIEF.md")).unwrap(),
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
    // is saved there.
    let run = tenaz()
        .current_dir(&project)
        .args(["run", "--media-timeout", "99"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    assert!(stderr.contains("Run finished: COMPLETE"), "{stderr}");
    let ledger: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join(".tenaz/run.json")).unwrap())
            .unwrap();
    assert_eq!(ledger["launch"]["mediaTimeoutSec"], 99);
    assert_eq!(ledger["launch"]["comfyui"], origin);
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
