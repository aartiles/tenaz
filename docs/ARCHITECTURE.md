# Architecture

The runner is a Cargo workspace. Dependencies point inwards: adapters depend
on the core, the core depends on nothing of theirs.

```text
crates/
  core/      tenaz-core      domain + ports + orchestration. No process, network or git code.
  adapters/  tenaz-adapters  git, verify runner, macOS memory probe, notifier, engine adapters
                           (generic commands, a load API as Sushi's), media adapters (ComfyUI, OpenAI Images),
                           Prometheus metrics.
  proxy/     tenaz-proxy     OpenAI-compatible reverse proxy: per-request usage, hold while swapping,
                           engine error classification, tool-argument repair, the lowered
                           reasoning effort, and replies at the output limit reported as cut.
  acp/       tenaz-acp       HarnessAdapter over ACP (fx and any ACP agent), harness as a subprocess.
  pi/        tenaz-pi        HarnessAdapter over Pi's `--mode rpc` JSON lines, one Pi process per session.
  mcp/       tenaz-mcp       MCP server (streamable HTTP) exposing media tools that take the lease.
  cli/       tenaz           the `tenaz` binary: init (flags or the setup wizard), run, status,
                           finish, guide (prints the agent skill in skills/tenaz/SKILL.md).
```

## Core (`tenaz-core`)

Pure modules, each unit-tested without I/O:

| Module | Responsibility |
|---|---|
| `params` | Run parameters, defaults and `key=value` overrides pinned in `run.json`. |
| `task` | Task IDs (`T007`, `T007a`), statuses, card frontmatter parse and field updates. |
| `spec` | Requirement lines (`- R1: ...`, `manual:`) and coverage. |
| `backlog` | Card validation, dependency DAG, resolved and ready tasks. |
| `verify` | The `verify` contract: last-line JSON, exit code, clipping. |
| `watchdog` | Loop, repeat, idle, big-output, context, turn, compaction and provider-error rules; emits steer or abort. |
| `session` | Harness-neutral session events, usage, metrics, outcome classification (engine failure, capacity limit, image refusal). |
| `briefing` | Prompt packing: role prompt (for the bootstrap, its step's part), spec, map, card, notes, continuation notes. |
| `role` | The session roles, their prompt paths and what a steered session is asked to write before it stops. |
| `effort` | The dial that lowers the reasoning effort for one session after a reply cut at the output limit. |
| `ownership` | Which paths each role may write; the rest is reverted. |
| `ledger` | `run.json` and the `journal.jsonl` record types. |
| `metrics` | Engine Prometheus counters, per-request deltas, engine summary. |
| `memory` | Memory samples and per-session windows. |
| `lease` | One resident engine: the lease manager state machine over the engine and media ports. |

`ports` declares the traits the orchestration needs: `Harness` and
`Session`, `Vcs`, `VerifyRunner`, `Clock`, `Notifier`, `MemoryProbe`,
`EngineAdapter`, `MediaAdapter`, `EngineMetricsSource`. `lease::Gate` is
what the proxy awaits while the LLM is away. `testing` (feature `testing`)
holds the scripted harness that plays a fixed session script and a fast
clock, for the crates above the core.
`orchestrator` is the application service: phases (bootstrap in three
steps, see ADR 0003; loop; final),
the session wrapper with engine retries, capacity caps and continuations,
the task attempt loop with review, split and block, and pause and resume.

State lives on disk under `.tenaz/` in the project, so a run survives the
process and the user's existing tooling (`jq` over the journal, `git log`)
keeps working.

## Harness adapters

Both adapters follow one shape: the harness process runs behind a background
task, commands go in, `SessionEvent`s come out, and a pure `translate`
module with unit tests turns the harness's own records into those events.

| | `tenaz-acp` (fx) | `tenaz-pi` (Pi) |
|---|---|---|
| Transport | ACP over the agent's stdio, via `agent-client-protocol` | Pi's RPC: one JSON object per LF-terminated line on stdin and stdout |
| Per session | `initialize`, `session/new` with the runner's MCP servers | `pi --mode rpc --session-dir DIR|--no-session --model P/ID --thinking L --tools read,bash,write,edit,grep,find,ls --no-mcp --no-extensions --no-skills [--skill DIR...] --approve --offline` |
| Prompt | `session/prompt` | `{"id":"prompt-N","type":"prompt","message":...}`; a `response` that is not `success` or whose disposition is `handled` becomes `PromptRejected` |
| Steer | fx's extension: a `session/prompt` with `_meta.fx.steer: true` while a prompt runs; otherwise dropped | `{"type":"steer"}`: Pi delivers it after the current turn's tool calls |
| Abort | `session/cancel` | `clear_queue` then `abort`; `agent_settled {aborted:true}` follows |
| Usage per request | from the engine proxy's observation feed | from Pi's `message_end.message.usage` (input, output, cacheRead, cacheWrite) |
| Permissions | `request_permission`, answered by the runner | none: Pi's built-in tools run unasked in RPC mode |
| Raw log | the translated events, one JSON line each; the reasoning text goes to `<label>.thinking.md` | Pi's own stdout lines, verbatim |
| Close | end the connection, wait, kill after the grace | end stdin (Pi's orderly shutdown), wait, kill after the grace |
| Engine path | through the proxy (usage, hold while swapping) | direct: Pi reads the provider's `baseUrl` from `~/.pi/agent/models.json`; the proxy is not on its path |

`tenaz init --harness pi` pins `pi --version` and, when `~/.pi/agent/models.json`
lists the provider, takes the base URL, context window and inputs from the
model's entry (a provider that does not list the model is an error). The `fake-pi` binary in `tenaz-pi` is a scripted Pi that speaks the
protocol without a model: the adapter's integration tests spawn it.

## Testing

- Unit tests next to each core module.
- Adapter conformance tests against fakes or a temporary git repository.
- One end-to-end test drives the whole loop with a scripted fake harness
  and a real temporary git repository.
- The `tenaz` binary runs the fake harness end to end through the real
  wiring (proxy, git, verify) in `crates/cli/tests/cli.rs`.
- The Pi adapter runs against the scripted `fake-pi` binary: a tool turn,
  several prompts in one session, a loop that is aborted, a steer, an engine
  error with a capacity message, a refused prompt, the stderr log, a close
  that has to kill. `pi --version` is checked when Pi is installed.
- Nothing in `cargo test` needs fx, Pi, Sushi or ComfyUI; checks against the
  real ones are listed in `COMPATIBILITY.md`.

## Conventions

- Edition 2024, `unsafe_code = "forbid"`, clippy `pedantic` warnings clean.
- Errors: `thiserror` enums per boundary; `anyhow` only in the binary.
- Async: tokio. Ports that wrap blocking tools (git, verify) are synchronous
  and short; the orchestrator calls them directly.
- Field names in `run.json` and the journal are kept stable across
  versions, so scripts that read them keep working.
