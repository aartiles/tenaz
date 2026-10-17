# ADR 0001: Language and runtime

Status: accepted, 2026-10-17.

## Context

`docs/SINGLE-MODEL-RUNNER-HANDOFF.md` proposes Rust with the harness as a
subprocess, for three reasons: fx cannot be embedded from Rust or Zig
anyway, process isolation is wanted, and one static binary is the
distribution the audience needs.

Facts about the development machine on 2026-10-17:

- No Rust toolchain is installed (no `cargo`, `rustc`, `rustup`, nothing
  from Homebrew). Installing it is one `rustup` command, user-level and
  reversible with `rustup self uninstall`.
- Node 24.16 is installed. The code being ported (`task-loop.mjs`, its fake
  Pi and its self-test) is JavaScript, and the owner's other repositories
  are JavaScript and Python.
- The fx 0.0.14 binary used by the spike is gone with the scratchpad. It
  must be downloaded again from the official release for any ACP test.
- Pi and Sushi 1.2.4 are installed; Sushi is serving
  `Qwen3.8-Flash-Next-Sushi-2.6bpw` on port 12345.

Both protocols the runner speaks have official SDKs in both languages:

| | Rust | TypeScript |
|---|---|---|
| ACP | `agent-client-protocol` 3.4.0 (Zed, 2026-10-16) | `@agentclientprotocol/sdk` |
| MCP | `rmcp` 3.5.1 (official, 2026-10-05) | `@modelcontextprotocol/sdk` |
| HTTP proxy | `hyper` 1.12 / `axum` 0.8 | `node:http` |
| Process supervision | `tokio::process` | `node:child_process` |
| NVIDIA (later) | `nvml-wrapper` | shell out to `nvidia-smi` |

## Options

### A. Rust (the handoff's choice)

- One binary per platform; no runtime for users of llama.cpp, Ollama or LM
  Studio to install.
- The state machine (phases, ledger, recovery ladder, lease) is checked by
  the compiler: exhaustive matches, no implicit nulls, `serde` for the
  ledger and journal schemas.
- Cost: a full rewrite with the JavaScript as the specification, slower
  iteration, and a toolchain the owner does not have today.

### B. TypeScript on Node 24

- The port is a restructuring of code the owner already wrote and reads.
- Faster iteration; the fake-harness end-to-end test is a direct
  descendant of `run-fake.sh`.
- Cost: distribution needs Node (or a Node single-executable build), which
  weakens "plug and play" for the audience the handoff names.

## Decision

**A, Rust.** The owner confirmed the handoff's choice on 2026-10-17. The
reasons still hold, both protocols have first-party crates, and the
toolchain gap was a one-minute install (`rustup` 1.99.1, user-level, with
`--no-modify-path`; use `~/.cargo/bin` or `. ~/.cargo/env`). The
architecture is:

- a domain core with no I/O (task cards, scheduling, budgets, watchdog
  rules, recovery ladder, lease state), fully unit-tested;
- ports (harness, engine, media, memory probe, version control, verify
  runner, clock, notifier) as interfaces the core depends on;
- adapters per port, each with a conformance test against a fake or a
  recorded fixture;
- an end-to-end test that drives the whole loop with a scripted fake
  harness, as `run-fake.sh` does today.

## Consequences

- Edition 2024, stable toolchain pinned in `rust-toolchain.toml`.
- A Cargo workspace with one crate per boundary (`docs/ARCHITECTURE.md`).
- `unsafe_code = "forbid"` and clippy `pedantic` as warnings in CI.
- The JavaScript in `docs/reference/` is the specification of the port;
  every behaviour listed in the handoff gets a test before it is marked
  carried over.
