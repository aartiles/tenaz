# ADR 0005: Fakes for the orchestrator's ports

Status: accepted, 2026-10-27.

## Context

Four ports have no fake: `Vcs`, `VerifyRunner`, `MemoryProbe` and
`EngineMetricsSource` (`crates/core/src/ports.rs`). `CONTRIBUTING.md` and
`AGENTS.md` ask for one with every port. Without them:

- The orchestrator (`orchestrator/phases.rs`, `session.rs`, `mod.rs`, about
  2,800 lines) has no tests in `tenaz-core`. Every test of it lives in
  `crates/adapters/tests/` and runs real `git` and a real `sh` `verify`.
- Each of those tests drives a whole run and takes 5 to 12 seconds. A test
  that wants one recovery path still plays the whole scripted run.
- The memory sampler and the engine counters are never exercised: the
  tests pass `memory: None` and `engine_metrics: None`.

Measured on `tenaz demo` (10.3 s, macOS): about 900 `git` calls, 378
`status` and 391 `diff`, mostly the working-tree fingerprint the runner
takes after every turn (`session.rs:177`). A `status` and `diff` pair
costs about 10 ms, so `git` is about 5 to 6 s of the run.

`tenaz-core` may read and write files (`Project` does), but may not spawn
processes or depend on another Tenaz crate (`scripts/check-architecture.sh`),
so it cannot use `GitRepo` or `VerifyProcess`, even in tests.

## The easy two

`MemoryProbe` and `EngineMetricsSource` return a value and have no state
worth modelling. Their fakes return readings a test gives them, in order,
and repeat the last. No alternatives are worth an ADR; they are here so the
four arrive together.

## `VerifyRunner`

The scripted harness writes `verify` and its checks itself. A check is one
of three lines: `exit 0`, `exit 1`, or `test -f <path>`
(`core/src/testing.rs`).

### A. Keep the real `sh`

- Not possible from core (no processes).

### B. Results scripted per test

- Each test lists what `verify` answers, in order.
- The answers do not follow the files the harness wrote, so a test can
  pass while the run it describes is impossible.

### C. A fake that reads the checks the harness wrote

- `ScriptedVerify` lists `checks/*.sh` under the project, keeps those the
  arguments select (`--task T007` takes the `T007.` prefix), judges each of
  the three forms against the files, and answers in `verify`'s JSON.
- It follows the run as the real one does, for the scripts the scripted
  harness writes. Any other line is a test bug and panics.

## `Vcs`

### A. An in-process git library behind the port

- `gix` or `git2` in a new adapter would save the process starts, not the
  disk, and the tests would still live in adapters. It adds a large
  dependency for the tests' sake only.

### B. A fake that snapshots the files

- `SnapshotVcs` keeps each commit as a map from path to bytes of the files
  under the root, in memory. `commit_all` snapshots, `reset_hard` and
  `restore_paths` write a snapshot back, `changed_paths` and the
  fingerprint compare the files with HEAD, `diff` lists the changed paths
  with their new contents (the reviewer prompt only needs text, not a real
  patch, and core gains no diff dependency).
- It must ignore what git would: the lines Tenaz writes into
  `.tenaz/.gitignore` (`private/`, `run.json`, `journal.jsonl`, `skills/`)
  and simple patterns in a root `.gitignore`. Otherwise `reset_hard` would
  put back an old `run.json`.
- The risk is drift from git: a test that passes on the fake and fails on
  git.

### C. B, held to git by contract tests

- One set of tests of the `Vcs` contract, written once in
  `core::testing`, runs against both `SnapshotVcs` (in core) and `GitRepo`
  (in adapters). Where the fake and git differ, the contract fails.
- The contract covers only what the orchestrator uses, including which
  `.gitignore` patterns the fake understands.

## Decision

- `MemoryProbe` and `EngineMetricsSource`: fakes that replay readings.
- `VerifyRunner`: **C**, a fake that judges the scripted harness's checks.
- `Vcs`: **C**, a snapshot fake held to `GitRepo` by shared contract tests.

The scenario tests move to `tenaz-core` one at a time, each in its own
commit, starting with the shortest (`capacity_wait`, `stop_grace`).
`crates/adapters/tests` keeps one whole fake run on real `git` and `sh`, so
the adapters are still exercised together. The big
`the_fake_run_completes_with_every_recovery_path_exercised` is split along
the way (the scripted harness growing opt-in recoveries is a separate
change).

The clock stays `FastClock` here; a virtual clock is its own decision.

## Consequences

- Orchestrator tests run in core, with no `git` or `sh`. Expected: a fake
  run in well under a second instead of 5 to 12. Measure it on the first
  moved test before moving the rest.
- `cargo test` stops depending on the machine's `git` and `sh` for most
  orchestrator tests; the remaining adapter smoke run still needs both.
- Two more pieces of test code to keep honest: the `Vcs` contract and the
  check reader. A new check form in the scripted harness needs one in
  `ScriptedVerify`.
- The sampler and engine counters can be tested in a run for the first
  time.
