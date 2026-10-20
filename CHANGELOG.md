# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

### Added

- `tenaz` binary with `init`, `run`, `status` and `params`.
- Setup wizard: `tenaz init` without `--model`, in a terminal, finds the
  coding agents on `PATH`, the engines on their usual ports with their
  models, context windows and vision, and ComfyUI, then prints the
  equivalent command line. `--interactive` forces it with a piped stdin.
- `run.json` pins how to launch the run (`launch`: engine API and
  start/stop commands, proxy, harness executable, ComfyUI and workflows,
  media timeout), so `tenaz run` needs no flags; a flag given to `run`
  replaces the pinned value and is saved. `run` and `status` default to
  the current directory.
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
