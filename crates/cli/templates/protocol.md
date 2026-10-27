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
  same way when a check passes for you and fails for the orchestrator.
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
  fails.

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
