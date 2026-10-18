# Contributing

Thanks for looking. Issues and pull requests are welcome.

## Before you open a pull request

Run the same three commands CI runs; all three must pass:

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
`Cargo.toml`), so fix the lint rather than allowing it unless the allow is
justified in a comment. The CLI end-to-end test takes about two minutes. The
macOS memory probe test is ignored on other platforms.

## Crate map

| Crate | Path | What it holds |
|---|---|---|
| `smr-core` | `crates/core` | Domain, ports and orchestration. No process, network or git code. |
| `smr-adapters` | `crates/adapters` | git, the verify runner, macOS memory probe, notifier, engine adapters (generic, Sushi), ComfyUI, Prometheus metrics. |
| `smr-proxy` | `crates/proxy` | OpenAI-compatible reverse proxy: per-request usage, hold while swapping, error classification. |
| `smr-acp` | `crates/acp` | Harness adapter over ACP (fx and any ACP agent). |
| `smr-pi` | `crates/pi` | Harness adapter over Pi's `--mode rpc`. |
| `smr-mcp` | `crates/mcp` | MCP server exposing media tools that take the lease. |
| `smr` | `crates/cli` | The `smr` binary: `init`, `run`, `status`, `params`. |

[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) has the details. New logic
belongs in `smr-core` when it can be pure; side effects belong in an adapter
behind a port.

## Tests need no model

The test suite must run on a laptop or a CI runner with no model, engine,
fx, Pi or ComfyUI installed. A scripted harness drives every orchestrator
path, fake engines sit under the lease, and fakes stand in for the ACP agent,
the Pi process, the engine behind the proxy and ComfyUI. Keep it that way:
a new adapter comes with a fake or a recorded transcript, and pure
translation code gets unit tests. Checks against real tools are documented in
`docs/` (see [`docs/STATUS.md`](docs/STATUS.md)) rather than added to
`cargo test`.

## License

By contributing you agree that your contributions are licensed under
MIT OR Apache-2.0, as the rest of the project.
