# Single-model runner: design handoff

Status: research handoff, 2026-10-17. Nothing here is built yet. The design
starts from scratch in a separate repository. This document collects the goal,
what to carry over from `scripts/task-loop.mjs`, the market survey, a proposed
architecture, and the measurements from the fx and Sushi spike.

## Goal

An open-source runner for people whose hardware holds one model at a time
(a Mac with 16–64 GB of unified memory, or one consumer GPU). Given a goal,
it plans, splits the work into tasks, and drives an agent harness until the
goal is met. Throughout, it guarantees:

- **One resident model.** The runner owns the memory. Only one engine is
  loaded at any moment: the LLM, an image model, a TTS model, and so on.
- **Media through the same lease.** The agent can call tools that need
  another engine (ComfyUI for image or video, TTS, STT). The runner unloads
  the LLM, runs the job, unloads that engine, reloads the LLM, and returns the
  result to the agent.
- **Deterministic progress.** The plan, the task state, retries, and
  blocking decisions belong to code, not to the model. The run survives
  engine failures and memory pressure and continues to the end.
- **Plug and play.** Start with Pi and Sushi on macOS. Leave room for other
  harnesses, engines, media back ends, and Linux with NVIDIA.

## What to carry over from the task loop

The source is `scripts/task-loop.mjs` and its design is
`docs/TASK-LOOP-ORCHESTRATOR.md` in this repository. Its value is the
behaviour on constrained hardware, more than the planner itself:

- **Fresh sessions per unit of work.** State lives on disk (`.agent/`:
  spec, task cards, notes, `run.json` ledger, `journal.jsonl`). Quality is
  carried by the handover between sessions, not by long contexts.
- **Roles.** Bootstrap, builder, reviewer, split, replan and final reviewer,
  all the same model, run one after another. Every role prompt is generic.
- **A fixed `verify` contract** that the runner executes itself, then
  commits or reverts.
- **Watchdogs and a recovery ladder.** Loop, repeat, idle and large-output
  detection. Steer, then abort, then split, then block. Blocking is the
  runner's decision.
- **Engine failures are not task failures.** A session that ends with an
  engine error is rerun without charging an attempt, with backoff
  (`engineRetries` 6, base 60 s, doubling to 15 minutes).
- **Capacity cap.** When the engine reports a lower maximum context, the run's
  context budget drops to 0.8 of it and the session reruns at once
  (`capacityHeadroom`, `capacityRetries` 4). A reported maximum below
  `contextFloor` (32768) means something else holds the memory: wait instead.
- **Continuations.** A session stopped at 1.3 times its context budget, or
  after the engine refuses an image it read, is continued by a fresh session
  told to start from `git status`, the diff and the task notes. Up to 3 times,
  uncounted.
- **Memory sampling.** On macOS: `kern.memorystatus_vm_pressure_level`,
  `vm.swapusage`, `vm_stat` swap-ins, and the server footprint, every 30 s,
  per session.
- **Pinned skills** copied into the project read-only and hashed, and **file
  ownership** per role.
- **Pause and resume** (SIGINT/SIGTERM), and a preflight that checks reasoning
  output and prefix caching before the first session.

## Market survey (2026-10)

| Piece | Existing tools | Gap |
|---|---|---|
| Swapping models | [llama-swap](https://github.com/mostlygeek/llama-swap/wiki/Configuration) (one model at a time by default, groups for coexistence); llama.cpp router mode with `--models-max 1` and per-model unload; LocalAI `--max-active-backends=1` (LRU eviction, idle and busy watchdogs); Ollama `keep_alive`; LM Studio JIT loading | Request-driven proxies. They know nothing of a goal, of tasks, or of memory pressure. |
| Freeing the GPU for image work | ComfyUI nodes that unload [Ollama](https://github.com/bradsec/ComfyUI_OllamaUnload), llama.cpp or LM Studio models before sampling | Inverted control: the image workflow drives, not the agent. |
| Loops to a goal | [pi-ralph](https://cdn.jsdelivr.net/npm/pi-ralph@1.0.1/README.md) (planner/builder/reviewer "hats" as a Pi extension), ralph-playbook, the fresh-context loop pattern | They assume the engine is always available, and count an engine failure as a task failure. |
| Harnesses | Pi (RPC mode, extensions with `pi.on()` hooks), fx (ACP), and others that speak ACP | An in-session extension cannot supervise, kill or outlive its session. |
| Pi over ACP | [DxTa/pi-acp](https://github.com/DxTa/pi-acp), [victor-software-house/pi-acp](https://github.com/victor-software-house/pi-acp), [pi-acpinator](https://docs.rs/pi-acpinator/0.1.0) (Rust) | Young projects. DxTa's accepts MCP servers in ACP but does not wire them to Pi. |

No surveyed project combines a deterministic goal loop, an exclusive resource
lease across modalities, and recovery from memory failures.

## Proposed architecture

```text
               ┌──────────────────────── runner (Rust, one binary) ────────────────────────┐
  goal ──▶     │  loop: plan → task → session → verify → review → commit/revert → replan    │
               │  ledger + journal (.agent/)        watchdogs (from harness + proxy events) │
               │                                                                             │
               │  lease manager ◀── MemoryProbe (macOS: sysctl/vm_stat; Linux: PSI, NVML)    │
               │     │                                                                       │
               │     ├── EngineAdapter: LLM (Sushi, llama-swap, llama.cpp router, Ollama, …) │
               │     └── MediaAdapter: ComfyUI, TTS, STT (start, run job, free)              │
               │                                                                             │
               │  engine proxy (OpenAI-compatible) ── per-request usage, holds while swapping│
               │  MCP server (media tools) ── each call takes the lease                      │
               └────────────┬───────────────────────────────┬────────────────────────────────┘
                            │ ACP or Pi RPC (stdio)         │ HTTP via proxy
                      harness process (fx, Pi, …) ─────────▶ LLM engine
```

### Components

- **Loop.** The task loop above, generalised: no Pi-specific types in the
  core.
- **HarnessAdapter.** Starts a session, sends the prompt, streams events
  (tool calls, results, text, end of turn, usage), steers, cancels, and
  answers permission requests. Two implementations first:
  - **ACP client** for fx and any other ACP agent.
  - **Pi RPC**, as `task-loop.mjs` does today, because the Pi ACP adapters do
    not yet carry MCP.
- **Engine proxy.** A small OpenAI-compatible reverse proxy between the
  harness and the LLM engine. It is central because ACP reports usage only once
  per turn (see the measurements). The proxy:
  1. Records every request's prompt, cached and output tokens, independent of
     the harness. This feeds the context-budget watchdog mid-turn, which ACP
     alone cannot do.
  2. Holds requests while the lease has the LLM unloaded, so the harness never
     sees a refused connection.
  3. Is the one place where engine errors (capacity messages, image refusals)
     are classified.
- **Lease manager.** One holder at a time. It drives adapters through
  `unload → load → ready` and records each transition with its timing and
  memory sample.
- **EngineAdapter.** `start`, `stop`, `load`, `unload`, `health`, `models`,
  `capacity` and `metrics`. Implement first a generic one (start and stop
  commands plus an OpenAI-compatible URL) and Sushi (which has
  `/v1/load-model` and `/v1/unload-model`). Then llama-swap, the llama.cpp
  router, Ollama, LM Studio and LocalAI.
- **MediaAdapter.** ComfyUI first: queue a workflow through `/prompt`, poll
  `/history`, then free its models before the LLM reloads. Then TTS and STT.
- **MCP server.** Exposes media tools to the harness session. A tool call
  takes the lease, runs the job through a MediaAdapter, and returns the output
  path.
- **MemoryProbe.** macOS as today. Linux: `/proc/pressure/memory` and NVML or
  `nvidia-smi`.

### The lease cycle for a media tool

1. The agent calls a media tool. With fx in `ask` mode, the runner receives
   `session/request_permission` first and can refuse, queue or approve.
2. The MCP server asks the lease manager for the image engine.
3. The proxy starts holding LLM requests. The LLM engine unloads.
4. The media engine loads, runs the job, writes the output, and unloads.
5. The LLM engine reloads and reports ready. The proxy releases its hold.
6. The tool returns the output path. The session continues.

Policy: **continue the same session** when the engine restores the prefix from
disk (Sushi does, see below). Otherwise the next request re-prefills the
whole context, so for long contexts on engines without a disk cache, the
alternative is to end the session and continue in a fresh one with the
existing continuation note. Make this a per-engine setting.

### Language and harness embedding

**Rust, with the harness as a subprocess.** Reasons:

- libfx, fx's embeddable core, ships only as a Node addon and as WebAssembly.
  There is no C ABI and no public Zig module (`build.zig.zon` version 0.0.0,
  no `addModule`). Embedding fx in-process from Rust or Zig would mean vendoring
  internal source of an experimental project.
- Process isolation is a feature. The supervisor must survive a harness crash
  and kill a stuck session.
- Harness footprint is minor next to the model. Distribution as one binary is
  the real benefit, and Rust gives that with a mature ecosystem: tokio, an ACP
  crate, MCP crates, NVML bindings.

fx is a good default harness for this runner: small system prompt, ACP,
durable sessions, local OpenAI-compatible providers, Apache-2.0. It is
experimental (v0.0.14), so pin its version.

## Spike measurements (2026-10-17)

All runs used fx 0.0.14 (build revision `1a070af32cdf`) from the official
macOS arm64 release, run with an isolated `HOME`. The ACP client, MCP servers
and fake engine were throwaway Node scripts in the session scratchpad (not
kept). Describe them again in the new repository if needed: about 50 lines
each.

### fx over ACP, against a fake OpenAI engine

- **Initialize.** `agentCapabilities`: `loadSession`; prompt `image` yes,
  `audio` no, `embeddedContext` yes; MCP over `http`, `sse` and `acp`;
  sessions `list`, `resume`, `close` and `systemPrompt`;
  `_meta.fx.steering`.
- **Tool calls are visible.** `session/update` sends `tool_call` (name such
  as `mcp_lease_slow_render`, `rawInput`, `_meta.fx.toolCall.mcp.server` and
  `.tool`), then `tool_call_update` with `in_progress`, then `completed` or
  `failed` and the result content.
- **Permission gate.** In `ask` mode, the client receives
  `session/request_permission` (options `allow_once`, `allow_always`, …)
  before execution. This is the runner's control point.
- **Usage is per turn only.** One `usage_update` (`used`, `size`) arrives at
  the end of the turn, even when the turn made several requests. `used` is the
  last request's input plus output. The `session/prompt` response carries
  `inputTokens` and `outputTokens` summed over the turn. There is no per-request
  usage over ACP, hence the engine proxy.
- **Request size.** 15 built-in tools, plus one per MCP tool offered. The system prompt is about 8.4–8.7 KB
  and the first request body about 32 KB. Against Sushi this was about 7.7k
  prompt tokens (`sys=8334b`, `tools=23086b`).
- **MCP servers passed in ACP `session/new`.** The tool is offered directly.
  A call is cut at **60 s** (`McpRequestTimedOut`; the server receives
  `notifications/cancelled` at 60.0 s). The 75 s call failed with and without
  progress notifications. Progress every 10 s was relayed to the client as
  `tool_call_update` text ("MCP progress 10.00/75.00") but did not extend the
  deadline.
- **MCP servers from the profile.** `~/.fx/mcp.json`:
  `{"mcp":{"lease":{"type":"stdio","command":["/abs/node","/abs/server.mjs"],"enabled":true,"operation_timeout_ms":600000,"env":{…}}}}`.
  In ACP mode these load only when `session/new` carries
  `"_meta":{"fx":{"profileMcpServers":true}}`. Their tools are lazy: the
  model must call `capability_search` first. With that, a **180 s call
  completed**. The default operation timeout is 60 s and the default startup
  timeout 30 s (`src/core/mcp/mcp_contract.zig`). The spike also ran
  `fx mcp trust approve lease`. Whether that is required for profile servers
  was not isolated.
- **Engine unreachable.** The fake engine stopped listening right after
  returning a tool call. fx retried silently and finished the turn about 4 s
  after the engine returned, for outages of **20 s, 120 s and 300 s** (turns of
  24.2 s, 124.2 s and 304.4 s). No ACP event is sent while it waits.
- **Configuration.** `~/.fx/settings.json`:
  `{"providers":{"sushi":{"protocol":"openai-chat-completions","base_url":"http://127.0.0.1:12345/v1","auth":{"type":"none"},"model_metadata":{"<model id>":{"context_window":131072,"max_output_tokens":16384,"supports_tool_use":true,"supports_vision":true}}}},"provider":"sushi","models":{"sushi":"<model id>"},"permission_mode":"ask"}`.
  `http` base URLs are accepted only for localhost.

### fx with Sushi and the real model

Engine: Sushi 1.2.4 (MLX 0.32.3), `Qwen3.8-Flash-Next-Sushi-2.6bpw`, 44.20 GB
resident, KV8, MTP on, 131072 context, prefix cache on SSD (20 GB, 1 entry
configured), on a 64 GB Mac. Log:
`~/qwen38-ds4-validation/validation-logs/raycast-sushi-128k/server-20261017T142330Z-31139.log`.
The spike's requests are about 18:25Z–18:29Z on 2026-10-17. Exclude them
from any Pi/Sushi matching.

Every fx request had `thinking=false`: fx did not ask for reasoning with
effort left at `auto`. Set the effort explicitly when that matters.

| Run | Requests | Wall time | Input / output tokens | Notes |
|---|---|---|---|---|
| Coding task, `ask` (write `fib.py` and tests, run them) | 3 | 21.1 s | 24,223 / 555 | Correct tool use. Tests pass. Prefix restored from SSD: 7,699/8,060 and 8,029/8,433 tokens. |
| Same task, `auto` | 14 (10 main + 4 reviewer) | 40.6 s | 77,113 / 1,280 | All 4 reviewer checks ended "Safety reviewer unavailable; action held". Every `shell` call was blocked, and the model worked around it. Reviewer requests: `max_tokens=2048`, 1.6–2.4k tokens, only 512 cached. |
| Lease cycle in a live session | 11 | 56.4 s | 107,983 / 1,344 | See below. |

**Conclusion on permissions:** use `ask` and let the runner approve. fx's
`auto` mode sends an extra model call per tool call, and this model does not
satisfy the reviewer's contract.

**Engine swap on Sushi:**

- `POST /v1/unload-model {"model": id}`: 3.58 s the first time (the log says
  it waited 3000 ms for the memory to unwire), 0.52 s inside the lease cycle.
- `POST /v1/load-model {"model": id}`: returns when ready. 7.5 s and 7.67 s,
  with the weights still in the OS page cache.
- The SSD prefix cache survives an unload and reload (the reload scanned 9,
  then 13, persisted entries).
- **Lease cycle in a live fx session.** An MCP tool unloaded the LLM, waited
  10 s in place of a ComfyUI job, reloaded it, and returned a path. Unload
  0.522 s, job 10.001 s, reload 7.666 s, total 18.189 s. The first request
  after the reload restored **7,753 of 7,967** prompt tokens from SSD in
  49 ms. The re-prefill cost of the swap was negligible. (The model then spent
  about 9 requests looking for the image file, which the fake job never wrote.)

**Not measured:**

- A truly cold load, after a media job has pushed the 44 GB of weights out of
  the page cache. Expect it to be bound by SSD read speed. Measure it with a
  real ComfyUI job.
- Re-prefill cost on engines without a disk prefix cache (llama.cpp, Ollama).
- A media tool longer than 60 s over ACP-passed MCP (needs the profile route
  or another transport).

## MVP scope

1. Runner core: the loop, ledger, journal, watchdogs and recovery ladder,
   ported from `task-loop.mjs`.
2. HarnessAdapter: ACP client (fx), then Pi RPC.
3. Engine proxy with per-request usage and hold-while-swapping.
4. EngineAdapter: generic commands plus Sushi.
5. MediaAdapter: ComfyUI image generation through the lease, exposed as an
   MCP tool.
6. MemoryProbe: macOS.
7. One end-to-end example: a goal that needs code and one generated image.

Out of scope: writing another engine manager (wrap llama-swap and the
llama.cpp router instead), a GUI, and remote or multi-GPU scheduling.

## Open questions

- **How to give the harness a long media tool.** Options: profile MCP with a
  long `operation_timeout_ms` (fx-specific `_meta`), MCP over HTTP or SSE
  served by the runner, or an ACP-level tool provided by the client. Test
  which one each harness supports.
- **Same session or fresh session after a swap,** per engine. Measure the
  re-prefill on llama.cpp and Ollama.
- **Where permission decisions live:** a runner policy (allowlist per role) or
  the model.
- **Which watchdogs move to the proxy** (context, latency, refusals) and which
  stay on harness events (loops, idle).
- **Pi's future.** Native RPC adapter only, or also a thin Pi extension that
  talks to the runner's MCP or socket for media tools.
- **Linux and NVIDIA probe** semantics. What replaces the macOS pressure level
  and swap-in signal.

## Risks

- **fx is experimental** (v0.0.14, "expect frequent changes"). Pin a version
  and keep the ACP adapter thin.
- **Adapters cost maintenance.** Engines change fast (llama-swap ships
  releases near v198). Each adapter needs an owner and a conformance test.
- **Pi's RPC protocol** can change between versions. `task-loop.mjs` already
  records `pi --version` per run.
- **The audience is a niche**, but a real one: one-model hardware, long
  unattended runs.

## Publication

The new repository is separate from this one. Do not publish, push to a public
remote or release anything without the user's explicit approval for that
specific publication (see `AGENTS.md` here). Approval for one release does not
cover later updates.
