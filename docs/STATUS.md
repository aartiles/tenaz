# Status

As of 2026-10-17. Everything below is in the repository; nothing is
committed yet.

## Built and verified by tests

| Piece | Where | Verified by |
|---|---|---|
| Task cards, spec, backlog, coverage, `verify` contract | `smr-core` | unit tests |
| Watchdog rules (loop, repeat, idle, big output, context, turns, compaction, provider errors) | `smr-core::watchdog` | unit tests with the task loop's thresholds |
| Session wrapper: engine retries with backoff, capacity cap, context and image continuations, pause | `smr-core::orchestrator::session` | the end-to-end fake run |
| Phases: bootstrap validation, attempts, review with retry, split, block, replan, final review | `smr-core::orchestrator::phases` | the end-to-end fake run (`crates/adapters/tests/fake_loop.rs`, the port of `run-fake.sh`) |
| Ledger and journal with the task loop's field names | `smr-core::ledger` | round-trip tests |
| Lease manager and gate | `smr-core::lease` | fake engines: full cycle, failed job, failed reload |
| git, `verify` process, clock, notifier, macOS memory probe, Prometheus metrics | `smr-adapters` | temp repositories, scripts, mock servers |
| Sushi, generic and ComfyUI adapters | `smr-adapters` | mock HTTP servers |
| Engine proxy: hold, per-request usage (JSON and SSE), error classification | `smr-proxy` | fake engine behind it |
| ACP harness: initialize, session/new with MCP servers and meta, tool calls, permissions, cancel, proxy usage merge | `smr-acp` | an in-process ACP agent built with the same crate |
| Media MCP server with `generate_image` through the lease | `smr-mcp` | rmcp client over HTTP against fake engines |
| `smr` binary: init, run, status, params | `smr` | the binary end to end with the fake harness |

## Not yet verified against the real thing

- **fx.** The spike's binary is gone. `smr run` assumes `fx acp` starts the
  ACP server (`--fx-args` overrides it) and that the profile in
  `.agent/private/fx-home/.fx/` is read with `HOME` pointed there. Both need
  a check against fx 0.0.14.
- **Steering over ACP.** ACP has no steer message. The watchdog's steers are
  recorded; only the abort (`session/cancel`) acts. fx advertises
  `_meta.fx.steering`; wiring it is an open point.
- **Sushi load and unload** through the lease in a live session (the adapter
  is tested against a mock), and a cold load after a media job.
- **ComfyUI** with a real workflow: the tool needs an API-format workflow
  file with `{{prompt}}`, `{{negative}}`, `{{seed}}`, `{{width}}` and
  `{{height}}` placeholders.
- **Long media calls through fx.** The runner offers its MCP server both in
  `session/new` and in the fx profile with a long `operation_timeout_ms`,
  and sets `_meta.fx.profileMcpServers`. Which route fx takes for a 10-minute
  call is to be measured.
- **Pi RPC harness.** Not started; the `Harness` port is where it goes.
- **Linux and NVIDIA memory probe.** Not started; the probe port is where it goes.

## Known limits

- The orchestrator calls git synchronously inside the async loop; fine for
  short commands, to be moved to a blocking pool if it ever shows.
- `verify`'s time limit kills the process but not grandchildren it left.
