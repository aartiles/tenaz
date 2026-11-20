# ADR 0039: Stop what an agent session left running

Status: accepted, 2026-11-20.

## Context

ADR 0031 stops what `verify` and heavy jobs leave running in a session
of their own. What the agent starts from its own shell tool was not
covered: Tenaz does not start those commands, and their leftovers are
older than any later `verify`.

In base run 1 of 2026-11-19 (Tenaz `2f1b04c`, Pi 1.4.0), the reviewer
session `055-reviewer-T009` wrote `.tenaz/private/shot.mjs`. It ran
`npx vite preview --port 4173 --strictPort` with `detached: true`, and
its kill used the signal name `'SIGKIG'`, which does not exist. The
script hung until Pi's bash timeout. Each retry took the next port. Nine
`vite preview` servers were left, adopted by pid 1, with their working
directory in the project. About 8 hours later they still held ports
4173 and up and about 264 MB.

When a session ends, Tenaz kills the harness. What the agent started
detached outlives it and is adopted, as what a check leaves is once
`verify` ends.

## Options

### A. Tell the agent

- The prompts already ask the agent to stop what it starts. The model
  in this run tried to, and got the signal name wrong. A rule in a prompt
  does not stop a process.

### B. Run the harness in a process group of its own and kill the group

- ACP agents already run so: their client kills the group when the
  session closes. Pi runs in Tenaz's group. Either way this misses what
  moved to a session of its own, which is the case here.

### C. The sweep of ADR 0031, after every session

- When a session ends (closed after its last prompt, aborted, or failed
  mid-turn), run the same search with the session's start as the
  window and its working directory (the project) as the root: born
  since, leads its own group, no terminal, same user, not `git`,
  working directory inside the project, adopted by pid 1 or a
  `systemd` above Tenaz. Its descendants go with it. "Its parent is
  still in the command's group" does not apply: Pi's group is Tenaz's,
  where the engine may run too, and an ACP agent's group is gone.
- Same rules and limits as ADR 0031, and the same code.

### Sparing the engine by time or by port

- The engine is the one detached process Tenaz may start itself (see
  the Decision). It could be told apart by the port it listens on:
  `lsof -iTCP:<port> -sTCP:LISTEN` on macOS, `/proc/net/tcp` on Linux.
  That needs a new process-table call on each system, and finds only
  the process that listens: a supervisor above it (Splash's Python
  parent) or a worker beside it would still be stopped, and an engine
  still loading listens on nothing yet.
- By time: note when Tenaz's own engine commands run, and spare what
  was born then. No new system call; it also spares what the agent
  starts in that time, which is missed.

## Decision

C, for every agent session (builder, reviewer, bootstrap, replan,
split, final review, the preflight), whatever the harness. A harness
wrapper does it when the session is closed, after the harness has
exited, and when the harness fails to open the session; a session that
fails mid-run is now closed at once rather than dropped, so the sweep
runs then too. The fake harness is not wrapped, so `tenaz demo` and the
tests run no `ps` after its sessions.

What Tenaz itself started and still runs is never a candidate: the
harness, the proxy and the MCP server are Tenaz's children or run in
its process. The engine is the exception: an `--engine-start` script
may start it detached (`setsid`, or an engine that daemonizes), and it
may run from inside the project: the README says to start `tenaz run`
from the project directory, and the script inherits that directory.
When it restarts the engine during a session, the engine is born in
the window and adopted by pid 1 once the script returns. So Tenaz notes
when each of its engine commands runs, with its wait for the model to
serve or go, and spares what was born then, widened by 5 seconds on
each side, for rounding and for small differences between the wall
clock and the one `ps` ages come from. `ps` may take seconds on a
machine short of memory, so Tenaz reads its clock before and after
each `ps`. The session's start is the later of the two readings'
starts, so nothing older is taken. Each span runs from where the
second reading puts its start to where the first puts its end, so the
engine is found inside whenever `ps` read its age. The first reading's
spans are kept too, in case the clock was set back between the two.
These times, and the session's start, are read from
the wall clock. `Instant` stops while the machine sleeps, and `ps`
ages do not: after a sleep, a span read from it would no longer cover
the engine.

## Consequences

- What an agent session leaves running in a session of its own, in the
  project, is stopped when the session ends. A warning on Tenaz's stderr
  names the pids and the session's label; the journal does not record
  them, as for `verify`.
- The window grows from the seconds of one `verify` to a whole session.
  Sessions run back to back, so it is in effect the whole run. A daemon
  that the user or an agent driving Tenaz starts from inside the project
  while Tenaz runs (`code .`, a new tmux server, `nohup setsid ...`, an
  engine restarted by hand from there) is stopped after the current
  session. The maintainer accepted this. A process with a terminal, one
  that stays in its shell's group (a plain `&` job), and one whose
  working directory is outside the project are left alone. The skill and
  the README say not to start background processes from inside the
  project while a run goes.
- A build or tool daemon that starts detached from inside the project
  during a session, from the agent or from the user's editor
  (watchman, the Gradle daemon, sccache's or Bazel's server), is
  stopped too. It is started again when next needed.
- A process the agent started detached and meant to keep across
  sessions (a dev server for the next session) is stopped too. The next
  session starts it again if it needs it.
- What an agent session started while one of Tenaz's own engine
  commands ran is spared with the engine. An engine restart takes
  seconds to minutes; what the agent starts in that time and leaves is
  missed.
- An engine that a stop script stops and that another process starts
  again later, not through `--engine-start`, is not spared.
- On macOS `ps` ages come from the wall clock, so a clock set back or
  forward moves the window and the spans with the processes. Linux
  `procps` counts them from boot: there a clock step of more than the
  slack during a session moves the spans off the engine, and a step
  forward widens the window, so a detached process from the project
  born that much before the session may be stopped. Reading the boot
  clock (`/proc/uptime`, as `procps` does) would avoid it on Linux;
  Tenaz does not yet, to keep one clock on both systems.
- An ACP agent is killed as its session closes, not waited for. If it
  has not exited by the time `ps` reads the table, what it left still
  has a parent and is missed; no later sweep takes it, as it was born
  before that sweep's window.
- A run killed by a signal Tenaz does not handle, or one that crashes,
  sweeps nothing after its last session.
- Tested with the scripted process table; with a real detached process
  started during a session of a test harness (it stays without the
  sweep), and by a harness that then failed to open; with one started
  while an engine span is open (it stays; it is stopped without the
  span), and one started after a span that ended earlier (it is
  stopped). Run on macOS; not yet in CI on Linux. Not yet seen in a
  real run.
