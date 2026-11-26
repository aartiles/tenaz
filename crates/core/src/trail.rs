//! What a session did, kept from its events for the session that continues
//! its work: the files its write tools wrote, its last check with the end
//! of its result, and its last words. Built from [`SessionEvent`]s, so it
//! is the same for every harness; `briefing::continuation_digest` puts it
//! into words (ADR 0045).

use crate::session::{SessionEvent, ToolEffect, command_arg, path_arg, tool_effect};
use crate::util::tail;

/// The end of a check's result that is kept: at most this many non-empty
/// lines and bytes.
const RESULT_LINES: usize = 8;
pub(crate) const RESULT_BYTES: usize = 500;

/// Programs that run checks when they are the command itself: the
/// project's `verify` (`./verify`, `.tenaz/verify`) and test runners.
const CHECK_PROGRAMS: [&str; 4] = ["verify", "pytest", "jest", "vitest"];
/// Programs whose `test` subcommand runs the tests.
const TEST_RUNNERS: [&str; 11] = [
    "npm", "pnpm", "yarn", "bun", "cargo", "go", "deno", "dotnet", "mix", "make", "swift",
];
/// Package managers whose `run test` runs the project's test script.
const SCRIPT_RUNNERS: [&str; 5] = ["npm", "pnpm", "yarn", "bun", "deno"];
/// Words before the program that only launch it. A launcher's options and
/// numbers (`timeout 600`, `nice -n 10`) are skipped with it.
const LAUNCHERS: [&str; 11] = [
    "npx", "bunx", "pnpx", "env", "time", "exec", "timeout", "nice", "uv", "poetry", "run",
];
/// Shell keywords before a command, skipped like an assignment.
const KEYWORDS: [&str; 9] = [
    "if", "elif", "while", "until", "do", "then", "else", "!", "{",
];
/// The line before a check's result whose start was cut.
const CUT_LINE: &str = "…\n";

/// The last check a session ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Check {
    pub command: String,
    /// The end of its result, cut to [`RESULT_LINES`] and [`RESULT_BYTES`].
    pub result: String,
    /// The harness reported the call as failed.
    pub failed: bool,
    /// The session wrote a file after the check ended.
    pub stale: bool,
}

/// A tool call that started and has not ended, and what it will mean.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    /// A write tool's call on this file.
    Write(String),
    /// A shell call that runs this check.
    Check(String),
}

/// What a session did, as its events showed it, or what a run of sessions
/// on the same work did ([`Trail::extend`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trail {
    written: Vec<String>,
    /// Calls that started and have not ended, by id. A call that never
    /// ends (the harness was stopped) counts for nothing.
    pending: Vec<(String, Pending)>,
    check: Option<Check>,
    last_words: String,
    /// The text of the current turn's reply, with any late text after it.
    /// `None` until the model replies: text an agent sends in a turn
    /// without a reply is the engine's error, not the model's words.
    reply: Option<String>,
}

impl Trail {
    /// Takes in one event of the session.
    pub fn observe(&mut self, event: &SessionEvent) {
        match event {
            start @ SessionEvent::ToolCallStart { id, .. } => {
                // An id seen again starts a new call: an earlier one with it
                // never ended.
                self.pending.retain(|(pending, _)| pending != id);
                if let Some(call) = pending_call(start) {
                    self.pending.push((id.clone(), call));
                }
            }
            SessionEvent::ToolCallEnd {
                id,
                result_text,
                is_error,
                ..
            } => {
                if let Some(at) = self.pending.iter().position(|(pending, _)| pending == id) {
                    self.call_ended(at, result_text, *is_error);
                }
            }
            SessionEvent::TurnStart => self.reply = None,
            // A call that has not ended when the harness settles never will.
            SessionEvent::Settled { .. } => self.pending.clear(),
            SessionEvent::MessageEnd {
                text,
                error_message,
                ..
            } => {
                self.reply = error_message.is_none().then(|| text.clone());
                self.keep_reply();
            }
            SessionEvent::LateText { text } => {
                if let Some(reply) = &mut self.reply {
                    reply.push_str(text);
                    self.keep_reply();
                }
            }
            _ => {}
        }
    }

    /// Files that write tools wrote without an error, each once, in the
    /// order first written. Edits made through the shell are not among them.
    /// Unlike the watchdog's write count, which counts a write tool's call
    /// when it starts, a call that failed wrote nothing here.
    pub fn written(&self) -> &[String] {
        &self.written
    }

    /// The last check that ended.
    pub fn check(&self) -> Option<&Check> {
        self.check.as_ref()
    }

    /// The text of the last reply that had any, trimmed; empty when none did.
    pub fn last_words(&self) -> &str {
        &self.last_words
    }

    /// Adds what a `later` session on the same work did: its files after
    /// these, its check if it ran one (else a write of it makes this one
    /// stale), and its last words if it said any.
    pub fn extend(&mut self, later: &Trail) {
        for path in &later.written {
            self.wrote(path.clone());
        }
        if later.check.is_some() {
            self.check.clone_from(&later.check);
        }
        if !later.last_words.is_empty() {
            self.last_words.clone_from(&later.last_words);
        }
    }

    fn keep_reply(&mut self) {
        if let Some(reply) = self.reply.as_deref().map(str::trim)
            && !reply.is_empty()
        {
            reply.clone_into(&mut self.last_words);
        }
    }

    /// The pending call at `at` ended with `result`.
    fn call_ended(&mut self, at: usize, result: &str, failed: bool) {
        match self.pending.remove(at).1 {
            Pending::Write(_) if failed => {}
            Pending::Write(path) => self.wrote(path),
            Pending::Check(command) => {
                self.check = Some(Check {
                    command,
                    result: result_tail(result),
                    failed,
                    stale: false,
                });
            }
        }
    }

    fn wrote(&mut self, path: String) {
        if !self.written.contains(&path) {
            self.written.push(path);
        }
        if let Some(check) = &mut self.check {
            check.stale = true;
        }
    }
}

/// What the call `start` starts will mean, if anything: a write tool's call
/// on a file, or a shell call that runs a check.
fn pending_call(start: &SessionEvent) -> Option<Pending> {
    let SessionEvent::ToolCallStart {
        name, args, effect, ..
    } = start
    else {
        return None;
    };
    if tool_effect(name, *effect) == Some(ToolEffect::Write) {
        return path_arg(args).map(|path| Pending::Write(path.to_owned()));
    }
    command_arg(args)
        .filter(|c| ShellCommand(c).runs_a_check())
        .map(|c| Pending::Check(c.trim().to_owned()))
}

/// The text of a shell command a tool call runs.
#[derive(Clone, Copy)]
struct ShellCommand<'a>(&'a str);

impl ShellCommand<'_> {
    /// Whether one of the command's parts runs a check (see [`is_check`]),
    /// after variable assignments, launchers, and the options and numbers a
    /// launcher takes (`timeout 600`, `nice -n 10`), and shell keywords
    /// (`if`, `do`, `!`). Only the shell's own words count: the body of a
    /// heredoc and the text of quoted strings (a notes file written with
    /// `cat <<EOF`, a commit message) are left out.
    fn runs_a_check(self) -> bool {
        let without_heredocs = self.without_heredocs();
        ShellCommand(&without_heredocs)
            .unquoted()
            .split(['\n', ';', '&', '|', '(', ')'])
            .any(|part| {
                let mut launched = false;
                let words: Vec<&str> = part
                    .split_whitespace()
                    .skip_while(|w| {
                        let launcher = LAUNCHERS.contains(w);
                        let option =
                            w.starts_with('-') || w.starts_with(|c: char| c.is_ascii_digit());
                        let skip = launcher
                            || KEYWORDS.contains(w)
                            || w.contains('=')
                            || (launched && option);
                        launched |= launcher;
                        skip
                    })
                    .collect();
                is_check(&words)
            })
    }

    /// The command without the bodies of its heredocs: a line with `<<` (not
    /// `<<<`, not inside quotes on that line) keeps what comes before and
    /// after the delimiter, and the lines up to the delimiter's own are dropped.
    fn without_heredocs(self) -> String {
        let mut kept = String::with_capacity(self.0.len());
        let mut lines = self.0.lines();
        while let Some(line) = lines.next() {
            let start = line.match_indices("<<").map(|(at, _)| at).find(|&at| {
                !line[at + 2..].starts_with('<')
                    && !line[..at].ends_with('<')
                    && line[..at].matches(['\'', '"']).count() % 2 == 0
            });
            let Some(at) = start else {
                kept.push_str(line);
                kept.push('\n');
                continue;
            };
            let mut after = line[at + 2..]
                .trim_start_matches('-')
                .trim_start()
                .splitn(2, char::is_whitespace);
            let delimiter = after.next().unwrap_or_default().trim_matches(['\'', '"']);
            kept.push_str(&line[..at]);
            kept.push(' ');
            kept.push_str(after.next().unwrap_or_default());
            kept.push('\n');
            if !delimiter.is_empty() {
                let _ = lines
                    .by_ref()
                    .find(|body| body.trim_start_matches('\t') == delimiter);
            }
        }
        kept
    }

    /// The command with each quoted string replaced by a space.
    fn unquoted(self) -> String {
        let mut words = String::with_capacity(self.0.len());
        let mut quote = None;
        for c in self.0.chars() {
            match quote {
                Some(open) if c == open => quote = None,
                Some(_) => {}
                None if c == '\'' || c == '"' => {
                    quote = Some(c);
                    words.push(' ');
                }
                None => words.push(c),
            }
        }
        words
    }
}

/// Whether the `words` of a command run a check: its program is one of
/// [`CHECK_PROGRAMS`] (by file name), or runs its tests
/// ([`runs_test_subcommand`]).
fn is_check(words: &[&str]) -> bool {
    let Some((program, rest)) = words.split_first() else {
        return false;
    };
    let name = program.rsplit('/').next().unwrap_or(program);
    CHECK_PROGRAMS.contains(&name) || runs_test_subcommand(name, rest)
}

/// Whether the program `name` with the words `rest` runs tests: a
/// [`TEST_RUNNERS`] program with `test`, a [`SCRIPT_RUNNERS`] one with
/// `run test`, `node --test`, or `python -m pytest`.
fn runs_test_subcommand(name: &str, rest: &[&str]) -> bool {
    match rest {
        ["test", ..] => TEST_RUNNERS.contains(&name),
        ["run", "test", ..] => SCRIPT_RUNNERS.contains(&name),
        ["--test", ..] => name == "node",
        ["-m", "pytest", ..] => name.starts_with("python"),
        _ => false,
    }
}

/// The last [`RESULT_LINES`] non-empty lines of `text`, within
/// [`RESULT_BYTES`], with code fences made harmless, and a [`CUT_LINE`]
/// first when anything before them was cut.
fn result_tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(RESULT_LINES);
    let kept = lines[start..].join("\n").replace("```", "'''");
    let end = tail(&kept, RESULT_BYTES - CUT_LINE.len());
    if start > 0 || end.len() < kept.len() {
        format!("{CUT_LINE}{end}")
    } else {
        end
    }
}

/// Events for tests elsewhere in the crate.
#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::testing::{end, reply, run, start, trail, write};
    use super::*;

    fn said(text: &str) -> SessionEvent {
        reply(text, None)
    }

    fn late(text: &str) -> SessionEvent {
        SessionEvent::LateText { text: text.into() }
    }

    #[test]
    fn write_tools_name_each_file_once_in_the_order_first_written() {
        let t = trail(&[
            start("1", "write_file", json!({"path": "src/reach.js"})),
            end("1", "ok", false),
            start("2", "edit_file", json!({"path": "checks/T011.mjs"})),
            end("2", "ok", false),
            start("3", "Edit", json!({"file_path": "src/reach.js"})),
            end("3", "ok", false),
            start("4", "write", json!({"content": "no path"})),
            end("4", "ok", false),
            start("5", "read_file", json!({"path": "src/map.js"})),
            end("5", "ok", false),
        ]);
        assert_eq!(t.written(), ["src/reach.js", "checks/T011.mjs"]);
    }

    /// An edit whose old text did not match changed nothing.
    #[test]
    fn a_write_that_failed_is_not_listed_and_leaves_the_check_current() {
        let [check_start, check_end] = run("1", "./verify", "ok", false);
        let t = trail(&[
            check_start,
            check_end,
            start("2", "edit_file", json!({"path": "src/a.js"})),
            end("2", "old_string not found", true),
        ]);
        assert!(t.written().is_empty(), "{t:?}");
        assert!(!t.check().expect("a check").stale);
    }

    #[test]
    fn a_write_that_never_ended_is_not_listed() {
        let t = trail(&[start("1", "write", json!({"path": "src/b.js"}))]);
        assert!(t.written().is_empty(), "{t:?}");
    }

    /// A call that never ended does not take the end of a later call that
    /// has its id.
    #[test]
    fn an_id_seen_again_starts_a_new_call() {
        let t = trail(&[
            start("1", "write", json!({"path": "src/old.js"})),
            start("1", "shell", json!({"command": "./verify"})),
            end("1", "ok", false),
        ]);
        assert!(t.written().is_empty(), "{t:?}");
        assert_eq!(t.check().map(|c| c.command.as_str()), Some("./verify"));
    }

    #[test]
    fn a_declared_write_effect_counts_whatever_the_tool_name() {
        let t = trail(&[
            SessionEvent::ToolCallStart {
                id: "1".into(),
                name: "replace".into(),
                args: json!({"path": "src/a.rs"}),
                effect: Some(ToolEffect::Write),
            },
            end("1", "ok", false),
        ]);
        assert_eq!(t.written(), ["src/a.rs"]);
    }

    #[test]
    fn the_last_check_that_ended_is_kept_with_the_end_of_its_result() {
        let [old_start, old_end] = run("1", "./verify --task T010", "old", false);
        let t = trail(&[
            old_start,
            old_end,
            start(
                "2",
                "shell",
                json!({"command": "CI=1 ./verify --task T011 | tail -6"}),
            ),
            start("3", "read_file", json!({"path": "a"})),
            end("3", "not the check", false),
            end("2", "verify: 1 check\n\n  ok   _setup\n  FAIL T011.x", true),
            start("4", "shell", json!({"command": "./verify --all"})),
        ]);
        assert_eq!(
            t.check(),
            Some(&Check {
                command: "CI=1 ./verify --task T011 | tail -6".into(),
                result: "verify: 1 check\n  ok   _setup\n  FAIL T011.x".into(),
                failed: true,
                stale: false,
            })
        );
    }

    /// A harness may run calls side by side: each check ends on its own.
    /// A session asked to wrap up writes its notes after its last check.
    #[test]
    fn notes_written_after_a_failed_check_leave_it_the_last_check() {
        let [check_start, check_end] = run("1", "./verify --all", "FAIL T001.x", true);
        let [notes_start, notes_end] = run(
            "2",
            "cat > .tenaz/notes/T001.md <<'EOF'\n- ./verify --all: T001.x fails\nEOF",
            "",
            false,
        );
        let t = trail(&[check_start, check_end, notes_start, notes_end]);
        let check = t.check().expect("a check");
        assert_eq!(check.command, "./verify --all");
        assert!(check.failed);
    }

    #[test]
    fn a_call_left_open_when_the_harness_settles_is_dropped() {
        let t = trail(&[
            start("1", "shell", json!({"command": "./verify"})),
            SessionEvent::Settled { aborted: true },
            end("1", "late", false),
        ]);
        assert_eq!(t.check(), None);
    }

    #[test]
    fn checks_that_overlap_each_end_on_their_own() {
        let t = trail(&[
            start("1", "shell", json!({"command": "cargo test -p a"})),
            start("2", "shell", json!({"command": "cargo test -p b"})),
            end("2", "b ok", false),
            end("1", "a ok", false),
        ]);
        assert_eq!(
            t.check().map(|c| c.command.as_str()),
            Some("cargo test -p a")
        );
    }

    #[test]
    fn a_check_is_stale_once_a_write_ends_after_it() {
        let [check_start, check_end] = run("1", "npm test", "", false);
        let [write_start, write_end] = write("2", "src/a.js");
        let t = trail(&[check_start, check_end, write_start, write_end]);
        assert!(t.check().expect("a check").stale);
    }

    #[test]
    fn a_write_that_ends_before_the_check_does_leaves_it_current() {
        let [check_start, check_end] = run("1", "npm test", "", false);
        let [write_start, write_end] = write("2", "src/a.js");
        let t = trail(&[check_start, write_start, write_end, check_end]);
        assert!(!t.check().expect("a check").stale);
    }

    #[test]
    fn the_result_keeps_its_last_non_empty_lines_and_no_fence() {
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let [s, e] = run(
            "1",
            "./verify",
            &format!("{}\n\n```", lines.join("\n")),
            false,
        );
        assert_eq!(
            trail(&[s, e]).check().expect("a check").result,
            "…\nline 43\nline 44\nline 45\nline 46\nline 47\nline 48\nline 49\n'''"
        );
    }

    #[test]
    fn the_result_keeps_its_last_bytes() {
        let lines: Vec<String> = (0..8).map(|i| format!("{i}{}", "x".repeat(199))).collect();
        let [s, e] = run("1", "./verify", &lines.join("\n"), false);
        let result = trail(&[s, e]).check().expect("a check").result.clone();
        assert_eq!(result.len(), RESULT_BYTES);
        assert!(result.ends_with(&lines[7]), "{result}");
    }

    #[test]
    fn only_commands_whose_program_runs_a_check_count_as_checks() {
        for command in [
            "./verify --all",
            ".tenaz/verify",
            "timeout 600 ./verify --task T001",
            "nice -n 10 cargo test",
            "uv run pytest -q",
            "poetry run pytest",
            "cat > t.py <<'EOF'\npytest -q\nEOF\npytest t.py",
            "cat <<-END > notes.md\n\tdone\n\tEND\n./verify",
            "git commit -m \"a << b\" && ./verify",
            "grep -q x <<< \"$out\" && ./verify",
            "if ./verify --all; then echo ok; fi",
            "for t in a b; do pytest $t; done",
            "! ./verify",
            "sleep 20; CI=1 NO_COLOR=1 ./verify --all > log 2>&1",
            "cargo test -p core",
            "npm run test",
            "python3 -m pytest -q",
            "(cd web && npx vitest run)",
            "node --test checks/",
        ] {
            assert!(ShellCommand(command).runs_a_check(), "{command}");
        }
        for command in [
            "sed -n 150,260p verify",
            "wc -l src/a.js verify",
            "grep -n verify src/a.js",
            "cd test && ls",
            "cargo run test",
            "bash -c './verify'",
            "cat > .tenaz/notes/T001.md <<'EOF'\n- pytest -q: 3 failing\n./verify --all\nEOF",
            "git commit -m \"Fix the gate\n\n./verify --all passes\"",
            "echo - pytest",
            "mkdir test",
            "echo test",
            "git status",
        ] {
            assert!(!ShellCommand(command).runs_a_check(), "{command}");
        }
    }

    #[test]
    fn the_last_words_are_the_last_reply_with_its_late_text() {
        let t = trail(&[
            said("Reading the map."),
            SessionEvent::TurnStart,
            said("Fixing the gate"),
            late(" next."),
            SessionEvent::TurnStart,
            said("  "),
        ]);
        assert_eq!(t.last_words(), "Fixing the gate next.");
    }

    /// fx sends an engine error as text in a turn the model never answered.
    #[test]
    fn late_text_in_a_turn_without_a_reply_is_not_kept() {
        let t = trail(&[
            said("Done with the gate."),
            SessionEvent::TurnStart,
            late("HTTP 400: context does not fit"),
        ]);
        assert_eq!(t.last_words(), "Done with the gate.");
    }

    #[test]
    fn a_failed_reply_is_not_kept() {
        let t = trail(&[
            said("Done with the gate."),
            SessionEvent::TurnStart,
            reply("partial", Some("HTTP 500")),
            late(" more"),
        ]);
        assert_eq!(t.last_words(), "Done with the gate.");
    }

    fn session(path: &str, check: Option<&str>, words: &str) -> Trail {
        let mut events = write("w", path).to_vec();
        if let Some(command) = check {
            events.extend(run("c", command, "ok", false));
        }
        events.push(said(words));
        trail(&events)
    }

    #[test]
    fn a_later_session_adds_its_files_check_and_words() {
        let mut t = session("src/a.js", Some("./verify --task T001"), "A.");
        t.extend(&session("src/b.js", Some("./verify --all"), "B."));
        assert_eq!(t.written(), ["src/a.js", "src/b.js"]);
        assert_eq!(
            t.check().map(|c| c.command.as_str()),
            Some("./verify --all")
        );
        assert_eq!(t.last_words(), "B.");
    }

    #[test]
    fn a_later_session_without_a_check_or_words_keeps_the_earlier_ones() {
        let mut t = session("src/a.js", Some("./verify"), "A.");
        t.extend(&trail(&write("1", "src/b.js")));
        let check = t.check().expect("a check");
        assert_eq!(check.command, "./verify");
        assert!(check.stale, "a later write makes it stale");
        assert_eq!(t.last_words(), "A.");
    }

    #[test]
    fn extending_with_an_empty_trail_changes_nothing() {
        let mut t = session("src/a.js", Some("./verify"), "A.");
        let before = t.clone();
        t.extend(&Trail::default());
        assert_eq!(t, before);
    }

    #[test]
    fn a_file_written_in_two_sessions_is_listed_once() {
        let mut t = session("src/a.js", None, "A.");
        t.extend(&session("src/a.js", None, "B."));
        assert_eq!(t.written(), ["src/a.js"]);
    }
}
