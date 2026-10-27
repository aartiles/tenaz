# ADR 0004: A time limit per tool call

Status: accepted, 2026-10-27.

## Context

In the Digger 3D run of 2026-10-27 (Pi 1.4.0, Ternary Bonsai on
mlx-serve), a bootstrap session ran `./verify --all` on a script with an
endless loop: its argument loop had no `shift` for `--all`. The call ran
for 21 minutes until the operator killed it. Nothing in Tenaz could stop it
earlier:

- Pi's `bash` tool takes a `timeout`, but has none by default, and Pi has
  no setting for one.
- A steer does not help: Pi delivers it after the running tool call ends.
- The idle rule counts turns without progress, and a hung call never ends
  its turn.
- Only the session's wall clock (`sessionMinutes`, 90) would have ended
  it, with the whole session and its context.

fx 0.0.14 does not have the problem with its shell: `shell.run` returns
after `yield_time_ms` and leaves a command that still runs as a session the
model can poll.

## Options

### A. Count time in the idle rule

- Steers and aborts are what the rule can do. A steer does not reach a
  turn stuck in a tool call, and an abort ends the session.

### B. A Pi extension that gives `bash` a default timeout

- Fixes it at the source for Pi, not for fx's other tools.
- Tenaz runs Pi with `--no-extensions`, so the user's extensions stay out;
  it would need an extension of its own, kept in step with Pi's API.

### C. Lower `sessionMinutes`

- Ends long sessions that are working, and loses the session's context.

### D. A time limit per tool call in the runner

- The runner knows when each call started (`ToolCallStart`) and can stop
  the turn with the harness's abort: Pi's `abort` kills the command's
  process tree, ACP's `session/cancel` ends the turn.
- After the turn settles, the session gets a prompt that names the call,
  and goes on with its context.

## Decision

**D.** A new parameter, `toolMinutes` (10 by default, 0 turns it off).

- When the oldest open tool call passes it, the watchdog records a
  `tool-timeout` event with the call and asks for an interrupt: the runner
  aborts the turn and, when it settles, prompts the same session with the
  call, the likely causes and what to do (run it with `timeout`, fix the
  cause, go on).
- An aborted turn after an interrupt is not the session's end.
- The second time in a session, the session is aborted ("a tool call ran
  over N minutes twice") and the attempt counts. A call the harness does
  not stop gets its time again, so it is the second time.
- The protocol tells sessions to give long commands a time limit.

Ten minutes leaves room for a build or a browser test suite, and stops a
hung call long before the session's 90.

## Consequences

- A hung command costs ten minutes, not the session.
- A long command that is not hung (a full build, a model download) is
  stopped at ten minutes; a project that needs more sets `toolMinutes`.
- Tested with the fake harness only. Not yet run against Pi or fx: that
  Pi takes a prompt after `abort` in the same session, and that fx stays
  usable after `session/cancel`, are untested.
