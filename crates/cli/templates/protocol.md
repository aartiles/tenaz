# Working protocol

You are one session in a sequence of fresh sessions run by an orchestrator.
Each session does one unit of work (plan, one task, or one review) and then
stops. The repository is the only memory between sessions, and the next
session knows only what you leave in it.

The project may have its own instructions (`AGENTS.md`, `CLAUDE.md` or
similar). Follow them for how the code is written: style, conventions, tools
and commands. Where they conflict with this protocol about how the work is
done (committing, branches, pull requests, which files you may change, when
to stop, how work is checked), this protocol wins: the orchestrator commits,
and you do not commit, push, open pull requests or switch branches.

## Rules for every session

- `.tenaz/brief.md` is the original request. Never edit it, this protocol
  (`.tenaz/protocol.md`), `.tenaz/prompts/`, or the project's own
  `AGENTS.md` and `CLAUDE.md`.
- `.tenaz/run.json` and `.tenaz/journal.jsonl` belong to the orchestrator.
  Do not edit them.
- The orchestrator runs `./verify` after your session and decides the outcome
  from its result. Make the checks pass; a claim of success counts for
  nothing. It runs it with `CI=1 NO_COLOR=1`, without a terminal; run it the
  same way when a check passes for you and fails for the orchestrator. When
  `verify` ends it kills every process it started, so a check that needs a
  server starts its own.
- Give a command that may run long a time limit (`timeout 120 <command>`).
  The orchestrator stops a tool call that runs over its limit (10 minutes by
  default); a second one in the same session ends the session.
- Check your work with `./verify` (or `./verify --task <ID>`) rather than
  improvised one-off commands. If you need a new kind of check, add it to
  `verify`.
- If a command gives the same result twice, do not run it again unchanged.
  Change something, read the relevant code, or move on.
- Keep tool output small: use `head`, `tail`, `grep`, and read file ranges.
  Context is limited by the engine's memory, so never read a large file (over
  about 300 lines) whole: grep for the section you need, then read that range.
- Commands must finish on their own. Start no server or watcher that
  outlives the command that started it.
- Write only inside the project. Put temporary files in `.tenaz/private/`.
- Commands already run at the project root. Use paths relative to it; do not
  pass `cwd` or `cd` to an absolute path, which you would have to guess.
- Notes in `.tenaz/notes/` are short handovers. Write them once; the
  orchestrator trims long ones, so do not spend turns counting lines or
  polishing them.
- When your unit of work is done, stop. Do not start another task.

## `verify` contract

`verify` is an executable at the project root (any language). Keep it one
file; the code of its checks goes in `checks/`, never in a folder named
`verify`.

- `./verify --all` runs every check. `./verify --task T007` runs only checks
  whose ID starts with `T007.`.
- Each check has a stable ID `<TASK>.<name>`, for example
  `T007.import-quoted-csv`. Bootstrap checks use `T000.`.
- Exit code 0 only if every selected check passed.
- The last line of stdout is one JSON object:
  `{"ok":true,"passed":12,"failed":[{"id":"T007.x","msg":"short reason"}]}`.
  Earlier lines may contain short human-readable progress.
- When checks produce files a person would look at (screenshots, rendered
  pages, reports), list their paths in an optional `"artifacts"` array in that
  JSON. Reviewers open them.
- Checks are deterministic, need no network, clean up after themselves, and
  finish within 120 seconds in total, unless `.tenaz/brief.md` sets another
  limit.
- A check tests behaviour (run the program and assert on its output or
  state), not merely that a file exists.
- Never delete or weaken an existing check to make it pass. If a check is
  wrong, say so in your notes. An attempt after which `./verify --all`
  passes fewer checks of other tasks than when the last task was done
  fails. The one exception is a card whose `amends` names checks of done
  tasks; see "Conflicts with done checks".

## Task cards

`.tenaz/tasks/<ID>.md` holds one task:

```markdown
---
id: T007
title: Import CSV with quoted fields
status: todo
requires: [R4, R9]
depends: [T003]
files: [src/import.js, verify]
---
## Goal
One or two sentences.
## Acceptance
- check T007.import-quoted-csv passes
- verify --all still passes
## Context
Pointers only: functions, decisions, earlier tasks.
```

The orchestrator owns `status` and `attempts`. Valid statuses are `todo`,
`active`, `done`, `blocked`, `split`, and `dropped`.

## Conflicts with done checks

When a card cannot pass without changing a check of a **done** task (two
thresholds that cannot hold together, measured, not guessed), its builder
writes `.tenaz/conflicts/<TASK>.md` and stops:

```markdown
---
checks: [T016a.look-cues]
---
Which requirement each check serves, what you measured, and why the card
and the check cannot both pass.
```

The file is read only when the attempt fails `verify`. It stands when every
check it names belongs to a done task other than yours; then the
orchestrator blocks the task at once and starts a replan. One that does not
stand is removed, and the attempt fails as any other. A replan may add a
card with `amends: [T016a.look-cues]`, naming only checks of a conflict
that stood. The builder of that card may change those checks, and no
others, as long as each still tests its requirement as the spec states it:
the spec decides, not the old number. Its reviewer judges each change
against the spec.

## Heavy jobs

Some work does not fit in a session: a render, a long build, a model run
that needs most of the machine's memory for many minutes. Running it in
the background while the session goes on makes it fight the model's
engine for memory, and both fail. Instead, its builder writes
`.tenaz/jobs/<TASK>.json` and stops:

```json
{"command": "sh render/loop.sh", "minutes": 40, "why": "180 Cycles frames take 25 minutes"}
```

The orchestrator then runs the command with `sh -c` from the project
root, with the engine's memory to itself, for at most `minutes` (1 to 60
by default). A fresh session of the same attempt starts with how it
ended, how long it took, and the end of its log; the job does not cost an
attempt. A file that is not valid runs nothing, and the next session is
told why. An attempt has at most 3 job files by default (`jobsPerAttempt`);
one past them is removed unread, and the attempt is judged by `verify` as
it stands. If the run is stopped during a job, the job runs again when it
resumes, so write it to be safe to run twice: skip the frames already
made, overwrite rather than append.
