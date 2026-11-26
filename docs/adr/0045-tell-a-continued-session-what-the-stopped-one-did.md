# ADR 0045: Tell a continued session what the stopped one did

Status: accepted, 2026-11-26.

## Context

When a session stops at its context or turn budget, or the engine refuses
one of its requests, a fresh session continues the same work (ADRs 0025,
0036). Its note only said to start with `git status`, `git diff --stat`
and the task's notes file. A stopped session rarely updates its notes, so
the fresh one reads the files again: over four benchmark runs, a builder
that continued stopped work took a median of 13 turns before its first
edit, the same as a fresh builder (143 and 266 sessions).

Tenaz sees every event of the stopped session: its tool calls with their
arguments and results, and its replies.

## Options

### A. A digest of the session's events

- Kept while the session runs, from the harness-neutral `SessionEvent`s
  the watchdog already sees: the files its write tools wrote without an
  error, its last check (a shell command that runs `verify` or a test
  runner) with the end of its result, and the text of its last reply.
- The same for every harness, and nothing to read back from disk. A
  session resumed after a pause has none, and gets the note alone.
- Misses files changed through the shell, and a check run some other
  way. With fx, a long result arrives clipped at its start.

### B. Read the stopped session's raw log

- `<label>.rpc.jsonl` holds Pi's own records with Pi and translated
  session events with an ACP agent: two formats to parse, and the log
  may be off (`persist: false`).
- Would also work for a session resumed after a pause.

### C. Ask the stopped session to write a hand-over

- The model knows best what it meant to do next.
- A session stopped at its budget has no room or turns left; one the
  engine refused cannot be asked anything. The wrap-up steer already asks
  for notes, and the runs show they are rarely written.

### D. Put `git diff --stat` and the notes into the prompt

- What the note already asks the session to run; it saves a turn or two
  but says nothing of the last check or what the session was doing.

## Decision

A. `trail` (tenaz-core) records the facts from each event the watchdog
handles; `briefing::continuation_digest` puts them into a section of at
most 2 KB after the continuation note. The note keeps its `git status`
advice, since the digest does not list shell edits. The recovery around
one session adds each run of it to one trail (`Trail::extend`): its
files after the earlier ones, its check if it ran one (else a write of
it marks the earlier check as before its last edits), and its last words
if it said any. So a second continuation, or the rerun of a continued
session the engine failed, hears of everything since the work started,
not only of the session just before it.

## Consequences

- Replayed over the 43 continued builder sessions of a Last Delivery run
  (fx), the digest came to 0.4 to 1.3 KB, with a last check in 36.
- Whether it cuts the turns before the first edit needs a live run; see
  `docs/COMPATIBILITY.md`.
