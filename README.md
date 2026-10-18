# Single-model runner (`smr`)

A runner for hardware that holds one model at a time: a Mac with 16–64 GB of
unified memory, or one consumer GPU. Given a goal, it plans, splits the work
into tasks, and drives an agent harness session by session until the goal is
met, while only one engine (the LLM, an image model, ...) is resident at any
moment.

It is the port of the task loop in
[`docs/reference/task-loop/`](docs/reference/task-loop/) to Rust, generalised
over harnesses and engines, plus the pieces the handoff asked for: an engine
proxy, a lease over the engines, and media tools over MCP. See
[`docs/SINGLE-MODEL-RUNNER-HANDOFF.md`](docs/SINGLE-MODEL-RUNNER-HANDOFF.md)
for the design and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the
crates. [`docs/STATUS.md`](docs/STATUS.md) says what is verified and what is
open.

## What it guarantees

- **One resident model.** The lease manager owns the memory. A media tool
  unloads the LLM, runs the job, frees the media engine and reloads the LLM.
- **Media through the same lease.** The session calls `generate_image` over
  MCP; the proxy holds its LLM requests while the LLM is away, so the harness
  never sees a refused connection.
- **Deterministic progress.** The plan, task state, retries and blocking
  decisions are the runner's. Engine failures are retried without charging
  the task. A run survives the process: pause with Ctrl-C, run again.
- **Plug and play.** fx over ACP and Pi over its RPC mode as harnesses;
  Sushi and any OpenAI-compatible engine with start/stop commands; ComfyUI
  for images.

## Build

Requires a stable Rust toolchain (`rustup`; the ADR pins why Rust).

```bash
cargo build --release
```

```bash
cargo test --workspace
```

The tests need no model, engine, fx or Pi: a scripted harness exercises every
orchestrator path, fake engines sit under the lease, and fakes stand in for
the ACP agent, the Pi process, the engine behind the proxy, and ComfyUI.

## Use

```bash
smr init --project runs/my-goal --brief brief.md \
  --model Qwen3.8-Flash-Next-Sushi-2.6bpw --provider sushi \
  --base-url http://127.0.0.1:12345/v1 --context-window 131072 --vision
```

```bash
smr run --project runs/my-goal --fx-bin /path/to/fx \
  --comfyui http://127.0.0.1:8188 --workflow default=workflows/txt2img.json
```

```bash
smr status --project runs/my-goal
```

- `init` copies the brief to `BRIEF.md`, the shared rules to `AGENTS.md`, the
  role prompts to `.agent/prompts/`, pins the model, harness version and
  parameters in `.agent/run.json`, and makes the first commit.
- `run` checks the pins, probes the engine (reasoning, prefix cache, vision),
  then loops: bootstrap, build, verify, review, split or block, replan, final
  review. Ctrl-C pauses after the current session; `run` again continues.
  `--resume` continues a BLOCKED run. `--set key=value` at `init` overrides
  a parameter; `smr params` lists them.
- `status` reads files only and is safe while a run works.
- `--harness pi` at `init` pins `pi --version` and reads the model's base
  URL, context window and inputs from `~/.pi/agent/models.json`, as the task
  loop did; `run` then spawns `pi --mode rpc` per session (`--pi-bin` names
  the executable). Pi talks to the engine directly, so `--no-proxy` is fine
  there.
- `--harness fake` at `init` runs the scripted harness end to end without a
  model (what the tests do).

State lives under `.agent/` in the project exactly as the task loop laid it
out: `spec.md`, `map.md`, `tasks/`, `notes/`, `reviews/`, the `run.json`
ledger, the `journal.jsonl` facts, and `private/` for raw session streams,
prompts, per-request engine timings and memory samples.

## Publication

Do not publish, push to a public remote or release anything from this
repository without the owner's explicit approval for that specific
publication.
