# Contributing

Thanks for looking. Issues and pull requests are welcome.

## Before you open a pull request

Enable the repository's git hooks once; `pre-commit` checks formatting and
the crate boundaries (`scripts/check-architecture.sh`), `pre-push` runs
everything CI runs:

```bash
git config core.hooksPath .githooks
```

Or run the same commands CI runs by hand; all must pass:

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

Clippy runs with the `pedantic` group enabled (see `[workspace.lints]` in
`Cargo.toml`), so fix the lint rather than silencing it. An exception has to
be `#[expect(lint, reason = "...")]`: clippy rejects `#[allow]` and an
expectation without a reason, and fails when an expected lint no longer
fires. The CLI end-to-end test takes about two minutes. The
macOS memory probe test is ignored on other platforms.

## Crate map

| Crate | Path | What it holds |
|---|---|---|
| `tenaz-core` | `crates/core` | Domain, ports and orchestration. No process, network or git code. |
| `tenaz-adapters` | `crates/adapters` | git, the verify runner, macOS memory probe, notifier, engine adapters (generic, load API), media adapters (ComfyUI, OpenAI Images), Prometheus metrics. |
| `tenaz-proxy` | `crates/proxy` | OpenAI-compatible reverse proxy: per-request usage, hold while swapping, error classification, tool-argument repair, lowered effort, replies at the output limit reported as cut. |
| `tenaz-acp` | `crates/acp` | Harness adapter over ACP (fx and any ACP agent). |
| `tenaz-pi` | `crates/pi` | Harness adapter over Pi's `--mode rpc`. |
| `tenaz-mcp` | `crates/mcp` | MCP server exposing media tools that take the lease. |
| `tenaz` | `crates/cli` | The `tenaz` binary: one module per command under `src/commands/` (`init`, `run`, `status`, `watch`, `finish`, `demo`, `doctor`; `info` for `params` and `guide`, `shims` for the hidden `fx-shim` and `sandbox-exec`); `main.rs` parses and dispatches. |

[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) has the details. New logic
belongs in `tenaz-core` when it can be pure; side effects belong in an adapter
behind a port.

## Tests need no model

The test suite must run on a laptop or a CI runner with no model, engine,
fx, Pi or ComfyUI installed. A scripted harness drives every orchestrator
path, fake engines sit under the lease, and fakes stand in for the ACP agent,
the Pi process, the engine behind the proxy and ComfyUI. Keep it that way:
a new adapter comes with a fake or a recorded transcript, and pure
translation code gets unit tests. Nor may a test depend on what runs on the
machine: tests that start `tenaz` set `TENAZ_NO_PROBE`, so `init` and
`doctor` do not find a real engine or ComfyUI on the usual ports. Checks
against real tools are documented in
[`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) rather than added to
`cargo test`.

A test that writes a script and then runs it, in a test binary whose other
threads may fork meanwhile, writes it with
`tenaz_adapters::testing::write_executable` (feature `testing`), not
`std::fs::write`. On Linux the exec fails now and then with "Text file busy"
when another thread forks while the script is open for writing. A binary
with a single test (`crates/acp/tests/subprocess_exit.rs`) does not need it.
The scripted harness in `tenaz-core`, which may not start processes, still
writes its `verify` in-process.

## License

By contributing you agree that your contributions are licensed under
MIT OR Apache-2.0, as the rest of the project.
