//! Whether a tool call reads something new, for the idle rule (ADR 0029).
//!
//! A read tool's call is new when its arguments are. Any other call that
//! does not write (a shell command, Pi's `grep` or `ls`) is new when it
//! names a path not read or named before: a builder that reads with
//! `sed -n`, `grep` or `cat`, or writes and runs a probe script, names the
//! files it works on.
//!
//! The paths in a shell command are a guess from its text, not a shell
//! parse. Quoted text, comments and heredoc bodies are skipped: they hold
//! patterns and code, not the files the command reads. A word counts as a
//! path when it has a `/` or ends in an extension (`world.js`, `checks/`,
//! `src/*.js`).

use std::collections::{HashSet, VecDeque};

use crate::session::{command_arg, path_arg};
use crate::write_scope::WriteScope;

/// What a session has read so far. A `scope`, where the run has one,
/// resolves paths against the project root, so that a path counts the same
/// absolute or relative.
#[derive(Debug, Default)]
pub struct Novelty {
    /// Arguments of every read tool call.
    reads: HashSet<String>,
    /// Every path read or named, relative to the root where it is inside.
    paths: HashSet<String>,
}

impl Novelty {
    /// A read tool's call: new when its arguments are. Its path is recorded
    /// for the shell.
    pub fn read(&mut self, args: &serde_json::Value, scope: Option<&WriteScope>) -> bool {
        if let Some(path) = path_arg(args) {
            self.see(path, scope);
        }
        self.reads.insert(args.to_string())
    }

    /// A call that neither reads nor writes by its effect: new when its
    /// command or its path argument names a path not seen before.
    pub fn other(&mut self, args: &serde_json::Value, scope: Option<&WriteScope>) -> bool {
        let command = command_arg(args).map(named_paths).unwrap_or_default();
        let mut new = false;
        for path in command.iter().map(String::as_str).chain(path_arg(args)) {
            new |= self.see(path, scope);
        }
        new
    }

    /// Records `path`; whether it was not seen before.
    fn see(&mut self, path: &str, scope: Option<&WriteScope>) -> bool {
        let key = scope.and_then(|scope| scope.relative(path)).map_or_else(
            || path.trim_start_matches("./").to_owned(),
            |relative| relative.display().to_string(),
        );
        !key.is_empty() && self.paths.insert(key)
    }
}

/// The paths `command` names, in order, each once.
fn named_paths(command: &str) -> Vec<String> {
    let words = Scan::new(command).run();
    let mut paths: Vec<String> = Vec::new();
    for path in words.split(is_separator).filter(|w| is_path(w)) {
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_owned());
        }
    }
    paths
}

/// Characters between the words of a command: blanks, operators,
/// redirections, substitutions and `=` (`--file=src/a.js`).
fn is_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')' | '`' | '=')
}

/// Where a `#` starts a comment: after a blank or an operator, not after
/// `=` (`a=#x` is one word).
fn is_word_break(c: char) -> bool {
    is_separator(c) && c != '='
}

/// Whether a word looks like a path.
fn is_path(word: &str) -> bool {
    is_word(word) && (word.contains('/') || has_extension(word))
}

/// A word made only of the characters of a file name or a glob, and not an
/// option or a device.
fn is_word(word: &str) -> bool {
    let allowed = |c: char| c.is_alphanumeric() || "_.,@~/*{}+-".contains(c);
    !word.is_empty()
        && !word.starts_with('-')
        && !word.starts_with("/dev/")
        && word.chars().all(allowed)
}

/// Whether the word ends in `.` and a name that starts with a letter, as a
/// file name does (`a.js`) and a number (`2.6`) or a git range
/// (`main..HEAD`) does not.
fn has_extension(word: &str) -> bool {
    word.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && !stem.ends_with('.')
            && ext.starts_with(|c: char| c.is_ascii_alphabetic())
            && ext.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// A heredoc opened on the current line: its end word, and whether the
/// end word may be indented with tabs (`<<-`).
struct Heredoc {
    end: String,
    tabs: bool,
}

/// One pass over a command that keeps its words outside quotes, escapes,
/// comments and heredoc bodies, with a blank in place of what it drops.
struct Scan {
    chars: Vec<char>,
    at: usize,
    out: String,
    heredocs: VecDeque<Heredoc>,
    /// Depth of `(( ))` on the current line, where `<<` is a shift.
    arithmetic: u32,
}

impl Scan {
    fn new(command: &str) -> Self {
        Self {
            chars: command.chars().collect(),
            at: 0,
            out: String::with_capacity(command.len()),
            heredocs: VecDeque::new(),
            arithmetic: 0,
        }
    }

    fn run(mut self) -> String {
        while let Some(c) = self.bump() {
            let dropped = self.dropped(c);
            // Arithmetic (`$((10/2))`) holds numbers, not paths.
            let kept = if dropped || self.arithmetic > 0 {
                ' '
            } else {
                c
            };
            self.out.push(kept);
        }
        self.out
    }

    /// Whether `c` starts what is not a word, which is then skipped.
    fn dropped(&mut self, c: char) -> bool {
        match c {
            '\'' | '"' => self.skip_quoted(c),
            '\\' => self.bump().is_some(),
            '\n' => self.end_line(),
            '#' => self.skip_comment(),
            '(' => self.open_arithmetic(),
            ')' => self.close_arithmetic(),
            '<' => self.redirect(),
            _ => false,
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.at).copied();
        self.at += usize::from(c.is_some());
        c
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn starts(&self, text: &str) -> bool {
        text.chars()
            .enumerate()
            .all(|(n, c)| self.chars.get(self.at + n) == Some(&c))
    }

    /// Whether the character just taken starts a word.
    fn at_word_start(&self) -> bool {
        self.at < 2 || is_word_break(self.chars[self.at - 2])
    }

    /// Whether the character just taken is preceded by `$`, as in `$'...'`.
    fn after_dollar(&self) -> bool {
        self.at >= 2 && self.chars[self.at - 2] == '$'
    }

    /// Past the closing quote; a `\` escapes the next character in `"..."`
    /// and in `$'...'`.
    fn skip_quoted(&mut self, quote: char) -> bool {
        let escapes = quote == '"' || self.after_dollar();
        while let Some(c) = self.bump() {
            if c == quote {
                break;
            }
            if escapes && c == '\\' {
                self.bump();
            }
        }
        true
    }

    /// A `#` that starts a word: up to the end of the line.
    fn skip_comment(&mut self) -> bool {
        if !self.at_word_start() {
            return false;
        }
        while self.peek().is_some_and(|c| c != '\n') {
            self.bump();
        }
        true
    }

    /// A `((`: arithmetic opens.
    fn open_arithmetic(&mut self) -> bool {
        let opens = self.starts("(");
        if opens {
            self.bump();
            self.arithmetic += 1;
        }
        opens
    }

    /// A `))` in arithmetic: it closes.
    fn close_arithmetic(&mut self) -> bool {
        let closes = self.arithmetic > 0 && self.starts(")");
        if closes {
            self.bump();
            self.arithmetic -= 1;
        }
        closes
    }

    /// A newline: arithmetic left open ends with its line, and the bodies
    /// of the heredocs the line opened follow it.
    fn end_line(&mut self) -> bool {
        self.arithmetic = 0;
        while let Some(heredoc) = self.heredocs.pop_front() {
            self.skip_body(&heredoc);
        }
        true
    }

    /// Past a heredoc's body, up to its end word, or to the end of the
    /// command when it is never closed.
    fn skip_body(&mut self, heredoc: &Heredoc) {
        while let Some(line) = self.line() {
            let line = if heredoc.tabs {
                line.trim_start_matches('\t')
            } else {
                &line
            };
            if line == heredoc.end {
                return;
            }
        }
    }

    /// The rest of the current line, past its newline.
    fn line(&mut self) -> Option<String> {
        self.peek()?;
        let mut line = String::new();
        while let Some(c) = self.bump().filter(|c| *c != '\n') {
            line.push(c);
        }
        Some(line)
    }

    /// After `<`, with `<` next: a here-string (`<<<`), or a heredoc whose
    /// end word is queued for the end of the line.
    fn redirect(&mut self) -> bool {
        if self.arithmetic > 0 || !self.starts("<") {
            return false;
        }
        self.bump();
        if self.starts("<") {
            self.bump();
            return true;
        }
        let tabs = self.starts("-");
        if tabs {
            self.bump();
        }
        while self.peek().is_some_and(|c| c == ' ' || c == '\t') {
            self.bump();
        }
        let end = self.end_word();
        if !end.is_empty() {
            self.heredocs.push_back(Heredoc { end, tabs });
        }
        true
    }

    /// A heredoc's end word (`EOF`, `'EOF'`, `"EOF"`, `\EOF`) without its
    /// quotes.
    fn end_word(&mut self) -> String {
        let mut end = String::new();
        while let Some(c) = self
            .peek()
            .filter(|c| !c.is_whitespace() && !is_separator(*c))
        {
            self.bump();
            if !matches!(c, '\'' | '"' | '\\') {
                end.push(c);
            }
        }
        end
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn reads_name_their_files() {
        let cases: [(&str, &[&str]); 7] = [
            (
                "sed -n 90,130p checks/T014.visual-pass.mjs; echo ----; sed -n 1,60p checks/T007.sprite-occlusion.mjs",
                &[
                    "checks/T014.visual-pass.mjs",
                    "checks/T007.sprite-occlusion.mjs",
                ],
            ),
            (
                "grep -n \"barrel\\|4.8\" src/world.js | head -30",
                &["src/world.js"],
            ),
            ("cat ./src/main.js 2>/dev/null", &["./src/main.js"]),
            ("wc -l src/*.js", &["src/*.js"]),
            ("node --input=data/a.json x.mjs", &["data/a.json", "x.mjs"]),
            ("ls {src,lib}/*.js", &["{src,lib}/*.js"]),
            ("echo it\\'s src/a.js", &["src/a.js"]),
        ];
        for (command, expected) in cases {
            assert_eq!(named_paths(command), expected, "{command}");
        }
    }

    #[test]
    fn commands_without_files_name_none() {
        for command in [
            "git status --short; echo ---; git diff --stat",
            "head -c 400 -n 2",
            "echo 2.6 7.2",
            "git diff main...HEAD; git log HEAD~1..main",
            "ls src",
            "",
        ] {
            assert!(named_paths(command).is_empty(), "{command}");
        }
    }

    #[test]
    fn quoted_text_and_comments_are_not_paths() {
        let cases: [(&str, &[&str]); 5] = [
            (
                "awk '/function makeBarrel/,/^}/' src/textures.js",
                &["src/textures.js"],
            ),
            (r#"grep -n "a\"b/c.js" d.md"#, &["d.md"]),
            (
                "node -e \"\nconst w = JSON.parse(fs.readFileSync('a.json'));\nconsole.log(w.tiles.length)\n\"",
                &[],
            ),
            ("python3 -c 'import os\nprint(os.path.join(1))'", &[]),
            ("cat a.js # don't read b.md\ncat c.js", &["a.js", "c.js"]),
        ];
        for (command, expected) in cases {
            assert_eq!(named_paths(command), expected, "{command}");
        }
    }

    #[test]
    fn a_heredoc_names_its_target_not_its_body() {
        let command = "mkdir -p .tenaz/private && cat > .tenaz/private/probe.mjs <<'EOF'\n\
import { getGamePage } from '../checks/_harness.mjs';\n\
const r = s.drawStart.length;\n\
EOF\n\
node .tenaz/private/probe.mjs | tail -60";
        assert_eq!(
            named_paths(command),
            [".tenaz/private", ".tenaz/private/probe.mjs"]
        );
    }

    #[test]
    fn heredoc_forms() {
        let cases: [(&str, &[&str]); 7] = [
            (
                "cat <<-END > a.js\n\tx.md\n\tEND\ncat b.js",
                &["a.js", "b.js"],
            ),
            ("cat <<END\n  END\ny.md\nEND\ncat b.js", &["b.js"]),
            ("cat << \"EOF\"\nx.md\nEOF\ncat b.js", &["b.js"]),
            ("cat <<\\EOF\nx.md\nEOF\ncat b.js", &["b.js"]),
            ("cat <<A <<B\nx.md\nA\ny.md\nB\ncat b.js", &["b.js"]),
            ("cat <<EOF\nx.md never closed", &[]),
            ("cat <<<x.md y.md", &["x.md", "y.md"]),
        ];
        for (command, expected) in cases {
            assert_eq!(named_paths(command), expected, "{command}");
        }
    }

    #[test]
    fn a_shift_is_not_a_heredoc() {
        let cases: [(&str, &[&str]); 4] = [
            (
                "grep -n '<<EOF' x.md\necho $((1<<3))\ncat src/b.js",
                &["x.md", "src/b.js"],
            ),
            ("((x<<2))\ncat b.js", &["b.js"]),
            ("echo $((10/2)) a.md", &["a.md"]),
            // Taken as arithmetic up to the end of its line.
            ("echo $((cd src && cat a.js) )\ncat <<EOF\nz.md\nEOF", &[]),
        ];
        for (command, expected) in cases {
            assert_eq!(named_paths(command), expected, "{command}");
        }
    }

    #[test]
    fn quote_forms() {
        let cases: [(&str, &[&str]); 2] = [
            ("echo $'it\\'s' src/a.js; cat b.js", &["src/a.js", "b.js"]),
            ("a=#x src/a.js", &["src/a.js"]),
        ];
        for (command, expected) in cases {
            assert_eq!(named_paths(command), expected, "{command}");
        }
    }

    /// Paths ADR 0029 accepts missing. When one of these is fixed, move it
    /// to `reads_name_their_files`.
    #[test]
    fn known_misses() {
        for (command, why) in [
            ("sed -n 1,5p \"src/a.js\"", "a quoted path"),
            ("cat Makefile README", "no extension"),
            ("cat .env", "a dotfile"),
            ("cat $HOME/a.js ${SRC}/b.js", "a variable"),
            ("git show HEAD:src/a.js", "a revision"),
            (
                "cat <<'E O F'\nx.md\nE O F\ncat b.js",
                "an end word with blanks never ends",
            ),
        ] {
            assert!(named_paths(command).is_empty(), "{why}: {command}");
        }
    }

    /// Words ADR 0029 accepts taking as paths. When one of these is fixed,
    /// move it to `reads_name_their_files`.
    #[test]
    fn known_extras() {
        // A dotted module name looks like a file; it counts once.
        assert_eq!(named_paths("python -m http.server"), ["http.server"]);
        // A `cd` is not followed: `a.js` is taken as the root's, not `src/a.js`.
        assert_eq!(named_paths("cd src && cat a.js"), ["a.js"]);
    }

    fn read(path: &str) -> serde_json::Value {
        serde_json::json!({ "path": path })
    }

    fn shell(command: &str) -> serde_json::Value {
        serde_json::json!({ "command": command })
    }

    #[test]
    fn a_shell_command_is_new_once_per_path() {
        let mut seen = Novelty::default();
        assert!(seen.other(&shell("sed -n 1,60p src/a.js"), None));
        assert!(!seen.other(&shell("sed -n 60,120p src/a.js"), None));
        assert!(
            seen.other(&serde_json::json!({ "cmd": "cat src/b.js" }), None),
            "the `cmd` key"
        );
        assert!(!seen.other(&shell("git status"), None));
    }

    #[test]
    fn a_tool_without_effect_counts_its_path() {
        let mut seen = Novelty::default();
        let grep = serde_json::json!({ "pattern": "barrel", "path": "src" });
        assert!(seen.other(&grep, None), "Pi's grep");
        assert!(!seen.other(&grep, None));
    }

    #[test]
    fn a_read_tool_is_new_by_its_arguments() {
        let mut seen = Novelty::default();
        assert!(seen.other(&shell("sed -n 1,9p src/a.js"), None));
        assert!(
            seen.read(&read("src/a.js"), None),
            "a read tool's first read of a file counts, as before"
        );
        assert!(!seen.read(&read("src/a.js"), None));
        assert!(seen.read(
            &serde_json::json!({ "path": "src/a.js", "offset": 40 }),
            None
        ));
    }

    #[test]
    fn the_shell_sees_what_a_read_tool_read() {
        let mut seen = Novelty::default();
        assert!(seen.read(&read("src/b.js"), None));
        assert!(!seen.other(&shell("cat ./src/b.js"), None));
    }

    fn project() -> WriteScope {
        WriteScope::new(Path::new("/work/project"), None, vec![])
    }

    #[test]
    fn paths_resolve_against_the_root() {
        let scope = project();
        let scope = Some(&scope);
        let mut seen = Novelty::default();
        let absolute = serde_json::json!({ "file_path": "/work/project/src/world.js" });
        assert!(seen.read(&absolute, scope));
        for same in ["cat src/world.js", "cat src/../src/./world.js"] {
            assert!(!seen.other(&shell(same), scope), "{same}");
        }
        assert!(
            seen.other(&shell("cat /elsewhere/src/world.js"), scope),
            "outside the root, as written"
        );
    }

    #[test]
    fn the_root_itself_names_no_file() {
        let scope = project();
        assert!(!Novelty::default().other(&shell("cat /work/project"), Some(&scope)));
    }

    #[test]
    fn without_a_root_absolute_and_relative_differ() {
        // A sandboxed run has no scope.
        let mut seen = Novelty::default();
        assert!(seen.read(&serde_json::json!({ "file_path": "/w/src/a.js" }), None));
        assert!(seen.other(&shell("cat src/a.js"), None));
    }
}
