# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

### Added

- `tenaz` binary with `init`, `run`, `status`, `finish` and `params`.
- `--help` describes the workflow with a non-interactive example; `run --help`
  lists the exit codes and `status --help` what each phase means.
- `tenaz run` exits with 0 when the goal is met, 3 when it completes with
  gaps, 4 when it is blocked and 5 when it pauses; 1 is an error and 2 a
  usage error.
- `tenaz status --json` prints the status as one JSON object (`phase`,
  `state`, `pid`, `blocked_reason`, `last_error`, task counts, current task,
  session in progress, `origin`); the text status gains a `State:` line.
- `tenaz run` holds a lock on the project while it works: a second run on
  the same project is refused, and `status` tells a working run (`running`)
  from one that was killed or failed (`interrupted`).
- `tenaz guide` prints an Agent Skill for delegating goals to Tenaz from
  another agent.
- Setup wizard: `tenaz init` without `--model`, in a terminal, finds the
  coding agents on `PATH`, the engines on their usual ports with their
  models, context windows and vision, and ComfyUI, then prints the
  equivalent command line. `--interactive` forces it with a piped stdin.
- `run.json` pins how to launch the run (`launch`: engine API and
  start/stop commands, proxy, harness executable, ComfyUI and workflows,
  media timeout), so `tenaz run` needs no flags; a flag given to `run`
  replaces the pinned value and is saved. `run` and `status` default to
  the current directory.
- On a repository that already has commits, `init` works on a new branch
  `tenaz/<goal>` in a worktree next to the repository instead of on the
  current branch, refuses a tree with uncommitted changes, and records the
  starting branch and commit in `run.json` (`origin`). `--in-place` keeps
  the old behaviour; `--worktree` and `--name` choose the directory and
  branch. The setup wizard asks which, `run` and `status` say where the
  run works, and both point to the worktree from the original checkout.
  A directory inside a repository that tracks nothing in it, such as an
  ignored directory with seed files, is a new project rather than refused.
- `tenaz finish`: a branch in the original repository with the run's
  result as one commit on the starting commit, without `.tenaz/` (nor,
  for older layouts, the root files and `.gitignore` lines Tenaz added);
  the run branch keeps the full history, `--remove-worktree` removes the
  worktree. Nothing is pushed.
- The run's state lives in `.tenaz/` (it was `.agent/`). A project still
  in the old layout is not run: `init`, `run` and `status` say how to move
  it (`mv .agent .tenaz`, the `.gitignore` lines, `--refresh-templates`).
- Tenaz keeps all its files under `.tenaz/`: the brief is
  `.tenaz/brief.md` (it was `BRIEF.md`), the shared rules are
  `.tenaz/protocol.md` (they were the project's `AGENTS.md`) and the ignore
  rules are in `.tenaz/.gitignore` (they were appended to the project's
  `.gitignore`). The protocol now starts every session's prompt instead of
  relying on the agent to load `AGENTS.md`, so `init` works in a project
  that has its own `AGENTS.md`, and sessions may not edit the project's
  `AGENTS.md` or `CLAUDE.md`. A run started with `BRIEF.md` at the root is
  not run: `run` and `status` say how to move it (`git mv`,
  `--refresh-templates`).
- `existingProject` parameter. `init` sets it when the repository already
  has commits: `verify` is then `.tenaz/verify`, a wrapper around the
  project's own tests, new checks go into the project's test suite, and
  bootstrap plans only the change instead of a skeleton.
- Without `--engine`, `init` asks the engine at `--base-url` which API it
  speaks (Sushi or generic) and names the provider after it.
- Orchestrator: bootstrap, build, verify, review,
  split or block, replan and final review, with pause and resume.
- Lease manager keeping one engine resident at a time.
- OpenAI-compatible engine proxy that holds requests while engines swap.
- Harness adapters for fx over ACP and Pi over its RPC mode, plus a scripted
  fake harness.
- Engine adapters for Sushi and generic OpenAI-compatible engines; ComfyUI
  media adapter; MCP server exposing `generate_image`.
- CI on macOS and Linux and a tagged release workflow.
- `requests.jsonl` rows carry the engine's own `timings` per request
  (`server`: prefill and decode milliseconds, cached tokens, and draft
  counts when the engine reports them), and `status` prints the draft
  acceptance.
- `tenaz watch` follows a run from its files as it works, at four levels:
  `summary` (the run log), `actions` (each tool call and reply on one line,
  with context, output, thinking and decode speed), `full` (whole replies,
  the start of tool results) and `debug` (the harness's log too). `--once`
  prints the current session and exits.
- A context budget lowered because the engine ran out of memory is raised
  halfway back after `capacityRecoverSessions` sessions in a row (4 by
  default; 0 keeps it lowered) that the engine held without critical memory
  pressure. A raise the engine overflows again lowers it as before and
  doubles the sessions the next raise waits for.
- A session is steered to wrap up when, at the pace its context grew over
  the last requests, it would reach the context stop within
  `contextSteerTurns` turns (10 by default; 0 steers only at the budget), so
  a slow-to-react model can still write its handoff before it is stopped.
- An attempt that uses up its context continuations and still fails
  `verify` is split at once instead of retried against the same budget
  (`contextSplit`, on by default), when the split depth allows.
- A builder's briefing shows only the spec requirements its card requires,
  with a pointer to the whole spec (`citedSpec`, on by default; a card that
  names none the spec has gets the whole spec). A session that continues
  stopped work no longer gets the repository map in its prompt.
- A session steered for going idle is asked to write what it already knows
  (the files and facts the change depends on, and its plan) in its task's
  notes before making the change, and the next attempt's briefing includes
  those notes.
- Reading something not read before counts as progress for the idle rule,
  like a file change; repeated reads of the same thing still count as idle.

### Fixed

- Ctrl-C during the engine preflight pauses the run instead of ending it
  with a failed preflight.
- A harness that ends before its turn settles is retried like an engine
  failure instead of being charged as a task attempt, and its exit code and
  stderr tail are journaled (`harness_exit`).
- fx writes one ACP log per session instead of overwriting a shared one.
- An ACP session handles a proxy observation before the prompt's end, so
  per-request usage is no longer occasionally lost or attributed to the
  wrong turn.
- The engine proxy adds the missing `action` to a tool call that sends a
  `command` without it, when exactly one variant of the argument takes a
  `command` (models drop it from fx's `shell` call, which fx then rejects).

### Security

- `SECURITY.md` and the README say what Tenaz does not isolate (the agent's
  commands, `verify`, the unauthenticated local proxy and MCP server) and
  how to report a vulnerability privately.
