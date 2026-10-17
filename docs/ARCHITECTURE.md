# Architecture

The runner is a Cargo workspace. Dependencies point inwards: adapters depend
on the core, the core depends on nothing of theirs.

```text
crates/
  core/      smr-core      domain + ports + orchestration. No process, network or git code.
  adapters/  smr-adapters  git, verify runner, macOS memory probe, notifier, engine adapters
                           (generic commands, Sushi), ComfyUI media adapter, Prometheus metrics.
  proxy/     smr-proxy     OpenAI-compatible reverse proxy: per-request usage, hold while swapping,
                           engine error classification.
  acp/       smr-acp       HarnessAdapter over ACP (fx and any ACP agent), harness as a subprocess.
  mcp/       smr-mcp       MCP server (streamable HTTP) exposing media tools that take the lease.
  cli/       smr           the `smr` binary: init, run, status, preflight.
```

## Core (`smr-core`)

Pure modules, each unit-tested without I/O:

| Module | Responsibility (ported from `docs/reference/task-loop/task-loop.mjs`) |
|---|---|
| `params` | Run parameters, defaults and `key=value` overrides pinned in `run.json`. |
| `task` | Task IDs (`T007`, `T007a`), statuses, card frontmatter parse and field updates. |
| `spec` | Requirement lines (`- R1: ...`, `manual:`) and coverage. |
| `backlog` | Card validation, dependency DAG, resolved and ready tasks. |
| `verify` | The `verify` contract: last-line JSON, exit code, clipping. |
| `watchdog` | Loop, repeat, idle, big-output, context, turn, compaction and provider-error rules; emits steer or abort. |
| `session` | Harness-neutral session events, usage, metrics, outcome classification (engine failure, capacity limit, image refusal). |
| `briefing` | Prompt packing: role prompt, spec, map, card, notes, continuation notes. |
| `ownership` | Which paths each role may write; the rest is reverted. |
| `ledger` | `run.json` and the `journal.jsonl` record types. |
| `metrics` | Engine Prometheus counters, per-request deltas, engine summary. |
| `memory` | Memory samples and per-session windows. |
| `lease` | One resident engine: the lease manager state machine over the engine and media ports. |

`ports` declares the traits the orchestration needs: `Harness` and
`Session`, `Vcs`, `VerifyRunner`, `Clock`, `Notifier`, `MemoryProbe`,
`EngineAdapter`, `MediaAdapter`, `EngineMetricsSource`. `lease::Gate` is
what the proxy awaits while the LLM is away. `testing` (feature `testing`)
holds the scripted harness that plays `fake-pi.mjs`'s script and a fast
clock, for the crates above the core.
`orchestrator` is the application service: phases (bootstrap, loop, final),
the session wrapper with engine retries, capacity caps and continuations,
the task attempt loop with review, split and block, and pause and resume.

State lives on disk under `.agent/` in the project, exactly as the task loop
lays it out, so a run survives the process and the user's existing tooling
(`jq` over the journal, `git log`) keeps working.

## Testing

- Unit tests next to each core module.
- Adapter conformance tests against fakes or a temporary git repository.
- One end-to-end test drives the whole loop with a scripted fake harness
  and a real temporary git repository, the descendant of `run-fake.sh`.
- The `smr` binary runs the fake harness end to end through the real
  wiring (proxy, git, verify) in `crates/cli/tests/cli.rs`.
- Nothing in `cargo test` needs fx, Sushi or ComfyUI; checks against the
  real ones are listed in `STATUS.md`.

## Conventions

- Edition 2024, `unsafe_code = "forbid"`, clippy `pedantic` warnings clean.
- Errors: `thiserror` enums per boundary; `anyhow` only in the binary.
- Async: tokio. Ports that wrap blocking tools (git, verify) are synchronous
  and short; the orchestrator calls them directly.
- Field names in `run.json` and the journal keep the task loop's names.
