//! What a continued session is told the sessions before it on the same
//! work did (ADR 0045): the facts a [`Trail`] kept, put into words within
//! a byte budget.

use crate::trail::{Check, RESULT_BYTES, Trail};
use crate::util::tail;

/// The most the digest takes, in bytes, whatever the trail holds.
pub(crate) const DIGEST_MAX_BYTES: usize = 2048;
/// At most this many bytes of the rendered list of files; the rest are counted.
const FILES_BYTES: usize = 400;
/// At most this many bytes of the last check's command.
const COMMAND_BYTES: usize = 200;
/// The end of the last words, at most this many bytes.
const WORDS_BYTES: usize = 400;
/// Marks where a part was cut.
const CUT: &str = "…";

/// The fixed text of the lines around the parts, generously: the bullets
/// and their words, quotes, code spans, a fence, `… and N more.`, the
/// joins.
const FIXED_BYTES: usize = 300;

// The parts at their budgets fit the bound the docs promise.
const _: () = assert!(
    HEADING.len() + FILES_BYTES + COMMAND_BYTES + RESULT_BYTES + WORDS_BYTES + FIXED_BYTES
        <= DIGEST_MAX_BYTES
);

const HEADING: &str = "### What the earlier sessions on this work did\n\nKept by Tenaz from their tool calls and replies. Files they changed through the shell are not listed; `git status` shows them. Do not reread whole files to find out what they did.";

/// The digest of `trail`, or `None` when it holds nothing. It stays within
/// [`DIGEST_MAX_BYTES`].
pub fn continuation_digest(trail: &Trail) -> Option<String> {
    let lines: Vec<String> = [
        file_line(trail),
        trail.check().map(check_line),
        words_line(trail),
    ]
    .into_iter()
    .flatten()
    .collect();
    if lines.is_empty() {
        return None;
    }
    let digest = format!("{HEADING}\n\n{}", lines.join("\n"));
    debug_assert!(digest.len() <= DIGEST_MAX_BYTES, "{} bytes", digest.len());
    Some(digest)
}

/// The files the sessions' write tools wrote, as `` `a`, `b` and N more ``
/// within [`FILES_BYTES`]; a first file longer than that keeps its end.
fn file_line(trail: &Trail) -> Option<String> {
    let files = trail.written();
    let first = files.first()?;
    let mut shown = vec![format!("`{}`", end_of(&code(first), FILES_BYTES - 5))];
    let mut bytes = shown[0].len();
    for file in &files[1..] {
        let file = format!("`{}`", code(file));
        bytes += file.len() + 2;
        if bytes > FILES_BYTES {
            break;
        }
        shown.push(file);
    }
    let more = match files.len() - shown.len() {
        0 => String::new(),
        more => format!(" and {more} more"),
    };
    Some(format!(
        "- Files their write tools wrote or edited: {}{more}.",
        shown.join(", ")
    ))
}

fn check_line(check: &Check) -> String {
    let (command, clipped) = start_of(code(&check.command), COMMAND_BYTES);
    let when = if check.stale {
        ", before the last edits"
    } else {
        ""
    };
    let how = if check.failed {
        "the tool reported an error"
    } else {
        "the tool reported no error"
    };
    let head = format!("- The last check{when}: `{command}`{clipped}; {how}");
    if check.result.is_empty() {
        format!("{head}, and no output.")
    } else {
        format!("{head}. Its output ended:\n\n```\n{}\n```", check.result)
    }
}

fn words_line(trail: &Trail) -> Option<String> {
    let words = trail.last_words();
    (!words.is_empty()).then(|| {
        format!(
            "- The last words: \"{}\"",
            end_of(&one_line(words), WORDS_BYTES)
        )
    })
}

/// The last `bytes` of `text` at most, with [`CUT`] in front when it was cut.
fn end_of(text: &str, bytes: usize) -> String {
    let kept = tail(text, bytes);
    let cut = if kept.len() < text.len() { CUT } else { "" };
    format!("{cut}{kept}")
}

/// The first `bytes` of `text` at most, and [`CUT`] when it was cut.
fn start_of(mut text: String, bytes: usize) -> (String, &'static str) {
    if text.len() <= bytes {
        return (text, "");
    }
    text.truncate(text.floor_char_boundary(bytes));
    (text, CUT)
}

/// `text` fit for a code span: on one line, without backticks.
fn code(text: &str) -> String {
    one_line(text).replace('`', "'")
}

/// `text` with runs of whitespace as one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::session::SessionEvent;
    use crate::trail::testing::{reply, run, start, trail, write};

    fn digest(events: &[SessionEvent]) -> String {
        continuation_digest(&trail(events)).expect("a digest")
    }

    fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
        text.lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line {prefix}: {text}"))
    }

    #[test]
    fn a_session_with_no_writes_checks_or_words_gets_none() {
        let [s, e] = run("2", "git status", "clean", false);
        let quiet = trail(&[start("1", "read_file", json!({"path": "src/a.js"})), s, e]);
        assert_eq!(continuation_digest(&quiet), None);
    }

    #[test]
    fn the_digest_names_the_files_the_last_check_and_the_last_words() {
        let mut events = Vec::new();
        events.extend(write("1", "src/reach.js"));
        events.extend(write("2", "checks/T011.mjs"));
        events.extend(run(
            "3",
            "CI=1 ./verify --task T011 | tail -6",
            "  ok   _setup\n  FAIL T011.no-softlock",
            false,
        ));
        events.push(reply("The push-out\nleft through the map edge.", None));
        assert_eq!(
            digest(&events),
            format!(
                "{HEADING}\n\n\
- Files their write tools wrote or edited: `src/reach.js`, `checks/T011.mjs`.\n\
- The last check: `CI=1 ./verify --task T011 | tail -6`; the tool reported no error. Its output ended:\n\n```\n  ok   _setup\n  FAIL T011.no-softlock\n```\n\
- The last words: \"The push-out left through the map edge.\""
            )
        );
    }

    #[test]
    fn a_stale_failed_check_without_output_says_so() {
        let mut events = run("1", "npm test", "", true).to_vec();
        events.extend(write("2", "src/a.js"));
        assert_eq!(
            line(&digest(&events), "- The last check"),
            "- The last check, before the last edits: `npm test`; the tool reported an error, and no output."
        );
    }

    /// Each listed file takes its length, two backticks, and `, ` before it
    /// after the first: 20 files of 16 bytes fit in 400.
    #[test]
    fn files_past_their_budget_are_counted() {
        let events: Vec<SessionEvent> = (0..60)
            .flat_map(|i| write(&i.to_string(), &format!("src/module_{i:02}.js")))
            .collect();
        let files = digest(&events);
        let files = line(&files, "- Files");
        assert!(
            files.ends_with("`src/module_19.js` and 40 more."),
            "{files}"
        );
    }

    #[test]
    fn a_first_file_over_the_budget_keeps_its_end() {
        let long = format!("{}/end.js", "d".repeat(600));
        let text = digest(&write("1", &long));
        let files = line(&text, "- Files");
        assert!(files.contains(": `…ddd"), "{files}");
        assert!(files.ends_with("ddd/end.js`."), "{files}");
        assert!(files.len() < FILES_BYTES + 60, "{} bytes", files.len());
    }

    #[test]
    fn paths_lose_backticks_and_line_breaks() {
        let mut events = write("1", "a`b.js").to_vec();
        events.extend(write("2", "c\nd.js"));
        let text = digest(&events);
        assert!(
            line(&text, "- Files").contains("`a'b.js`, `c d.js`."),
            "{text}"
        );
    }

    #[test]
    fn a_long_command_is_cut_outside_its_code_span_and_loses_backticks() {
        let command = format!("./verify `x` {}", "a".repeat(300));
        let text = digest(&run("1", &command, "ok", false));
        let check = line(&text, "- The last check");
        assert!(
            check.starts_with("- The last check: `./verify 'x' aaa"),
            "{check}"
        );
        assert!(
            check.ends_with("aaa`…; the tool reported no error. Its output ended:"),
            "{check}"
        );
    }

    #[test]
    fn long_last_words_keep_their_end_and_say_they_were_cut() {
        let words = format!("start {}end", "word ".repeat(200));
        let text = digest(&[reply(&words, None)]);
        let said = line(&text, "- The last words");
        assert!(said.starts_with("- The last words: \"…"), "{said}");
        assert!(said.ends_with(" end\""), "{said}");
        assert!(!said.contains("start"), "{said}");
    }

    #[test]
    fn a_digest_with_every_part_full_stays_within_its_bound() {
        let mut events: Vec<SessionEvent> = (0..400)
            .flat_map(|i| write(&i.to_string(), &format!("{i:03}")))
            .collect();
        let result = vec!["é".repeat(300); 20].join("\n");
        events.extend(run(
            "c",
            &format!("./verify {}", "`".repeat(400)),
            &result,
            true,
        ));
        events.extend(write("w", "000"));
        events.push(reply(&"é ".repeat(400), None));
        let text = digest(&events);
        assert!(text.len() <= DIGEST_MAX_BYTES, "{} bytes", text.len());
        assert!(
            text.len() > DIGEST_MAX_BYTES * 3 / 4,
            "{} bytes: not every part full",
            text.len()
        );
    }
}
