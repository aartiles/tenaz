//! What the scripted harness's model does in one session: the script per
//! role, written as tool calls and replies on the session's events.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::ScriptedHarness;
use super::abort::Abort;
use super::paced::PacedSender;
use crate::session::{SessionEvent, StopReason, Usage};

/// One tool call of the scripted model: the tool, its arguments, what it
/// returns, and the prompt tokens of the reply that made it (`None`: a few
/// more than the call before).
struct ToolCall<'a> {
    tool: &'a str,
    args: serde_json::Value,
    result: &'a str,
    input: Option<u64>,
}

impl<'a> ToolCall<'a> {
    /// A call that returns `ok`.
    fn new(tool: &'a str, args: serde_json::Value) -> Self {
        Self {
            tool,
            args,
            result: "ok",
            input: None,
        }
    }
}

pub(super) struct Player {
    pub(super) cwd: PathBuf,
    pub(super) stderr_log: Option<PathBuf>,
    pub(super) events: PacedSender,
    pub(super) abort: Arc<Abort>,
    pub(super) prompts: u32,
    pub(super) call: u32,
    pub(super) script: ScriptedHarness,
}

/// Whether the script's `script_field` names `task`.
fn names(script_field: Option<&str>, task: &str) -> bool {
    script_field == Some(task)
}

/// The value of the first `name:` line in a prompt.
pub(super) fn field<'a>(prompt: &'a str, name: &str) -> Option<&'a str> {
    prompt.lines().find_map(|l| {
        l.strip_prefix(name)
            .and_then(|r| r.strip_prefix(": "))
            .or_else(|| l.strip_prefix(name).and_then(|r| r.strip_prefix(':')))
            .map(str::trim)
    })
}

/// Where `outside_bootstrap` writes: outside the project and outside the
/// temporary directories the tests run in.
pub const OUTSIDE: &str = "/elsewhere";

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
        self.events.send(event);
    }

    /// Whether the turn was aborted, or the runner closed the session:
    /// either way nothing more is read.
    fn reads_no_more(&self) -> bool {
        self.abort.is_set() || self.events.is_closed()
    }

    /// Returns once the turn is aborted, or the runner closed the session.
    async fn until_no_more_read(&self) {
        tokio::select! {
            () = self.abort.wait() => {}
            () = self.events.closed() => {}
        }
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

    async fn turn(&mut self, call: ToolCall<'_>, effect: impl FnOnce(&Self)) {
        let ToolCall {
            tool,
            args,
            result,
            input,
        } = call;
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
        self.events.handled().await;
    }

    async fn write_tool(&mut self, path: &str, text: &str, executable: bool) {
        let (p, t) = (path.to_owned(), text.to_owned());
        self.turn(
            ToolCall::new("write", serde_json::json!({ "path": path })),
            move |me| me.write(&p, &t, executable),
        )
        .await;
    }

    fn finish(&self, text: &str) {
        let aborted = self.abort.is_set();
        self.emit(SessionEvent::MessageEnd {
            server: None,
            sent: None,
            usage: Usage {
                input: 1200,
                output: 10,
                cache_read: if self.prompts > 1 && !self.script.uncached {
                    900
                } else {
                    0
                },
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

    pub(super) async fn play(mut self, prompt: &str) {
        if let Some(task) = prompt
            .split("runs no passing check with ID prefix `")
            .nth(1)
            .and_then(|rest| rest.split('.').next())
        {
            if self.first(
                self.script.crash_on_checks_task.as_deref(),
                "crash-checks",
                task,
            ) {
                return self.crash(false).await;
            }
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
            "REVIEWER" => self.reviewer(prompt, &task).await,
            "SPLIT" => self.split(&task).await,
            "REPLAN" => self.replan().await,
            "FINAL-REVIEWER" => {
                self.write_tool(".tenaz/reviews/FINAL.md", "Verdict: ACCEPT\n", false)
                    .await;
                self.finish("Done.");
            }
            other => self.finish(&format!("unknown role {other}")),
        }
    }

    /// Writes the verdict to the review file the prompt names: accept,
    /// unless the script has this task's first review say otherwise.
    async fn reviewer(&mut self, prompt: &str, task: &str) {
        let file = field(prompt, "Review file")
            .unwrap_or(".tenaz/reviews/unknown.md")
            .to_owned();
        if let Some((at, stop)) = &self.script.stop_at_review
            && at == task
        {
            stop.cancel();
            return self.hang().await;
        }
        let first_review = self.bump(&format!("review-{task}")) == 1;
        let first_of =
            |script_field: &Option<String>| first_review && names(script_field.as_deref(), task);
        if first_of(&self.script.outgrow_review) {
            return self.outgrow().await;
        }
        if first_of(&self.script.no_verdict_review) {
            return self.finish("(no verdict written)"); // must be retried
        }
        let verdict = if first_of(&self.script.rejected_review) {
            "Verdict: CHANGES\n- [MAJOR] fake finding\n"
        } else if first_of(&self.script.minor_review) {
            "Verdict: CHANGES\n- [MINOR] a decision is not recorded\n"
        } else {
            "Verdict: ACCEPT\n"
        };
        self.write_tool(&file, verdict, false).await;
        self.finish("Done.");
    }

    /// Splits `task` into two cards, the second depending on the first.
    /// They cover R3 and depend on T001, as the scripted T003 does.
    async fn split(&mut self, task: &str) {
        self.write_tool(
            &format!(".tenaz/tasks/{task}a.md"),
            &card(&format!("{task}a"), &["R3"], &["T001"], Some(task)),
            false,
        )
        .await;
        self.write_tool(
            &format!(".tenaz/tasks/{task}b.md"),
            &card(
                &format!("{task}b"),
                &["R3"],
                &[&format!("{task}a")],
                Some(task),
            ),
            false,
        )
        .await;
        self.write_tool(".tenaz/notes/PLANNER.md", "Split.\n", false)
            .await;
        self.finish("Done.");
    }

    /// Changes nothing, but may edit a card out of its role.
    async fn replan(&mut self) {
        self.write_tool(".tenaz/notes/PLANNER.md", "Plan is fine.\n", false)
            .await;
        if let Some(task) = self.script.tampered_card.clone()
            && self.cwd.join(format!(".tenaz/tasks/{task}.md")).exists()
        {
            self.write_tool(
                &format!(".tenaz/tasks/{task}.md"),
                "tampered done card\n",
                false,
            )
            .await; // must be reverted
        }
        self.finish("Done.");
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
        self.events.handled().await;
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
        self.until_no_more_read().await;
        self.emit(SessionEvent::ToolCallEnd {
            id,
            name: "bash".to_owned(),
            result_text: "Command aborted".to_owned(),
            is_error: true,
        });
        self.finish("Stopped.");
    }

    /// One reply whose context crosses the stop and whose tool call writes
    /// the task's file; the write runs after the reply ends, as in Pi, and
    /// is cancelled if the turn was aborted by then. The next turn waits
    /// for the stop.
    async fn big_write(&mut self, task: &str) {
        self.call += 1;
        let id = format!("call_{}", self.call);
        let path = format!("src/{task}.txt");
        self.emit(SessionEvent::TurnStart);
        self.emit(SessionEvent::MessageEnd {
            server: None,
            sent: None,
            usage: Usage {
                input: 120_000,
                output: 12_000,
                ..Default::default()
            },
            stop_reason: StopReason::ToolUse,
            error_message: None,
            thinking_chars: 4,
            text: String::new(),
        });
        // An abort sent at the reply's end, or a close, arrives before the
        // write runs.
        self.events.handled().await;
        self.emit(SessionEvent::ToolCallStart {
            id: id.clone(),
            name: "write".to_owned(),
            args: serde_json::json!({ "path": path }),
        });
        let (result_text, is_error) = if self.reads_no_more() {
            ("Operation aborted", true)
        } else {
            self.write(&path, &format!("{task}\n"), false);
            ("ok", false)
        };
        self.emit(SessionEvent::ToolCallEnd {
            id,
            name: "write".to_owned(),
            result_text: result_text.to_owned(),
            is_error,
        });
        self.emit(SessionEvent::TurnEnd);
        self.emit(SessionEvent::TurnStart);
        self.until_no_more_read().await;
        self.finish("Stopped.");
    }

    /// Writes the task's file but not its check, then the context is
    /// compacted twice; the turn waits for the stop.
    async fn compact_twice(&mut self, task: &str) {
        self.write_tool(&format!("src/{task}.txt"), &format!("{task}\n"), false)
            .await;
        for _ in 0..2 {
            self.emit(SessionEvent::CompactionStart {
                reason: "threshold".to_owned(),
            });
            self.events.handled().await;
        }
        self.until_no_more_read().await;
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
        self.events.handled().await;
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
        if self.failed_bootstrap().await {
            return;
        }
        let steps = if self.script.stepwise_bootstrap {
            step..=step
        } else {
            1..=3
        };
        for step in steps {
            match step {
                1 => self.bootstrap_spec().await,
                2 => self.bootstrap_verify().await,
                3 => self.bootstrap_cards().await,
                _ => {}
            }
        }
        if self.script.tampered_brief {
            self.write_tool(".tenaz/brief.md", "tampered\n", false)
                .await; // must be reverted
        }
        self.finish("Done.");
    }

    /// The scripted bootstrap sessions that do not do the step: engine
    /// failures, a reply cut at the output limit, or one that ends without
    /// its files. True when one played.
    async fn failed_bootstrap(&mut self) -> bool {
        if self.script.engine_failures && self.bump("bootstrap") <= 2 {
            self.engine_failure(self.bump("engine-failure")).await;
        } else if self.script.truncate_bootstrap && self.bump("truncate-bootstrap") == 1 {
            self.truncate().await;
        } else {
            return self.unfinished_bootstrap().await;
        }
        true
    }

    /// A bootstrap session that ends without its files in the project:
    /// written outside it, or only in part. True when one played.
    async fn unfinished_bootstrap(&mut self) -> bool {
        if self.script.outside_bootstrap && self.bump("outside-bootstrap") == 1 {
            for file in ["spec.md", "map.md"] {
                let path = format!("{OUTSIDE}/.tenaz/{file}");
                self.turn(
                    ToolCall::new("write", serde_json::json!({ "path": path })),
                    |_| {},
                )
                .await;
            }
            self.finish("Done.");
        } else if self.script.partial_bootstrap && self.bump("partial-bootstrap") == 1 {
            self.write_tool(".tenaz/spec.md", "# Spec\n\n- R1: one\n", false)
                .await;
            self.write_tool("index.html", "<h1>draft</h1>\n", false)
                .await;
            self.finish("Out of time.");
        } else {
            return false;
        }
        true
    }

    /// A large read, then the engine fails: out of memory the first time,
    /// then a prompt over its current maximum. Rerun, not counted.
    async fn engine_failure(&mut self, n: u32) {
        let message = if n == 1 {
            "400: not enough GPU memory".to_owned()
        } else {
            "400: {\"message\":\"Prompt (98353 tokens) requires ~4406MB GPU memory but only ~3494MB is available. Maximum context at KV8 and prefill chunk 512: 61488 tokens.\"}".to_owned()
        };
        let big = "x".repeat(40_000); // about 10k tokens: steered
        let read = ToolCall {
            result: &big,
            ..ToolCall::new("read", serde_json::json!({ "path": "big.md" }))
        };
        self.turn(read, |_| {}).await;
        self.engine_error(&message);
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
        while !self.reads_no_more() && n < 20 {
            let read = ToolCall {
                input: Some(30_000 * n),
                ..ToolCall::new(
                    "read",
                    serde_json::json!({ "path": format!("src/part{n}.md") }),
                )
            };
            self.turn(read, |_| {}).await;
            n += 1;
        }
        self.finish("Done.");
    }

    /// Whether the run is stopped at this task's builder; stops it if so.
    fn stops_at(&self, task: &str) -> bool {
        match &self.script.stop_at {
            Some((at, stop)) if at == task => {
                stop.cancel();
                true
            }
            _ => false,
        }
    }

    /// Whether `task` is the one `script_field` names, the first time it
    /// is asked under `key`.
    fn first(&self, script_field: Option<&str>, key: &str, task: &str) -> bool {
        names(script_field, task) && self.bump(&format!("{key}-{task}")) == 1
    }

    async fn builder(&mut self, task: &str, attempt: u32) {
        if self.failed_session(task, attempt).await {
            return;
        }
        self.build(task).await;
    }

    /// The scripted builder sessions of `task` that do not do its work.
    /// True when one played.
    async fn failed_session(&mut self, task: &str, attempt: u32) -> bool {
        self.cut_short(task).await
            || self.stuck(task, attempt).await
            || self.stopped(task).await
            || self.failing(task).await
    }

    /// A session stopped by a test, crashed, or cut at the output limit.
    async fn cut_short(&mut self, task: &str) -> bool {
        if self.stops_at(task) {
            self.hang().await;
        } else if self.first(self.script.crash_task.as_deref(), "crash", task) {
            self.crash(false).await;
        } else if self.first(
            self.script.crash_in_tool_task.as_deref(),
            "crash-in-tool",
            task,
        ) {
            self.crash(true).await;
        } else if self.first(self.script.truncate_task.as_deref(), "truncate", task) {
            self.truncate().await;
        } else {
            return false;
        }
        true
    }

    /// A session that loops on one call, or outgrows its context.
    async fn stuck(&mut self, task: &str, attempt: u32) -> bool {
        if names(self.script.looping_task.as_deref(), task) && attempt == 1 {
            self.tool_loop().await;
        } else if names(self.script.outgrow_task.as_deref(), task) {
            self.outgrow().await;
        } else {
            return false;
        }
        true
    }

    /// A session the runner stops: a tool call that hangs, a write past the
    /// context stop, or too many turns.
    async fn stopped(&mut self, task: &str) -> bool {
        if names(self.script.hang_task.as_deref(), task)
            && self.bump(&format!("hang-{task}")) <= self.script.hang_times
        {
            self.hang().await;
        } else if self.first(self.script.big_write_task.as_deref(), "big-write", task) {
            self.big_write(task).await;
        } else if self.first(self.script.long_task.as_deref(), "long", task) {
            self.long_session(task).await;
        } else if self.first(self.script.compact_task.as_deref(), "compact", task) {
            self.compact_twice(task).await;
        } else {
            return false;
        }
        true
    }

    /// A session whose check fails, every time, and that leaves a note
    /// saying it cannot pass.
    async fn failing(&mut self, task: &str) -> bool {
        if !names(self.script.failing_task.as_deref(), task) {
            return false;
        }
        self.write_tool(&format!("checks/{task}.exists.sh"), "exit 1\n", false)
            .await;
        self.write_tool(
            &format!(".tenaz/notes/{task}.md"),
            &format!("{task} cannot pass as written.\n"),
            false,
        )
        .await;
        self.finish("Done.");
        true
    }

    /// The same tool call until the turn is aborted.
    async fn tool_loop(&mut self) {
        while !self.reads_no_more() {
            let ls = ToolCall::new("bash", serde_json::json!({ "command": "ls checks" }));
            self.turn(ls, |_| {}).await;
        }
        self.finish("Done.");
    }

    /// New files until the session is stopped at its turn budget.
    async fn long_session(&mut self, task: &str) {
        let mut n = 0;
        while !self.reads_no_more() && n < 200 {
            n += 1;
            self.write_tool(&format!("src/{task}-part{n}.txt"), "part\n", false)
                .await;
        }
        self.finish("Done.");
    }

    /// A builder session that does the work of `task`: its file, its check
    /// and its notes, with what the script adds on the way.
    async fn build(&mut self, task: &str) {
        self.write_tool(&format!("src/{task}.txt"), &format!("{task}\n"), false)
            .await;
        if names(self.script.image_task.as_deref(), task) {
            self.write_tool(&format!("assets/{task}.png"), "not really a PNG\n", false)
                .await;
        }
        if self.first(self.script.forget_checks_task.as_deref(), "forget", task) {
            self.write_notes(task).await;
            return self.finish("Done.");
        }
        if names(self.script.drop_check_task.as_deref(), task) {
            if self.bump(&format!("drop-check-{task}")) == 1 {
                let _ = std::fs::remove_file(self.cwd.join("checks/T001.exists.sh"));
            } else {
                self.write_check("T001").await;
            }
        }
        self.write_check(task).await;
        self.write_notes(task).await;
        if names(self.script.alternating_task.as_deref(), task) {
            return self.alternate(task).await;
        }
        if self.first(self.script.context_stop_task.as_deref(), "context", task) {
            // Context grows past 1.3x the budget: stopped and continued.
            return self.outgrow().await;
        }
        if names(self.script.tampered_card.as_deref(), task) {
            self.write_tool(&format!(".tenaz/tasks/{task}.md"), "tampered\n", false)
                .await; // must be reverted
        }
        self.finish("Done.");
    }

    async fn write_notes(&mut self, task: &str) {
        self.write_tool(
            &format!(".tenaz/notes/{task}.md"),
            &format!("Built {task}.\n"),
            false,
        )
        .await;
    }

    /// Finished, then circles between two calls with the same results.
    async fn alternate(&mut self, task: &str) {
        while !self.reads_no_more() {
            self.write_notes(task).await;
            let wc = ToolCall::new(
                "bash",
                serde_json::json!({ "command": format!("wc -l .tenaz/notes/{task}.md") }),
            );
            self.turn(wc, |_| {}).await;
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
}
