//! A real agent process that dies mid-turn: a shell script answers
//! `initialize` and `session/new`, then writes to stderr and exits 1 when the
//! prompt arrives (as fx did in the Pocket Ledger run). The session ends
//! without settling, the exit code is 1, and the stderr is in the session's
//! stderr log.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use tenaz_acp::AcpHarness;
use tenaz_core::ports::{Harness, HarnessInfo, SessionSpec};
use tenaz_core::session::SessionEvent;

const AGENT: &str = r#"#!/bin/sh
# A minimal ACP agent over stdio that crashes on the first prompt.
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[^,}]*' | head -1 | cut -d: -f2)
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{}}}\n' "$id" ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1"}}\n' "$id" ;;
    *'"method":"session/prompt"'*)
      echo "fake agent: panic: reached unreachable code" >&2
      exit 1 ;;
  esac
done
"#;

#[tokio::test]
async fn an_agent_that_exits_mid_turn_leaves_its_stderr_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent.sh");
    std::fs::write(&agent, AGENT).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let harness = AcpHarness::subprocess(
        HarnessInfo {
            name: "sh".into(),
            version: "1".into(),
        },
        &agent,
        Vec::new(),
        Vec::new(),
    );
    let stderr_log = dir.path().join("001-builder-T001.stderr.log");
    let spec = SessionSpec {
        label: "001-builder-T001".into(),
        cwd: dir.path().to_path_buf(),
        persist_dir: None,
        raw_log: None,
        stderr_log: Some(stderr_log.clone()),
    };
    let mut session = harness.open(spec).await.expect("the handshake succeeds");
    session.prompt("build T001").await.unwrap();
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), session.next_event())
        .await
        .expect("the stream ends within 10 s")
    {
        events.push(event);
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::Settled { .. })),
        "{events:?}"
    );
    let exit = session.close(Duration::from_secs(5)).await;
    assert_eq!(exit.code, Some(1));
    let stderr = std::fs::read_to_string(&stderr_log).unwrap();
    assert!(
        stderr.contains("fake agent: panic: reached unreachable code"),
        "{stderr}"
    );
}
