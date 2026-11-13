# ADR 0031: Stop what verify left in a session of its own

Status: accepted, 2026-11-09.

## Context

Tenaz runs `verify` and heavy jobs in a process group of their own and
kills the group when the command ends, so a server a check started and
never stopped goes with it. A process that moved to a session of its
own is not in that group. Node's `spawn(..., { detached: true })` calls
`setsid`, and so do daemons.

In a raycaster run on Sushi with Qwen3.8 Flash Next, the model's
`checks/_helper.mjs` started `npx vite preview` with `detached: true`
and killed it only in a `finally` block. The check called
`process.exit` inside a callback, so the `finally` never ran. Every
`verify` left one more `npx` and `node vite` pair. 175 pairs (350
processes, about 2.5 GB resident) built up, swap filled, and the engine
ran out of memory once.

When the check ends, the parent of such a process is gone, and it is
adopted: by `launchd` on macOS, `init` (pid 1), or a subreaper above
it on Linux. Nothing then ties it to `verify` in the process
tree.

## Options

### A. A marker in the environment

- Give `verify` a unique variable (`TENAZ_VERIFY=<id>`); every process
  under it inherits it unless a program clears it. Afterwards, stop
  the processes whose environment holds it.
- Exact, and it does not depend on timing or the working directory.
- macOS no longer shows another process's environment: `ps -E` printed
  none on Darwin 27, even for the same user's processes, inside or
  outside Claude Code's sandbox. Linux has `/proc/<pid>/environ`. It
  would work on Linux only.

### B. Follow the tree while verify runs

- Read the process table every few hundred milliseconds while `verify`
  runs and remember each process under it; afterwards stop the ones
  still running.
- Misses a process whose parent exits between two readings, which is
  the usual shape of a daemon. Costs a `ps` every interval of every
  `verify`.

### C. A snapshot after verify ends

- Once the group is killed, read the table and stop a process when all
  of these hold: it was born since the command started (to the second,
  as `ps` reports ages); it leads a process group of its own, as
  `setsid` and Node's `detached: true` make it do; its parent adopts
  orphans (pid 1, or a `systemd` above Tenaz, the `systemd --user`
  subreaper on Linux) or is a member of the command's group (killed,
  not yet exited); it has no controlling terminal; it runs as Tenaz's
  user; it is not `git`; and its working directory is inside the
  project, at any depth. Its descendants go with it. Read the table
  again while a round finds a new one, for at most three rounds in
  all.
- One `ps` a round, and `lsof` (macOS) or `/proc` (Linux) only for the
  few processes that pass the rest. Same rules on both systems. After
  10 seconds a `ps` or `lsof` still running is killed and nothing more
  is stopped, as a pid listed earlier may have been reused; `lsof` can
  hang on a stale network mount.
- Misses a process that changed its working directory to one outside
  the project; on Linux one adopted by a subreaper not named `systemd`;
  and everything when Tenaz is pid 1, as in a container, where Tenaz
  adopts the orphans itself.
- Leaves alone what the user or the agent driving Tenaz starts in the
  project at that time without detaching it: a job of a shell has a
  terminal or stays in its shell's group, even once its shell has
  exited, and a command a terminal, tmux or Claude Code starts has a
  parent that adopts nothing. Taking every ancestor of Tenaz as an
  adopter, or any orphan as left behind, would have stopped those.
- Stops, with everything under it, a daemon the user or that agent
  starts from inside the project (a nested worktree included) in the
  window: `code .`, a new tmux server, `nohup setsid ...`. `git gc
  --auto` detaches the same way and is left alone by name: one killed
  mid-write leaves its locks behind. The window is `verify`'s run, or a
  heavy job's, up to its time limit. The local model's own dev servers
  are older than it: Tenaz runs `verify` and heavy jobs between
  sessions, or while a session waits for its next prompt.

## Decision

C, for `verify` and for heavy jobs, which share the code that starts and
reaps them. A works only on Linux, and B leaves a race in the case it
is for. C needs no change to the projects' checks and no new
dependency.

## Consequences

- A check that leaves a server running, detached or not, no longer
  builds up processes across a run, in the cases above. A warning on
  Tenaz's stderr names the pids it stopped; the journal does not record
  them.
- Processes are killed with SIGKILL, as the group is, from a listing
  that may be a few seconds old; a pid reused in between would be
  killed. Pids are handed out in order on macOS and Linux, so this
  needs one to wrap around in that time.
- `./verify` the agent runs in its own shell is not covered: Tenaz does
  not start it, and its leftovers are older than any later window.
- `crates/adapters/src/verify.rs` has a test with a real detached
  process in the project and one outside it, and `job.rs` one in the
  project; they pass on macOS and in CI on Ubuntu (`ubuntu-latest`).
  No real run has used this yet.
- (2026-11-13) Tenaz and the processes above it are never stopped. A
  supervisor that starts Tenaz in a session of its own, from the
  project, meets every rule above when Tenaz's first `verify` follows
  its start within the window, as on a run resumed straight into a
  review; stopping it, with everything under it, stopped Tenaz.
