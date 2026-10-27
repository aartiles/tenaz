//! Fakes for tests: a scripted harness that exercises every orchestrator
//! path without a model (a fixed session script), a clock that runs faster
//! than real time, replayed memory readings and engine counters, the
//! scripted harness's `verify` judged without a shell, and a repository kept
//! as snapshots in memory with the contract that holds it to git.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

mod notifier;
mod replay;
mod vcs;
pub mod vcs_contract;
mod verify;

pub use notifier::SilentNotifier;
pub use replay::Replay;
pub use vcs::SnapshotVcs;
pub use vcs_contract::MakeVcs;
pub use verify::ScriptedVerify;

use crate::ports::{
    Clock, Harness, HarnessError, HarnessInfo, PermissionDecision, Session, SessionExit,
    SessionSpec,
};
use crate::session::{SessionEvent, StopReason, Usage};

/// A clock that runs `speed` times faster than real time, so backoffs and
/// grace periods in tests take milliseconds.
#[derive(Debug)]
pub struct FastClock {
    started: std::time::Instant,
    speed: u32,
}

impl FastClock {
    pub fn new(speed: u32) -> Self {
        Self {
            started: std::time::Instant::now(),
            speed: speed.max(1),
        }
    }
}

#[async_trait]
impl Clock for FastClock {
    fn now_rfc3339(&self) -> String {
        let now = time::OffsetDateTime::now_utc();
        now.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    }

    fn monotonic_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
            * u64::from(self.speed)
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration / self.speed).await;
    }
}

/// The scripted stand-in for an agent harness. It plays a fixed script per
/// role: bootstrap (with two engine failures first), a review rejection, a
/// tool-call loop that must be steered and aborted, an alternating loop after
/// finished work, a task that fails until it is split, a replan, out-of-role
/// edits that must be reverted, and the final review.
#[derive(Clone, Debug, Default)]
pub struct ScriptedHarness {
    /// A builder session for this task waits before acting, so a test can pause the run.
    pub slow_task: Option<String>,
    pub slow_for: Duration,
    /// The first builder session for this task dies mid-turn between tool
    /// calls: it writes to stderr and exits with code 1.
    pub crash_task: Option<String>,
    /// The first builder session for this task dies while a shell call runs,
    /// as fx does on output that is not UTF-8.
    pub crash_in_tool_task: Option<String>,
    /// Where bootstrap writes `verify`; `None` is `verify` at the root.
    pub verify_path: Option<String>,
    /// Every builder session for this task grows its context past the stop
    /// without writing anything, so its attempt outgrows the budget.
    pub outgrow_task: Option<String>,
    /// The first builder session for this task that does the work leaves
    /// out its check, and adds it when the orchestrator asks.
    pub forget_checks_task: Option<String>,
    /// The first builder session for this task reasons until its reply is
    /// cut at the output limit, and the harness exits, as fx does.
    pub truncate_task: Option<String>,
    /// The first bootstrap session after the engine failures does the same.
    pub truncate_bootstrap: bool,
    /// The first bootstrap session writes the spec and a new file, then
    /// ends without the rest.
    pub partial_bootstrap: bool,
    /// Each bootstrap session writes only the files of the step its prompt
    /// names. Otherwise the first one writes them all.
    pub stepwise_bootstrap: bool,
    /// The first review of this task reads until its context is stopped.
    pub outgrow_review: Option<String>,
    /// The first review of this task asks for changes with a MINOR finding.
    pub minor_review: Option<String>,
    /// The builder of this task also writes an image, `assets/<task>.png`.
    pub image_task: Option<String>,
    /// The first attempt of this task deletes T001's check; later ones put
    /// it back.
    pub drop_check_task: Option<String>,
    /// The first builder session of this task keeps writing new files until
    /// it is stopped at its turn budget.
    pub long_task: Option<String>,
    /// The first `hang_times` builder turns of this task start a shell call
    /// that never ends until the turn is aborted.
    pub hang_task: Option<String>,
    pub hang_times: u32,
}

impl ScriptedHarness {
    /// The version pinned in the ledger of a fake run.
    pub fn version(&self) -> String {
        "fake-pi 0.0.1".to_owned()
    }
}

#[async_trait]
impl Harness for ScriptedHarness {
    fn info(&self) -> HarnessInfo {
        HarnessInfo {
            name: "fake".to_owned(),
            version: self.version(),
        }
    }

    async fn open(&self, spec: SessionSpec) -> Result<Box<dyn Session>, HarnessError> {
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Box::new(ScriptedSession {
            cwd: spec.cwd,
            raw_log: spec.raw_log,
            stderr_log: spec.stderr_log,
            exit_code: Some(0),
            tx,
            rx,
            aborted: Arc::new(AtomicBool::new(false)),
            prompts: 0,
            first: String::new(),
            role: String::new(),
            play: None,
            script: self.clone(),
        }))
    }
}

struct ScriptedSession {
    cwd: PathBuf,
    raw_log: Option<PathBuf>,
    stderr_log: Option<PathBuf>,
    exit_code: Option<i32>,
    tx: mpsc::UnboundedSender<SessionEvent>,
    rx: mpsc::UnboundedReceiver<SessionEvent>,
    aborted: Arc<AtomicBool>,
    prompts: u32,
    /// The first prompt, which a prompt after a stopped tool call continues.
    first: String,
    /// The `Role` field of the first prompt.
    role: String,
    play: Option<tokio::task::JoinHandle<()>>,
    script: ScriptedHarness,
}

#[async_trait]
impl Session for ScriptedSession {
    async fn prompt(&mut self, text: &str) -> Result<(), HarnessError> {
        self.prompts += 1;
        self.aborted.store(false, Ordering::SeqCst);
        if self.first.is_empty() {
            text.clone_into(&mut self.first);
        }
        // After a stopped tool call the session goes on with its first task.
        let text = if text.contains("was stopped") {
            self.first.clone()
        } else {
            text.to_owned()
        };
        if let Some(role) = field(&text, "Role")
            && self.role.is_empty()
        {
            role.clone_into(&mut self.role);
        }
        let player = Player {
            cwd: self.cwd.clone(),
            stderr_log: self.stderr_log.clone(),
            tx: self.tx.clone(),
            aborted: self.aborted.clone(),
            prompts: self.prompts,
            call: 0,
            script: self.script.clone(),
        };
        let prompt = text;
        self.play = Some(tokio::spawn(async move { player.play(&prompt).await }));
        Ok(())
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        let event = self.rx.recv().await?;
        if let SessionEvent::Exited { code } = event {
            self.exit_code = code;
        }
        if let Some(log) = &self.raw_log
            && let Ok(line) = serde_json::to_string(&event)
        {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .and_then(|mut f| {
                    std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes())
                });
        }
        Some(event)
    }

    /// Steers are appended to `.tenaz/private/fake-steers.log`, after the role.
    async fn steer(&mut self, text: &str) -> Result<(), HarnessError> {
        let log = self.cwd.join(".tenaz/private/fake-steers.log");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .and_then(|mut f| {
                std::io::Write::write_all(&mut f, format!("{}: {text}\n", self.role).as_bytes())
            });
        Ok(())
    }

    async fn abort(&mut self) -> Result<(), HarnessError> {
        self.aborted.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn answer_permission(
        &mut self,
        _request_id: &str,
        _decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        Ok(())
    }

    async fn close(&mut self, _grace: Duration) -> SessionExit {
        if let Some(play) = self.play.take() {
            let _ = play.await;
        }
        SessionExit {
            code: self.exit_code,
        }
    }
}

struct Player {
    cwd: PathBuf,
    stderr_log: Option<PathBuf>,
    tx: mpsc::UnboundedSender<SessionEvent>,
    aborted: Arc<AtomicBool>,
    prompts: u32,
    call: u32,
    script: ScriptedHarness,
}

fn field<'a>(prompt: &'a str, name: &str) -> Option<&'a str> {
    prompt.lines().find_map(|l| {
        l.strip_prefix(name)
            .and_then(|r| r.strip_prefix(": "))
            .or_else(|| l.strip_prefix(name).and_then(|r| r.strip_prefix(':')))
            .map(str::trim)
    })
}

/// The `verify` the scripted harness writes at bootstrap.
pub const VERIFY: &str = r#"#!/bin/sh
# Runs checks/<ID>.<name>.sh; --task T007 selects the T007. prefix.
prefix=""
[ "$1" = "--task" ] && prefix="$2."
passed=0; failed=""
for f in checks/*.sh; do
  id=$(basename "$f" .sh)
  case "$id" in "$prefix"*) ;; *) continue ;; esac
  if sh "$f" >/dev/null 2>&1; then passed=$((passed+1)); else failed="$failed{\"id\":\"$id\",\"msg\":\"failed\"},"; fi
done
failed=${failed%,}
if [ -z "$failed" ]; then echo "{\"ok\":true,\"passed\":$passed,\"failed\":[]}"; exit 0; fi
echo "{\"ok\":false,\"passed\":$passed,\"failed\":[$failed]}"; exit 1
"#;

fn card(id: &str, requires: &[&str], depends: &[&str], parent: Option<&str>) -> String {
    format!(
        "---\nid: {id}\ntitle: Fake task {id}\nstatus: todo\nrequires: [{}]\ndepends: [{}]\n{}files: [src/{id}.txt]\n---\n## Goal\nCreate src/{id}.txt.\n## Acceptance\n- check {id}.exists passes\n",
        requires.join(", "),
        depends.join(", "),
        parent.map(|p| format!("parent: {p}\n")).unwrap_or_default()
    )
}

impl Player {
    fn emit(&self, event: SessionEvent) {
        let _ = self.tx.send(event);
    }

    fn aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Per-project counters, so repeated sessions take different branches.
    fn bump(&self, key: &str) -> u32 {
        let path = self.cwd.join(".tenaz/private/fake-state.json");
        let mut state: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let next = state
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            + 1;
        state.insert(key.to_owned(), serde_json::json!(next));
        if path.parent().is_some_and(Path::exists) {
            let _ = std::fs::write(&path, serde_json::Value::Object(state).to_string());
        }
        u32::try_from(next).unwrap_or(u32::MAX)
    }

    fn write(&self, relative: &str, text: &str, executable: bool) {
        let path = self.cwd.join(relative);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, text);
        if executable {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
        }
    }

    async fn turn(
        &mut self,
        tool: &str,
        args: serde_json::Value,
        effect: impl FnOnce(&Self),
        result: &str,
        input: Option<u64>,
    ) {
        self.call += 1;
        let id = format!("call_{}", self.call);
        let input = input.unwrap_or(1000 + u64::from(self.call) * 10);
        self.emit(SessionEvent::TurnStart);
        self.emit(SessionEvent::ToolCallStart {
            id: id.clone(),
            name: tool.to_owned(),
            args: args.clone(),
        });
        effect(self);
        self.emit(SessionEvent::ToolCallEnd {
            id,
            name: tool.to_owned(),
            result_text: result.to_owned(),
            is_error: false,
        });
        self.emit(SessionEvent::MessageEnd {
            server: None,
            // What a proxy that saw the request would report.
            sent: Some(crate::session::RequestSettings {
                reasoning_effort: Some("medium".to_owned()),
                max_tokens: Some(16_384),
                ..Default::default()
            }),
            usage: Usage {
                input,
                output: 20,
                ..Default::default()
            },
            stop_reason: StopReason::ToolUse,
            error_message: None,
            thinking_chars: 4,
            text: String::new(),
        });
        self.emit(SessionEvent::TurnEnd);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    async fn write_tool(&mut self, path: &str, text: &str, executable: bool) {
        let (p, t) = (path.to_owned(), text.to_owned());
        self.turn(
            "write",
            serde_json::json!({ "path": path }),
            move |me| me.write(&p, &t, executable),
            "ok",
            None,
        )
        .await;
    }

    fn finish(&self, text: &str) {
        let aborted = self.aborted();
        self.emit(SessionEvent::MessageEnd {
            server: None,
            sent: None,
            usage: Usage {
                input: 1200,
                output: 10,
                cache_read: if self.prompts > 1 { 900 } else { 0 },
                ..Default::default()
            },
            stop_reason: if aborted {
                StopReason::Aborted
            } else {
                StopReason::Stop
            },
            error_message: None,
            thinking_chars: 7,
            text: text.to_owned(),
        });
        self.emit(SessionEvent::TurnEnd);
        self.emit(SessionEvent::Settled { aborted });
    }

    fn engine_error(&self, message: &str) {
        self.emit(SessionEvent::MessageEnd {
            server: None,
            sent: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some(message.to_owned()),
            thinking_chars: 0,
            text: String::new(),
        });
        self.emit(SessionEvent::TurnEnd);
        self.emit(SessionEvent::Settled { aborted: false });
    }

    async fn play(mut self, prompt: &str) {
        if let Some(task) = prompt
            .split("runs no passing check with ID prefix `")
            .nth(1)
            .and_then(|rest| rest.split('.').next())
        {
            self.write_check(task).await;
            return self.finish("Added the check.");
        }
        let Some(role) = field(prompt, "Role").map(str::to_owned) else {
            if prompt.contains(crate::orchestrator::TOOL_PROBE_FILE) {
                let printed =
                    std::fs::read_to_string(self.cwd.join(crate::orchestrator::TOOL_PROBE_FILE))
                        .unwrap_or_default();
                return self.finish(printed.trim());
            }
            return self.finish("323");
        };
        let task = field(prompt, "Task").map(str::to_owned).unwrap_or_default();
        let attempt: u32 = field(prompt, "Attempt")
            .and_then(|a| a.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(1);
        match role.as_str() {
            "BOOTSTRAP" => {
                let step = field(prompt, "Step")
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(1);
                self.bootstrap(step).await;
            }
            "BUILDER" => self.builder(&task, attempt).await,
            "REVIEWER" => {
                let file = field(prompt, "Review file")
                    .unwrap_or(".tenaz/reviews/unknown.md")
                    .to_owned();
                let n = self.bump(&format!("review-{task}"));
                if self.script.outgrow_review.as_deref() == Some(task.as_str()) && n == 1 {
                    return self.outgrow().await;
                }
                if task == "T004" && n == 1 {
                    return self.finish("(no verdict written)"); // must be retried
                }
                let verdict = if task == "T001" && n == 1 {
                    "Verdict: CHANGES\n- [MAJOR] fake finding\n"
                } else if self.script.minor_review.as_deref() == Some(task.as_str()) && n == 1 {
                    "Verdict: CHANGES\n- [MINOR] a decision is not recorded\n"
                } else {
                    "Verdict: ACCEPT\n"
                };
                self.write_tool(&file, verdict, false).await;
                self.finish("Done.");
            }
            "SPLIT" => {
                self.write_tool(
                    &format!(".tenaz/tasks/{task}a.md"),
                    &card(&format!("{task}a"), &["R3"], &["T001"], Some(&task)),
                    false,
                )
                .await;
                self.write_tool(
                    &format!(".tenaz/tasks/{task}b.md"),
                    &card(
                        &format!("{task}b"),
                        &["R3"],
                        &[&format!("{task}a")],
                        Some(&task),
                    ),
                    false,
                )
                .await;
                self.write_tool(".tenaz/notes/PLANNER.md", "Split.\n", false)
                    .await;
                self.finish("Done.");
            }
            "REPLAN" => {
                self.write_tool(".tenaz/notes/PLANNER.md", "Plan is fine.\n", false)
                    .await;
                if self.cwd.join(".tenaz/tasks/T001.md").exists() {
                    self.write_tool(".tenaz/tasks/T001.md", "tampered done card\n", false)
                        .await; // must be reverted
                }
                self.finish("Done.");
            }
            "FINAL-REVIEWER" => {
                self.write_tool(".tenaz/reviews/FINAL.md", "Verdict: ACCEPT\n", false)
                    .await;
                self.finish("Done.");
            }
            other => self.finish(&format!("unknown role {other}")),
        }
    }

    /// Dies mid-turn, during a shell call when `in_tool`: stderr, then exit 1.
    async fn crash(&mut self, in_tool: bool) {
        self.emit(SessionEvent::TurnStart);
        let stderr = if in_tool {
            self.call += 1;
            self.emit(SessionEvent::ToolCallStart {
                id: format!("call_{}", self.call),
                name: "shell".to_owned(),
                args: serde_json::json!({ "command": "head -c 400 checks/result.json" }),
            });
            "Error: stream did not contain valid UTF-8\n"
        } else {
            "fake harness: panic: reached unreachable code\n"
        };
        if let Some(log) = &self.stderr_log {
            let _ = std::fs::write(log, stderr);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.emit(SessionEvent::Exited { code: Some(1) });
    }

    /// A shell call that runs until the turn is aborted, as a script in an
    /// endless loop would.
    async fn hang(&mut self) {
        self.call += 1;
        let id = format!("call_{}", self.call);
        self.emit(SessionEvent::TurnStart);
        self.emit(SessionEvent::ToolCallStart {
            id: id.clone(),
            name: "bash".to_owned(),
            args: serde_json::json!({ "command": "./verify --all" }),
        });
        while !self.aborted() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.emit(SessionEvent::ToolCallEnd {
            id,
            name: "bash".to_owned(),
            result_text: "Command aborted".to_owned(),
            is_error: true,
        });
        self.finish("Stopped.");
    }

    /// One reply that reasons until it is cut at the output limit, then the
    /// harness exits with code 1, as fx does on `OutputTruncated`.
    async fn truncate(&mut self) {
        self.emit(SessionEvent::TurnStart);
        self.emit(SessionEvent::MessageEnd {
            server: None,
            sent: None,
            usage: Usage {
                input: 1000,
                output: 16384,
                ..Default::default()
            },
            stop_reason: StopReason::MaxTokens,
            error_message: None,
            thinking_chars: 60_000,
            text: String::new(),
        });
        if let Some(log) = &self.stderr_log {
            let _ = std::fs::write(log, "the agent connection ended: OutputTruncated\n");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.emit(SessionEvent::Exited { code: Some(1) });
    }

    async fn write_check(&mut self, task: &str) {
        self.write_tool(
            &format!("checks/{task}.exists.sh"),
            &format!("test -f src/{task}.txt\n"),
            false,
        )
        .await;
    }

    async fn bootstrap(&mut self, step: u32) {
        let boot = self.bump("bootstrap");
        if boot == 1 || boot == 2 {
            // Engine out of memory, then a prompt over its current maximum: rerun, not counted.
            let message = if boot == 1 {
                "400: not enough GPU memory".to_owned()
            } else {
                "400: {\"message\":\"Prompt (98353 tokens) requires ~4406MB GPU memory but only ~3494MB is available. Maximum context at KV8 and prefill chunk 512: 61488 tokens.\"}".to_owned()
            };
            let big = "x".repeat(40_000); // about 10k tokens: steered
            self.turn(
                "read",
                serde_json::json!({ "path": "big.md" }),
                |_| {},
                &big,
                None,
            )
            .await;
            return self.engine_error(&message);
        }
        if self.script.truncate_bootstrap && self.bump("truncate-bootstrap") == 1 {
            return self.truncate().await;
        }
        if self.script.partial_bootstrap && self.bump("partial-bootstrap") == 1 {
            self.write_tool(".tenaz/spec.md", "# Spec\n\n- R1: one\n", false)
                .await;
            self.write_tool("index.html", "<h1>draft</h1>\n", false)
                .await;
            return self.finish("Out of time.");
        }
        let only = self.script.stepwise_bootstrap;
        if !only || step == 1 {
            self.bootstrap_spec().await;
        }
        if !only || step == 2 {
            self.bootstrap_verify().await;
        }
        if !only || step == 3 {
            self.bootstrap_cards().await;
        }
        self.write_tool(".tenaz/brief.md", "tampered\n", false)
            .await; // must be reverted
        self.finish("Done.");
    }

    async fn bootstrap_spec(&mut self) {
        self.write_tool(
            ".tenaz/spec.md",
            "# Spec\n\n- R1: one\n- R2: two\n- R3: three\n- R4: looks good (manual: visual)\n",
            false,
        )
        .await;
        self.write_tool(
            ".tenaz/map.md",
            "- verify: runs checks/*.sh\n- src/: one file per task\n",
            false,
        )
        .await;
    }

    async fn bootstrap_verify(&mut self) {
        let verify = self
            .script
            .verify_path
            .clone()
            .unwrap_or_else(|| "verify".to_owned());
        self.write_tool(&verify, VERIFY, true).await;
        self.write_tool("checks/T000.smoke.sh", "exit 0\n", false)
            .await;
        self.write_tool(".tenaz/notes/BOOTSTRAP.md", "Fake stack.\n", false)
            .await;
    }

    async fn bootstrap_cards(&mut self) {
        self.write_tool(
            ".tenaz/tasks/T001.md",
            &card("T001", &["R1"], &[], None),
            false,
        )
        .await;
        self.write_tool(
            ".tenaz/tasks/T002.md",
            &card("T002", &["R2"], &["T001"], None),
            false,
        )
        .await;
        self.write_tool(
            ".tenaz/tasks/T003.md",
            &card("T003", &["R3"], &["T001"], None),
            false,
        )
        .await;
        self.write_tool(
            ".tenaz/tasks/T004.md",
            &card("T004", &["R1"], &["T002"], None),
            false,
        )
        .await;
    }

    /// Reads until the context passes the stop and the session is aborted.
    async fn outgrow(&mut self) {
        let mut n = 1;
        while !self.aborted() && n < 20 {
            self.turn(
                "read",
                serde_json::json!({ "path": format!("src/part{n}.md") }),
                |_| {},
                "ok",
                Some(30_000 * n),
            )
            .await;
            n += 1;
        }
        self.finish("Done.");
    }

    async fn builder(&mut self, task: &str, attempt: u32) {
        if self.script.slow_task.as_deref() == Some(task) {
            tokio::time::sleep(self.script.slow_for).await; // gives the test time to pause the run
        }
        if self.script.crash_task.as_deref() == Some(task)
            && self.bump(&format!("crash-{task}")) == 1
        {
            return self.crash(false).await;
        }
        if self.script.crash_in_tool_task.as_deref() == Some(task)
            && self.bump(&format!("crash-in-tool-{task}")) == 1
        {
            return self.crash(true).await;
        }
        if self.script.truncate_task.as_deref() == Some(task)
            && self.bump(&format!("truncate-{task}")) == 1
        {
            return self.truncate().await;
        }
        if task == "T002" && attempt == 1 {
            while !self.aborted() {
                self.turn(
                    "bash",
                    serde_json::json!({ "command": "ls checks" }),
                    |_| {},
                    "ok",
                    None,
                )
                .await;
            }
            return self.finish("Done.");
        }
        if self.script.outgrow_task.as_deref() == Some(task) {
            return self.outgrow().await;
        }
        if self.script.hang_task.as_deref() == Some(task)
            && self.bump(&format!("hang-{task}")) <= self.script.hang_times
        {
            return self.hang().await;
        }
        if self.script.long_task.as_deref() == Some(task) && self.bump(&format!("long-{task}")) == 1
        {
            let mut n = 0;
            while !self.aborted() && n < 200 {
                n += 1;
                self.write_tool(&format!("src/{task}-part{n}.txt"), "part\n", false)
                    .await;
            }
            return self.finish("Done.");
        }
        if task == "T003" {
            self.write_tool("checks/T003.exists.sh", "exit 1\n", false)
                .await;
            return self.finish("Done.");
        }
        self.write_tool(&format!("src/{task}.txt"), &format!("{task}\n"), false)
            .await;
        if self.script.image_task.as_deref() == Some(task) {
            self.write_tool(&format!("assets/{task}.png"), "not really a PNG\n", false)
                .await;
        }
        if self.script.forget_checks_task.as_deref() == Some(task)
            && self.bump(&format!("forget-{task}")) == 1
        {
            self.write_tool(
                &format!(".tenaz/notes/{task}.md"),
                &format!("Built {task}.\n"),
                false,
            )
            .await;
            return self.finish("Done.");
        }
        if self.script.drop_check_task.as_deref() == Some(task) {
            if self.bump(&format!("drop-check-{task}")) == 1 {
                let _ = std::fs::remove_file(self.cwd.join("checks/T001.exists.sh"));
            } else {
                self.write_check("T001").await;
            }
        }
        self.write_check(task).await;
        self.write_tool(
            &format!(".tenaz/notes/{task}.md"),
            &format!("Built {task}.\n"),
            false,
        )
        .await;
        if task == "T004" {
            // Finished, then circles between two calls with the same results.
            while !self.aborted() {
                self.write_tool(
                    &format!(".tenaz/notes/{task}.md"),
                    &format!("Built {task}.\n"),
                    false,
                )
                .await;
                self.turn(
                    "bash",
                    serde_json::json!({ "command": format!("wc -l .tenaz/notes/{task}.md") }),
                    |_| {},
                    "ok",
                    None,
                )
                .await;
            }
            return self.finish("Done.");
        }
        if task == "T001" && self.bump("context-T001") == 1 {
            // Context grows past 1.3x the budget: stopped and continued.
            return self.outgrow().await;
        }
        if task == "T001" {
            self.write_tool(".tenaz/tasks/T001.md", "tampered\n", false)
                .await; // must be reverted
        }
        self.finish("Done.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_fields_are_read_from_the_session_header() {
        let prompt = "ROLE: BUILDER\n\n## Session\n\nRole: BUILDER\nTask: T007\nAttempt: 2 of 3\nReview file: .tenaz/reviews/T007.md\n";
        assert_eq!(field(prompt, "Role"), Some("BUILDER"));
        assert_eq!(field(prompt, "Task"), Some("T007"));
        assert_eq!(field(prompt, "Attempt"), Some("2 of 3"));
        assert_eq!(field(prompt, "Review file"), Some(".tenaz/reviews/T007.md"));
        assert_eq!(field("nothing", "Role"), None);
    }

    #[tokio::test]
    async fn a_session_without_a_role_answers_and_settles() {
        let harness = ScriptedHarness::default();
        let dir = tempfile::tempdir().unwrap();
        let mut session = harness
            .open(SessionSpec {
                label: "preflight".into(),
                cwd: dir.path().to_path_buf(),
                persist_dir: None,
                raw_log: None,
                thinking_log: None,
                stderr_log: None,
            })
            .await
            .unwrap();
        session.prompt("what is 17 * 23?").await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = session.next_event().await {
            let done = matches!(event, SessionEvent::Settled { .. });
            events.push(event);
            if done {
                break;
            }
        }
        assert!(
            matches!(&events[0], SessionEvent::MessageEnd { text, stop_reason: StopReason::Stop, .. } if text == "323")
        );
        assert_eq!(session.close(Duration::ZERO).await.code, Some(0));
    }
}
