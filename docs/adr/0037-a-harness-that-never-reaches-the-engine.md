# ADR 0037: A harness that never reaches the engine is not an engine failure

Status: accepted, 2026-11-19.

## Context

A session that ends before its turn settles, without a tool call running,
is rerun without charging the task, with the engine backoff
(`engineRetryBaseSec` doubled per rerun, up to 15 minutes). After
`engineRetries` reruns (6) the run blocks with `engine unavailable`. That
is right when the engine failed, or when the harness died in the middle
of a turn.

In the Raycaster Shooter run of 2026-11-19 (Tenaz 0.8.0, Pi 1.4.0,
Qwen3.8-27B on Splash 1.7.0) a shell command of the agent's removed
`.tenaz/private`, and with it Pi's agent directory. Every later Pi
session exited 1 in under a second with `Model "<provider>/<id>" not
found`, before it read its prompt. The engine was serving. Tenaz waited
through its six reruns, 45 minutes, and blocked with `engine unavailable`,
which sent the reader to the wrong place. (Tenaz now writes the agent
directory again before every session, so this cause is gone; others,
such as a broken harness installation, end the same way.)

Such a session has no turn started, no reply, no tool call and no engine
error: the harness never asked the engine anything. A harness that dies
after its turn started (Pi killed during prefill) did ask it, and is
rerun as before.

## Options

### A. Keep it an engine failure

- No change. A harness that cannot start costs 45 minutes and a wrong
  diagnosis.

### B. Block at the first such session

- Fast, but a fault that passes (a lock, a cache being written, a port
  still held) stops a run that one rerun would have saved.

### C. Rerun once, then block when the engine serves

- The first such session is rerun as now. A second in a row, when the
  engine lists the run's models, blocks the run as `harness failed:` with
  the harness's exit and stderr.
- When the engine does not serve, the session may have ended because of
  it (an agent that checks the engine as it starts): the reruns go on as
  before, within `engineRetries`, and the harness is blamed at the first
  such session after the engine serves again.

## Decision

**C.** `SessionMetrics::exit_before_engine` tells such a session from
the others; the recovery counts them in a row (any session that reached
the engine starts the count again) and, from the second, names the
harness as suspect beside its usual step; the runner asks the engine and
blames the harness when it serves, before that step, so even with no
engine retries left. The
block reason quotes the harness's exit note with its stderr tail, on one
line, and says to fix the harness and run with `--resume`. A builder's
attempt is given back, as for the other blocks.

## Consequences

- A harness that cannot start blocks the run after one rerun and one
  backoff (60 s by default), not 45 minutes.
- With `engineRetries=0` there is no rerun, so no second session: the
  first such session ends the run as `engine unavailable`, as before.
- The journal still records the rerun as an `engine-retry` with cause
  `harness`; the new block reason starts `harness failed:`. The exit
  codes of `tenaz run` do not change: it is a block like the others.
- An agent that exits without a turn because the engine is down, while
  the engine's model list still answers, would be blamed. No agent Tenaz
  drives is known to do that.
- Tested with the scripted harness (a session that writes Pi's message to
  stderr and exits 1 before its first turn), with the engine serving and
  with it going down as the harness ends; not yet seen against a real
  harness.
