# AGENTS.md

Tenaz is a Rust workspace that drives a local model through a coding agent
until a goal's `verify` passes. Read before changing code:
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) (crates, ports, conventions),
[`CONTRIBUTING.md`](CONTRIBUTING.md) (crate map, tests),
[`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) (real tools, known gaps).

## Setup

- Enable the hooks once: `git config core.hooksPath .githooks`. `pre-commit`
  runs fmt and `scripts/check-architecture.sh`; `pre-push` runs what CI runs.
- `cargo` may only be in `~/.cargo/bin`; the hooks add it to `PATH`.
- `cargo test --workspace` needs no model and takes about two minutes.

## Rules the tools cannot check

- Pure logic goes in `tenaz-core` with unit tests; side effects go in an
  adapter behind a trait in `ports`. Do not add a port without a fake.
- Tests never need a model, engine, fx, Pi or ComfyUI. A new adapter comes
  with a fake or a recorded transcript. Checks against real tools are written
  up in `docs/COMPATIBILITY.md`, not added to `cargo test`.
- A flaky test is a bug: find the race, do not retry or sleep around it.
- Fix a clippy lint instead of allowing it. When an `#[allow]` is right, say
  why in a comment on the same line.
- Keep stable what scripts read: field names in `run.json` and
  `journal.jsonl`, `tenaz status --json`, and the exit codes of `tenaz run`.
- Prompt templates (`crates/cli/templates/`) and the agent skill
  (`crates/cli/skill/SKILL.md`) are compiled into the binary. A change to the
  CLI's behaviour or flags updates `SKILL.md` and the README with it.
- A decision with real alternatives gets an ADR in `docs/adr/`.

## Every change

- One behaviour per commit, with its docs: a user-visible change adds a line
  under `Unreleased` in `CHANGELOG.md`; update `README.md`,
  `docs/ARCHITECTURE.md` or `docs/COMPATIBILITY.md` when they stop being true.
- Commit subject: imperative, says what Tenaz now does. Body: why, and what
  was and was not verified (for example "not yet run on a Linux kernel").
  Anything untested against a real tool also goes under "Known gaps".
- Write docs and comments in plain, short English, like the existing ones.

## Never without the maintainer's explicit approval

Push to a remote, create a tag (a `v*` tag starts the release workflow),
publish a crate, or change CI secrets or release settings. Approval covers
one publication, not the next.
