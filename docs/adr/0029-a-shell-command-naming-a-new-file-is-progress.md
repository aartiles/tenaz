# ADR 0029: A shell command naming a new file is progress

Status: accepted, 2026-11-08.

## Context

The idle rule stops a builder after `idleAbort` turns (20) in which the
working tree did not change and nothing new was read. It catches a model
that loops. A read counted only when the tool call was a read tool. Reads
through the shell (`sed -n`, `grep`, `cat`) never counted, and the working
tree fingerprint leaves out `.tenaz/private/`, where builders write their
probe scripts.

In the raycaster run (2026-11-08, dsh and Qwen3.8-Flash-Next on Sushi
1.5.1) the rule stopped sessions 042, 050, 054 and 055. In each the
builder was reading checks it had not opened before with `sed -n` and
`grep`, and writing and running probe scripts under `.tenaz/private/`. Two
of them cost T019 two of its three attempts. The run went on only after
`idleAbort` was raised to 40 by hand.

## Options

### A. Raise `idleAbort`

Simple, and a looping model runs that much longer before it is stopped.
It does not tell investigation from a loop.

### B. Count every new shell command

The text of a command almost always changes (`tail -60`, then `tail -40`),
so a looping model would reset the count with each call. The rule would
stop almost nothing.

### C. Fingerprint `.tenaz/private/` too

A probe script rewritten every turn would count as a change every turn,
and a read through the shell would still not count.

### D. A shell command that names a path not seen before is a read

Take the words of the command that look like paths (with a `/` or an
extension), leaving out quoted text and heredoc bodies, which hold
patterns and code. A path not read or named before resets the count, as a
read tool's new read does. Reading the same file again, in any range, does
not; nor does running a probe again. A tool that neither reads nor writes
by its effect but takes a `path` (Pi's `grep`, `find`, `ls`) counts its
path the same way.

## Decision

D. It counts what a model does when it learns something, opening a file
it has not seen, and each path counts once per session, so a loop over
the same files and commands is still stopped. Paths are not checked on
disk: a name that does not exist counts once, as a read tool's read of a
missing file does.

## Consequences

- Replayed through the watchdog, the four stopped sessions reach at most
  9 idle turns (042), 3 (050), 5 (054) and 4 (055), against 20.
- A model can keep the count down by naming a new path every few turns,
  for example by writing `probe3.mjs`, `probe4.mjs`. The turn and context
  budgets still bound such a session.
- Paths under the project root count the same absolute or relative. In a
  sandboxed run, which has no write scope, the root is not known and an
  absolute path counts once more.
- A read tool still counts by its arguments, as before: its first read of
  a file the shell already named counts, and so does a new range.
- The words are a guess from the text, not a shell parse. Quoted text,
  `#` comments, heredoc bodies and arithmetic (`$(( ))`, which ends with
  its line) are skipped. A path in quotes, a name without an extension
  (`Makefile`), a dotfile (`.env`), a path after `$VAR/` or in
  `HEAD:src/a.js` is not seen, nor is anything after a heredoc whose
  quoted end word has a blank. A word that is not a file
  (`python -m http.server`) counts once. A `cd` is not followed: after
  `cd src`, `a.js` is taken as the root's.
- A file written with a write tool counts once when it is first read
  back; one written through the shell counted when the command named it.
