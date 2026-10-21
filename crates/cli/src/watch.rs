//! `tenaz watch`: follows a run from its files, as `tail -f` would, at a
//! chosen level of detail. It reads `.tenaz/private/orchestrator.log`, the
//! current session's event stream (`<label>.rpc.jsonl`, the same for every
//! harness) and, at `debug`, the harness's own logs (`<label>.*.log`). It
//! only reads, so it is safe while a run works.

use std::fmt::Write as _;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tenaz_core::project::Project;
use tenaz_core::session::SessionEvent;

/// How much of a run `watch` shows; each level adds to the one before.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Level {
    /// The run log only: sessions starting and ending, steers, tasks done.
    Summary,
    /// Also every tool call and model reply on one line, with the context
    /// each turn used and the decode speed.
    Actions,
    /// Also replies in full and the start of every tool result.
    Full,
    /// Also the harness's own log lines.
    Debug,
}

#[derive(Clone, Debug)]
pub struct WatchOptions {
    pub level: Level,
    /// Lines of the current session shown before following.
    pub history: usize,
    /// Print what is there and exit instead of following.
    pub once: bool,
}

const ORCHESTRATOR_LOG: &str = "orchestrator.log";
const RPC: &str = ".rpc.jsonl";
/// Characters of a line at `actions`.
const LINE: usize = 200;
/// Lines of a tool result at `full`.
const RESULT_LINES: usize = 20;

/// Watches the run in `project` until interrupted (or once).
pub fn watch(project: &Project, options: &WatchOptions) -> anyhow::Result<()> {
    let private = project.private_dir();
    let mut out = std::io::stdout().lock();
    let state = tenaz_core::orchestrator::status_json(project)["state"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    if state != "running" && !options.once {
        writeln!(
            out,
            "No run is working in {} now ({state}); waiting for one. Ctrl-C stops watching.",
            project.root().display()
        )?;
    }

    let mut orchestrator = Tail::new(private.join(ORCHESTRATOR_LOG));
    let backlog = orchestrator.read_lines()?;
    let shown = if options.level == Level::Summary {
        options.history
    } else {
        options.history.min(5)
    };
    for line in last(&backlog, shown) {
        writeln!(out, "{}", run_line(line))?;
    }

    let mut session = latest_session(&private).map(|label| SessionTail::open(&private, &label));
    if options.level > Level::Summary
        && let Some(current) = session.as_mut()
    {
        writeln!(out, "{}", header(&current.label))?;
        let lines = current.read(options.level)?;
        for line in last(&lines, options.history) {
            writeln!(out, "{line}")?;
        }
    }
    out.flush()?;
    if options.once {
        return Ok(());
    }

    loop {
        std::thread::sleep(Duration::from_millis(500));
        for line in orchestrator.read_lines()? {
            writeln!(out, "{}", run_line(&line))?;
        }
        if options.level > Level::Summary {
            let newest = latest_session(&private);
            if newest.is_some() && newest.as_deref() != session.as_ref().map(|s| s.label.as_str()) {
                // Whatever the old session wrote last, then the new one from its start.
                if let Some(old) = session.as_mut() {
                    for line in old.read(options.level)? {
                        writeln!(out, "{line}")?;
                    }
                }
                let label = newest.unwrap_or_default();
                writeln!(out, "{}", header(&label))?;
                session = Some(SessionTail::open(&private, &label));
            }
            if let Some(current) = session.as_mut() {
                for line in current.read(options.level)? {
                    writeln!(out, "{line}")?;
                }
            }
        }
        out.flush()?;
    }
}

fn last(lines: &[String], n: usize) -> &[String] {
    &lines[lines.len().saturating_sub(n)..]
}

fn header(label: &str) -> String {
    format!("── {label} ──")
}

/// `[2026-10-21T06:57:52.26Z] Session …` → `06:57:52 Session …`.
fn run_line(line: &str) -> String {
    if let Some(rest) = line.strip_prefix('[')
        && let Some((stamp, text)) = rest.split_once("] ")
        && let Some((_, time)) = stamp.split_once('T')
    {
        let time = time.get(..8).unwrap_or(time);
        return format!("{time} {text}");
    }
    line.to_owned()
}

/// The label of the session with the highest sequence number.
fn latest_session(private: &Path) -> Option<String> {
    std::fs::read_dir(private)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(RPC).map(str::to_owned))
        .filter_map(|label| {
            let seq: u64 = label.split('-').next()?.parse().ok()?;
            Some((seq, label))
        })
        .max()
        .map(|(_, label)| label)
}

/// A file read from where the last read stopped, whole lines only.
struct Tail {
    path: PathBuf,
    offset: u64,
    partial: String,
}

impl Tail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: String::new(),
        }
    }

    fn read_lines(&mut self) -> std::io::Result<Vec<String>> {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Ok(Vec::new());
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            // Rewritten from the start (a harness that truncates its log).
            self.offset = 0;
            self.partial.clear();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&bytes));
        let Some(end) = self.partial.rfind('\n') else {
            return Ok(Vec::new());
        };
        let complete: String = self.partial.drain(..=end).collect();
        Ok(complete.lines().map(str::to_owned).collect())
    }
}

/// One session's event stream and, for `debug`, its harness logs.
struct SessionTail {
    label: String,
    events: Tail,
    logs: Vec<Tail>,
    private: PathBuf,
}

impl SessionTail {
    fn open(private: &Path, label: &str) -> Self {
        Self {
            label: label.to_owned(),
            events: Tail::new(private.join(format!("{label}{RPC}"))),
            logs: Vec::new(),
            private: private.to_owned(),
        }
    }

    /// New lines, rendered at `level`.
    fn read(&mut self, level: Level) -> std::io::Result<Vec<String>> {
        let mut lines = Vec::new();
        for raw in self.events.read_lines()? {
            match serde_json::from_str::<SessionEvent>(&raw) {
                Ok(event) => lines.extend(render(&event, level)),
                Err(_) if level == Level::Debug => lines.push(format!("  ? {raw}")),
                Err(_) => {}
            }
        }
        if level == Level::Debug {
            self.find_logs();
            for log in &mut self.logs {
                let name = log
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix(&format!("{}.", self.label)))
                    .and_then(|n| n.strip_suffix(".log"))
                    .unwrap_or("log")
                    .to_owned();
                for line in log.read_lines()? {
                    lines.push(format!("    [{name}] {line}"));
                }
            }
        }
        Ok(lines)
    }

    /// `<label>.*.log` files that appeared since the last look.
    fn find_logs(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.private) else {
            return;
        };
        let prefix = format!("{}.", self.label);
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let is_log = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with(&prefix)
                    && Path::new(n)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("log"))
            });
            if is_log && !self.logs.iter().any(|l| l.path == path) {
                self.logs.push(Tail::new(path));
            }
        }
    }
}

/// The lines one session event shows at `level` (none at `summary`).
pub fn render(event: &SessionEvent, level: Level) -> Vec<String> {
    let full = level >= Level::Full;
    let fit = |text: &str| {
        if full {
            text.to_owned()
        } else {
            clip(&first_line(text), LINE)
        }
    };
    match event {
        _ if level == Level::Summary => Vec::new(),
        SessionEvent::ToolCallStart { name, args, .. } => {
            vec![format!("  → {name}: {}", fit(&describe_args(args)))]
        }
        SessionEvent::ToolCallEnd {
            result_text,
            is_error,
            ..
        } => {
            if full {
                let mut lines = result_lines(result_text);
                if *is_error {
                    lines.insert(0, "    ✗ error".to_owned());
                }
                lines
            } else if *is_error {
                vec![format!("    ✗ {}", fit(&error_summary(result_text)))]
            } else {
                Vec::new()
            }
        }
        SessionEvent::MessageEnd {
            usage,
            error_message,
            thinking_chars,
            text,
            server,
            ..
        } => {
            let mut lines = Vec::new();
            if !text.trim().is_empty() {
                if full {
                    lines.extend(text.trim().lines().map(|l| format!("  » {l}")));
                } else {
                    lines.push(format!("  » {}", fit(text.trim())));
                }
            }
            let mut stats = format!(
                "    ctx {} · out {} · think {} ch",
                thousands(usage.context()),
                usage.output,
                thinking_chars
            );
            if let Some(decode) = server.and_then(|s| s.decode_ms)
                && decode > 0.0
            {
                #[allow(clippy::cast_precision_loss)]
                let speed = usage.output as f64 / decode * 1000.0;
                let _ = write!(stats, " · {speed:.0} tok/s");
            }
            lines.push(stats);
            if let Some(error) = error_message {
                lines.push(format!("    ✗ engine: {}", fit(error)));
            }
            lines
        }
        SessionEvent::CompactionStart { reason } => vec![format!("  ⇢ compaction: {reason}")],
        SessionEvent::PromptRejected { reason } => {
            vec![format!("  ✗ prompt rejected: {}", fit(reason))]
        }
        SessionEvent::Settled { aborted } => vec![if *aborted {
            "  ■ settled (aborted)".to_owned()
        } else {
            "  ■ settled".to_owned()
        }],
        SessionEvent::Exited { code } => vec![format!(
            "  ■ harness exited ({})",
            code.map_or_else(|| "signal".to_owned(), |c| format!("code {c}"))
        )],
        SessionEvent::PermissionRequest { tool, .. } if level == Level::Debug => {
            vec![format!("    permission: {tool}")]
        }
        SessionEvent::TurnStart if level == Level::Debug => vec!["    turn".to_owned()],
        _ => Vec::new(),
    }
}

/// A tool result at `full`: its first lines or, for a JSON envelope (fx's
/// shell results, which the stream keeps only the start of), its exit code.
fn result_lines(text: &str) -> Vec<String> {
    if text.trim_start().starts_with('{') {
        return exit_code(text).map_or_else(Vec::new, |c| vec![format!("    │ exit {c}")]);
    }
    let mut lines: Vec<String> = text
        .lines()
        .take(RESULT_LINES)
        .map(|l| format!("    │ {}", clip(l, LINE)))
        .collect();
    let more = text.lines().count().saturating_sub(RESULT_LINES);
    if more > 0 {
        lines.push(format!("    │ … {more} more lines"));
    }
    lines
}

/// The exit code of a JSON envelope (fx's shell results), if it says.
fn exit_code(text: &str) -> Option<String> {
    let (_, rest) = text.split_once("\"exit_code\":")?;
    let code: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    (!code.is_empty()).then_some(code)
}

/// A failed tool result on one line: the exit code of a JSON envelope,
/// else the text.
fn error_summary(text: &str) -> String {
    if text.trim_start().starts_with('{') {
        return exit_code(text).map_or_else(|| "failed".to_owned(), |c| format!("exit {c}"));
    }
    text.to_owned()
}

/// The part of a tool call's arguments worth reading: the command, path or
/// pattern, else the arguments as JSON. A leading `cd <dir> &&` is dropped.
fn describe_args(args: &serde_json::Value) -> String {
    for key in [
        "command",
        "cmd",
        "path",
        "file_path",
        "pattern",
        "query",
        "url",
    ] {
        if let Some(value) = args.get(key).and_then(serde_json::Value::as_str) {
            let value = value.trim();
            if let Some(rest) = value.strip_prefix("cd ")
                && let Some((_, after)) = rest.split_once("&& ")
            {
                return after.trim().to_owned();
            }
            return value.to_owned();
        }
    }
    // A call on a running process (fx's `interact`): what it does to which.
    if let Some(action) = args.get("action").and_then(serde_json::Value::as_str) {
        let target = ["session_id", "id", "pid"]
            .iter()
            .find_map(|k| args.get(*k).and_then(serde_json::Value::as_str));
        return target.map_or_else(|| action.to_owned(), |t| format!("{action} {t}"));
    }
    args.to_string()
}

fn first_line(text: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default().to_owned();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(max).collect();
    clipped.push('…');
    clipped
}

/// `34852` → `34.9k`.
fn thousands(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    #[allow(clippy::cast_precision_loss)]
    let k = n as f64 / 1000.0;
    format!("{k:.1}k")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tenaz_core::session::{ServerTimings, StopReason, Usage};

    fn message(text: &str) -> SessionEvent {
        SessionEvent::MessageEnd {
            usage: Usage {
                input: 852,
                output: 200,
                cache_read: 34_000,
                cache_write: 0,
                reasoning: 0,
            },
            stop_reason: StopReason::ToolUse,
            error_message: None,
            thinking_chars: 125,
            text: text.to_owned(),
            server: Some(ServerTimings {
                decode_ms: Some(5000.0),
                ..ServerTimings::default()
            }),
        }
    }

    #[test]
    fn actions_show_one_line_per_call_and_reply() {
        let call = SessionEvent::ToolCallStart {
            id: "1".into(),
            name: "shell".into(),
            args: serde_json::json!({"command": "cd /some/project && grep -n x src/main.js"}),
        };
        assert_eq!(
            render(&call, Level::Actions),
            ["  → shell: grep -n x src/main.js"]
        );
        assert_eq!(
            render(&message("First line\nsecond line"), Level::Actions),
            [
                "  » First line …",
                "    ctx 35.1k · out 200 · think 125 ch · 40 tok/s"
            ]
        );
        let ok = SessionEvent::ToolCallEnd {
            id: "1".into(),
            name: "shell".into(),
            result_text: "fine".into(),
            is_error: false,
        };
        assert!(render(&ok, Level::Actions).is_empty());
        let failed = SessionEvent::ToolCallEnd {
            id: "1".into(),
            name: "shell".into(),
            result_text: "{\"session_id\":null,\"state\":\"completed\",\"full_output_hand".into(),
            is_error: true,
        };
        assert_eq!(render(&failed, Level::Actions), ["    ✗ failed"]);
        let interact = SessionEvent::ToolCallStart {
            id: "2".into(),
            name: "shell".into(),
            args: serde_json::json!({"action": "interact", "session_id": "shell-9", "yield_time_ms": 60000}),
        };
        assert_eq!(
            render(&interact, Level::Actions),
            ["  → shell: interact shell-9"]
        );
        assert!(render(&call, Level::Summary).is_empty());
    }

    #[test]
    fn full_shows_whole_replies_and_the_start_of_results() {
        assert_eq!(
            render(&message("First line\nsecond line"), Level::Full)[..2],
            ["  » First line", "  » second line"]
        );
        let long: Vec<String> = (0..25).map(|i| format!("line {i}")).collect();
        let end = SessionEvent::ToolCallEnd {
            id: "1".into(),
            name: "shell".into(),
            result_text: long.join("\n"),
            is_error: true,
        };
        let lines = render(&end, Level::Full);
        assert_eq!(lines[0], "    ✗ error");
        assert_eq!(lines[1], "    │ line 0");
        assert_eq!(lines.last().unwrap(), "    │ … 5 more lines");
        assert_eq!(
            result_lines("{\"state\":\"completed\",\"exit_code\":2,\"output\":\"cut sho"),
            ["    │ exit 2"]
        );
        assert!(result_lines("{\"state\":\"running\",\"outp").is_empty());
    }

    #[test]
    fn run_log_lines_keep_the_time_of_day() {
        assert_eq!(
            run_line("[2026-10-21T06:57:52.26Z] Session 051-builder-T026a started"),
            "06:57:52 Session 051-builder-T026a started"
        );
        assert_eq!(run_line("plain"), "plain");
    }

    #[test]
    fn the_latest_session_is_the_highest_number_and_reads_resume() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "009-builder-T001.rpc.jsonl",
            "1000-reviewer-T002.rpc.jsonl",
            "preflight.fx-acp.log",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        assert_eq!(
            latest_session(dir.path()).as_deref(),
            Some("1000-reviewer-T002")
        );

        let path = dir.path().join("1000-reviewer-T002.rpc.jsonl");
        let mut tail = Tail::new(path.clone());
        std::fs::write(&path, "{\"kind\":\"turn_start\"}\n{\"kind\":\"settl").unwrap();
        assert_eq!(tail.read_lines().unwrap(), ["{\"kind\":\"turn_start\"}"]);
        std::fs::write(
            &path,
            "{\"kind\":\"turn_start\"}\n{\"kind\":\"settled\",\"aborted\":false}\n",
        )
        .unwrap();
        assert_eq!(
            tail.read_lines().unwrap(),
            ["{\"kind\":\"settled\",\"aborted\":false}"]
        );
    }
}
