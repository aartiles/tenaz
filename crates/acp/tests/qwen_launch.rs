//! How Qwen Code is launched for each session: a shell script stands in
//! for it, records its arguments and exits, so no session opens and no
//! agent runs.

#![cfg(unix)]

use std::time::Duration;

use tenaz_acp::qwen::QwenLaunch;
use tenaz_core::ports::{Harness, SessionModel, SessionSpec};

/// Qwen Code takes its model from its command line: `harness` launches
/// `session_command` for each session's model, so each session gets
/// `--model` with the id the engine lists for its model, also through a
/// wrapper (the sandbox launcher).
#[tokio::test]
async fn qwen_starts_each_session_on_its_model() {
    let dir = tempfile::tempdir().unwrap();
    let argv = dir.path().join("argv");
    let recorder = dir.path().join("record");
    let record = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 1\n",
        argv.display()
    );
    tenaz_adapters::testing::write_executable(&recorder, &record);
    let direct = QwenLaunch::new(&recorder, dir.path().join("home"), "http://127.0.0.1:1/v1");
    // Not run: only the wrapper is.
    let missing = "/nonexistent/qwen";
    let wrapped = QwenLaunch {
        binary: missing.into(),
        wrapper: vec![recorder.display().to_string(), "--sandboxed".into()],
        ..direct.clone()
    };
    let cases = [
        ("direct", direct, vec![]),
        ("wrapped", wrapped, vec!["--sandboxed", missing]),
    ];
    for (case, launch, leading) in cases {
        for id in ["critic", "m"] {
            let spec = SessionSpec {
                label: "001-builder-T001".into(),
                cwd: dir.path().to_path_buf(),
                persist_dir: None,
                raw_log: None,
                thinking_log: None,
                stderr_log: None,
                model: SessionModel {
                    name: format!("p/{id}"),
                    id: id.into(),
                    context_window: 65536,
                    supports_images: false,
                },
            };
            let opened = tokio::time::timeout(
                Duration::from_secs(10),
                launch.harness("1".into()).open(spec),
            )
            .await
            .expect("the launch ends");
            assert!(opened.is_err(), "{case}: the recorder is not an ACP agent");
            let text = std::fs::read_to_string(&argv).expect("the recorder ran");
            let args: Vec<&str> = text.lines().collect();
            assert_eq!(args[..leading.len()], leading[..], "{case} {id}: {args:?}");
            let model = args.windows(2).find(|pair| pair[0] == "--model");
            assert_eq!(model, Some(&["--model", id][..]), "{case}: {args:?}");
        }
    }
}
