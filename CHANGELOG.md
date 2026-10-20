# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

### Added

- `tenaz` binary with `init`, `run`, `status`, `finish` and `params`.
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
- `tenaz finish`: a branch in the original repository with the run's
  result as one commit on the starting commit, without `.tenaz/` or the
  files and `.gitignore` lines Tenaz added; the run branch keeps the full
  history, `--remove-worktree` removes the worktree. Nothing is pushed.
- The run's state lives in `.tenaz/` (it was `.agent/`). A project still
  in the old layout is not run: `init`, `run` and `status` say how to move
  it (`mv .agent .tenaz`, the `.gitignore` lines, `--refresh-templates`).
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

### Fixed

- A harness that ends before its turn settles is retried like an engine
  failure instead of being charged as a task attempt, and its exit code and
  stderr tail are journaled (`harness_exit`).
- fx writes one ACP log per session instead of overwriting a shared one.
