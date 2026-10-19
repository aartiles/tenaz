# ADR 0001: Language and runtime

Status: accepted, 2026-10-17.

## Context

Tenaz supervises a harness (fx, Pi or another ACP agent), proxies an
engine, serves MCP tools and keeps a state machine on disk. Three
constraints shape the choice:

- The harness must run as a subprocess: fx cannot be embedded, and the
  runner has to be able to kill, restart and outlive a session.
- The people it is for run llama.cpp, Ollama, LM Studio or similar. They
  should not need a language runtime to install the runner.
- The orchestration (phases, ledger, recovery ladder, lease) is a state
  machine whose mistakes cost hours of model time.

Both protocols the runner speaks have official SDKs in both candidate
languages:

| | Rust | TypeScript |
|---|---|---|
| ACP | `agent-client-protocol` 3.4.0 (Zed, 2026-10-16) | `@agentclientprotocol/sdk` |
| MCP | `rmcp` 3.5.1 (official, 2026-10-05) | `@modelcontextprotocol/sdk` |
| HTTP proxy | `hyper` 1.12 / `axum` 0.8 | `node:http` |
| Process supervision | `tokio::process` | `node:child_process` |
| NVIDIA (later) | `nvml-wrapper` | shell out to `nvidia-smi` |

## Options

### A. Rust

- One binary per platform; nothing else to install.
- The state machine is checked by the compiler: exhaustive matches, no
  implicit nulls, `serde` for the ledger and journal schemas.
- Cost: slower iteration than a scripting language.

### B. TypeScript on Node

- Faster iteration.
- Cost: distribution needs Node (or a Node single-executable build), which
  weakens "plug and play".

## Decision

**A, Rust.** The architecture is:

- a domain core with no I/O (task cards, scheduling, budgets, watchdog
  rules, recovery ladder, lease state), fully unit-tested;
- ports (harness, engine, media, memory probe, version control, verify
  runner, clock, notifier) as interfaces the core depends on;
- adapters per port, each with a conformance test against a fake or a
  recorded fixture;
- an end-to-end test that drives the whole loop with a scripted fake
  harness.

## Consequences

- Edition 2024, stable toolchain pinned in `rust-toolchain.toml`.
- A Cargo workspace with one crate per boundary (`docs/ARCHITECTURE.md`).
- `unsafe_code = "forbid"` and clippy `pedantic` as warnings in CI.
