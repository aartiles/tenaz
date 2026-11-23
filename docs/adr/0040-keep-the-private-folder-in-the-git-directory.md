# ADR 0040: Keep the private folder in the git directory

Status: accepted, 2026-11-23. Extends ADR 0038, which stays as the
fallback.

## Context

ADR 0038 chose to notice a removed `.tenaz/private/` and left "move it
outside the project" for later. On 2026-11-23, in a 26-hour run (Digger
3D, Pi 1.6.0, Inkling 2 Mini on mlx-serve), two builder sessions of T017
removed it again. The detection worked: two `private-removed` records,
the lock taken again, the run finished. But Pi's session files, the
proxy's request records and the memory samples of sessions 001 to 069
were gone: `tenaz status` counted about 1,095 of 2,600 engine requests,
and half the run could not be matched request by request to a benchmark.
The transcript that would show the command went too. Noticing is not
enough; the files must not be in reach.

What a run keeps there: `orchestrator.log`, each session's prompt, events,
reasoning and stderr, the run lock, `requests.jsonl`, `memory.jsonl`,
`tool-repairs.jsonl`, Pi's session files and agent directory, and the
`HOME` of fx, OpenCode, Qwen Code and dsh. The agent itself is told to
put temporary files and contact sheets in `.tenaz/private/`, and the
media tools write their outputs there for the agent to read and copy.

## Options

### A. Deny the folder in the sandbox

- The harness runs in the sandbox with the shell commands it starts:
  Seatbelt and Landlock see one process tree and cannot let the harness
  write a folder while denying its shell. Denying only Tenaz's own files
  (the log, the records) works with Seatbelt, but Landlock cannot deny a
  path inside a folder it lets a process write, and the sandbox's guard
  puts protected files back after each session, which would undo the log
  lines of that session. It also does nothing without the sandbox, the
  default.

### B. A folder per run in the user's state directory

- Out of the project and of the git directory, so with the sandbox on
  Tenaz's own files could be left out of what the sandbox may write.
- Found by a hash of the project's path: moving or copying the project
  loses it, removing the project leaves it behind, and nothing cleans it.

### C. The git directory of the project or worktree

- `git rev-parse --absolute-git-dir`: `.git/tenaz/`, or for a run in its
  own worktree `<repository>/.git/worktrees/<name>/tenaz/`. It goes with
  the checkout, is removed with the worktree, and git never touches a
  folder it does not know there.
- None of the commands that clean a project reaches it: `rm -rf .tenaz`,
  `git clean -fdx`, `git stash --all`, `find . -delete`. A command must
  name the git directory to remove it.
- The sandbox already lets the agent write the git directory (it commits),
  so the harness can write there with or without it. It does not stop a
  command that names the folder.

### D. Keep it, and only notice (ADR 0038)

- What happened on 2026-11-23.

## Decision

**C, with D kept.** The private folder is `<git dir>/tenaz/`. Tenaz
finds it with git when a command opens the project. `.tenaz/private/`
stays in the project, out of git, as the agent's scratch folder:
temporary files, contact sheets, media outputs and job logs, which the
agent reads and which a harness that confines its file tools to the
project could not read elsewhere. Nothing Tenaz needs is kept there.

A project that is not the top of a git working tree keeps the folder in
`.tenaz/private/` as before: one whose `.git` was removed, or that sits
inside another repository, where several projects would share one git
directory. Tenaz makes a repository at `init`, so this is rare.

A run of the old layout (a `.tenaz/private/` with `run.lock` or
`orchestrator.log`, and no `<git dir>/tenaz/`) is read in place by
`tenaz status` and `tenaz watch`. `tenaz run` moves it, holding its run
lock so that no other run is using it, and says so in `orchestrator.log`;
when another run holds that lock, it stops and names it.
Its `jobs/` and `media/`, which task notes name by their path, stay in
`.tenaz/private/`; other scratch files of the agent's move with it. When
it cannot be moved (another file system: the rename fails), the run
keeps it where it is and its log says why.

`tenaz status` prints the folder, and `tenaz status --json` has it as
`private_dir`. There is no link to it in the project: a link the agent's
commands can follow (`rm -rf .tenaz/private/run/`) is a way back to what
this decision takes away.

ADR 0038's checks stay: a session that names the folder and removes it,
or its prompts, or the run lock, is noticed as before, and the messages
name the folder.

## Consequences

- An agent that cleans the project, as on 2026-11-19 and 2026-11-23,
  removes only its scratch folder; the run's logs and records are kept.
- With the sandbox on, the agent can still remove the folder by naming
  it; ADR 0038 notices that.
- Paths change for people and scripts that read `.tenaz/private/`: read
  `tenaz status --json`'s `private_dir` instead. A run's folder is not in
  the project copy: copy it from there when you keep finished runs.
- Two runs in one checkout share one git directory: a new run
  initialized `--in-place` after removing `.tenaz/`, or on another
  branch, would find the earlier run's folder. `init` renames it to
  `tenaz-before-<seconds>-<pid>` in the git directory and says so, so the
  new run's log and records start empty; resuming the earlier run on its
  own branch then means moving that folder back to `tenaz` by hand. A
  folder whose run still holds its lock is left, with a warning. An
  old-layout `.tenaz/private/` left by hand after `run.json` was removed
  is not set aside: the new run's first `tenaz run` takes it as its own.
  Removing `.tenaz/` no longer removes the earlier run's logs.
- A `tenaz watch` started before the move follows the old path until it
  is started again.
- `git worktree remove` removes the folder with the worktree, as it
  removed `.tenaz/private/` before.
- Each harness's `HOME` is outside the project now. A harness whose file
  tools are confined to the project (Qwen Code) may refuse to read a
  pinned skill's other files through the link in its `HOME`. Not yet
  checked with a real harness.
- A run of the old layout is as exposed as before until a `tenaz run` of
  this version moves its folder. While an earlier version runs it and its
  `.tenaz/private/` is removed and not yet written again, a new command
  finds no old run there and looks in the git directory, so a second
  `tenaz run` could start beside it, and when only its lock file was
  removed, `tenaz run` takes it for a run from before the lock and moves
  the folder from under it, as it would under a still-working version
  from before the lock. An earlier version started on a run after
  the move takes a lock of its own in `.tenaz/private/`.
- Tested with the scripted harness (a builder that removes the scratch
  folder, and one that removes the private folder) and with a real git
  repository; not yet in a real run, nor on Linux.
