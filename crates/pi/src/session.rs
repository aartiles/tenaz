//! One Pi process in `--mode rpc`: commands are written to its stdin as JSON
//! lines, its stdout is read in a background task and translated into
//! session events, its stderr is appended to the session's stderr log.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use smr_core::ports::{HarnessError, PermissionDecision, Session, SessionExit, SessionSpec};
use smr_core::session::SessionEvent;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use crate::translate::Translator;

pub(crate) struct PiSession {
    child: Child,
    stdin: Option<ChildStdin>,
    events: mpsc::UnboundedReceiver<SessionEvent>,
    reader: Option<tokio::task::JoinHandle<()>>,
    prompts: u32,
}

impl PiSession {
    /// Spawns `binary args...` in `spec.cwd` and starts reading its stdout.
    pub(crate) fn spawn(
        binary: &Path,
        args: &[String],
        env: &[(String, String)],
        spec: &SessionSpec,
    ) -> Result<Self, HarnessError> {
        if let Some(dir) = &spec.persist_dir {
            std::fs::create_dir_all(dir).map_err(|e| {
                HarnessError::Spawn(format!("cannot create {}: {e}", dir.display()))
            })?;
        }
        let mut command = Command::new(binary);
        command
            .args(args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .map_err(|e| HarnessError::Spawn(format!("cannot start {}: {e}", binary.display())))?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Spawn("no stdout pipe".to_owned()))?;
        let stderr = child.stderr.take();
        let (tx, events) = mpsc::unbounded_channel();
        let raw_log = spec.raw_log.clone();
        let reader = tokio::spawn(read_stdout(stdout, raw_log, tx));
        if let Some(stderr) = stderr {
            tokio::spawn(drain_stderr(stderr, spec.stderr_log.clone()));
        }
        Ok(Self {
            child,
            stdin,
            events,
            reader: Some(reader),
            prompts: 0,
        })
    }

    async fn send(&mut self, command: &Value) -> Result<(), HarnessError> {
        let stdin = self.stdin.as_mut().ok_or(HarnessError::Closed)?;
        let mut line = command.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| HarnessError::Closed)?;
        stdin.flush().await.map_err(|_| HarnessError::Closed)
    }
}

/// Splits stdout on LF only (a JSON string may hold U+2028), appends every
/// line to the raw log and translates it.
async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    raw_log: Option<PathBuf>,
    tx: mpsc::UnboundedSender<SessionEvent>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut translator = Translator::new();
    let mut log = raw_log.and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| tracing::warn!("cannot open {}: {e}", path.display()))
            .ok()
    });
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            continue;
        }
        if let Some(log) = &mut log {
            let _ = std::io::Write::write_all(log, format!("{line}\n").as_bytes());
        }
        match serde_json::from_str::<Value>(line) {
            Ok(record) => {
                for event in translator.event(&record) {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }
            Err(error) => tracing::debug!("pi: not JSON ({error}): {line}"),
        }
    }
}

async fn drain_stderr(mut stderr: tokio::process::ChildStderr, log: Option<PathBuf>) {
    let mut file = log.and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });
    let mut buffer = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buffer).await {
        if n == 0 {
            break;
        }
        if let Some(file) = &mut file {
            let _ = std::io::Write::write_all(file, &buffer[..n]);
        }
    }
}

#[async_trait]
impl Session for PiSession {
    async fn prompt(&mut self, text: &str) -> Result<(), HarnessError> {
        let id = format!("prompt-{}", self.prompts);
        self.prompts += 1;
        self.send(&json!({"id": id, "type": "prompt", "message": text}))
            .await
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        self.events.recv().await
    }

    /// Pi delivers a steer after the current turn's tool calls, before the
    /// next model call.
    async fn steer(&mut self, text: &str) -> Result<(), HarnessError> {
        self.send(&json!({"type": "steer", "message": text})).await
    }

    /// Clears queued messages first, so the abort is not followed by them.
    async fn abort(&mut self) -> Result<(), HarnessError> {
        self.send(&json!({"type": "clear_queue"})).await?;
        self.send(&json!({"type": "abort"})).await
    }

    /// Pi's built-in tools run without asking in RPC mode; nothing to answer.
    async fn answer_permission(
        &mut self,
        _request_id: &str,
        _decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        Ok(())
    }

    /// Ends stdin (Pi's orderly shutdown), waits `grace`, then kills.
    async fn close(&mut self, grace: Duration) -> SessionExit {
        drop(self.stdin.take());
        let grace = grace.max(Duration::from_millis(50));
        let status = if let Ok(status) = tokio::time::timeout(grace, self.child.wait()).await {
            status
        } else {
            tracing::debug!("pi did not exit within {grace:?}; killing it");
            let _ = self.child.start_kill();
            self.child.wait().await
        };
        if let Some(reader) = self.reader.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), reader).await;
        }
        SessionExit {
            code: status.ok().and_then(|s| s.code()),
        }
    }
}
