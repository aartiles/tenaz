//! An agent that does not exit when its session is closed is killed,
//! with its process group, before `close` returns, even with no grace.
//! Tenaz then looks for what the session left (ADR 0039): while the
//! agent lived, what it started would still have a parent, and be
//! missed.

#![cfg(unix)]

use std::time::Duration;

use tenaz_acp::AcpHarness;
use tenaz_adapters::testing::{KillOnDrop, gone_within, write_executable};
use tenaz_core::ports::{Harness, HarnessInfo, SessionModel, SessionSpec};

/// A minimal ACP agent over stdio that writes its pid to `$PIDFILE` and, once
/// its input ends, keeps running.
const AGENT: &str = r#"#!/bin/sh
echo $$ > "$PIDFILE"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[^,}]*' | head -1 | cut -d: -f2)
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{}}}\n' "$id" ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"s1"}}\n' "$id" ;;
  esac
done
exec sleep 30
"#;

#[tokio::test]
async fn an_agent_that_outlives_its_close_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent.sh");
    let pidfile = dir.path().join("agent.pid");
    write_executable(&agent, AGENT);
    let harness = AcpHarness::subprocess(
        HarnessInfo {
            name: "sh".into(),
            version: "1".into(),
        },
        &agent,
        Vec::new(),
        vec![("PIDFILE".into(), pidfile.display().to_string())],
    );
    let spec = SessionSpec {
        label: "001-builder-T001".into(),
        cwd: dir.path().to_path_buf(),
        persist_dir: None,
        raw_log: None,
        thinking_log: None,
        stderr_log: None,
        model: SessionModel {
            name: "p/m".into(),
            id: "m".into(),
            context_window: 65536,
            supports_images: false,
        },
    };
    let mut session = harness.open(spec).await.expect("the handshake succeeds");
    let pid = KillOnDrop::read(&pidfile);
    session.close(Duration::ZERO).await;
    assert!(
        gone_within(pid.pid(), Duration::from_millis(500)),
        "the agent still runs after its session was closed"
    );
}
