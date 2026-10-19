# Status

As of 2026-10-18. Everything below is in the repository; the fx and Pi
verification changes are not committed yet.

## Built and verified by tests

| Piece | Where | Verified by |
|---|---|---|
| Task cards, spec, backlog, coverage, `verify` contract | `tenaz-core` | unit tests |
| Watchdog rules (loop, repeat, idle, big output, context, turns, compaction, provider errors) | `tenaz-core::watchdog` | unit tests with the task loop's thresholds |
| Session wrapper: engine retries with backoff, capacity cap, context and image continuations, pause | `tenaz-core::orchestrator::session` | the end-to-end fake run |
| Phases: bootstrap validation, attempts, review with retry, split, block, replan, final review | `tenaz-core::orchestrator::phases` | the end-to-end fake run (`crates/adapters/tests/fake_loop.rs`, the port of `run-fake.sh`) |
| Ledger and journal with the task loop's field names | `tenaz-core::ledger` | round-trip tests |
| Lease manager and gate | `tenaz-core::lease` | fake engines: full cycle, failed job, failed reload |
| git, `verify` process, clock, notifier, macOS memory probe, Prometheus metrics | `tenaz-adapters` | temp repositories, scripts, mock servers |
| Sushi, generic and ComfyUI adapters | `tenaz-adapters` | mock HTTP servers |
| Engine proxy: hold, per-request usage and finish reason (JSON and SSE), `reasoning_effort` default, error classification | `tenaz-proxy` | fake engine behind it, and Sushi (below) |
| ACP harness: initialize, session/new with MCP servers and meta, tool calls, permissions, cancel, fx steering (a `session/prompt` with `_meta.fx.steer` while a turn runs), proxy usage merge with the engine's finish reason | `tenaz-acp` | an in-process ACP agent built with the same crate, and fx 0.0.14 (below) |
| Pi harness: `piArgs` command line, prompt ids, tool calls, usage and stop reasons, steer, `clear_queue` + `abort`, prompt rejection, raw and stderr logs, close with kill | `tenaz-pi` | the scripted `fake-pi` binary over real pipes; `models.json` reading; `pi --version` when installed; streams recorded from Pi 1.1.0 (below) |
| Media MCP server with `generate_image` through the lease | `tenaz-mcp` | rmcp client over HTTP against fake engines |
| `tenaz` binary: init, run, status, params; `init --harness pi` from `models.json` | `tenaz` | the binary end to end with the fake harness; init with a stand-in `pi` |

## Verified against fx 0.0.14 and Sushi (2026-10-17/18)

fx 0.0.14 (build `1a070af32cdf`, the official macOS arm64 release,
`fx-macos-aarch64.tar.gz` from `github.com/vercel-labs/fx/releases/tag/v0.0.14`,
sha256 `619c6dcc…7b1`, installed at `~/.local/bin/fx`) against Sushi 1.2.4
serving `Qwen3.8-Flash-Next-Sushi-2.6bpw`. The run is kept, uncommitted, in
`local-runs/fx-smoke-20261017/` (brief, `run.log`, the project with its
`.agent/journal.jsonl`, `.agent/private/requests.jsonl`, the per-session
`*.rpc.jsonl` event logs and fx's own `fx-acp.log`).

- **`fx acp` is the ACP server** (`fx acp [--model <id>] [--ultrafast|--no-ultrafast] [--log-file <path>]`;
  global flags go before `acp`). The default in `FxLaunch::new` was right.
  The wiring adds `--log-file .agent/private/<label>.fx-acp.log`, one per
  session (fx truncates the file it is given; a shared log kept only the
  last session's), the only place fx's diagnostics go in ACP mode.
- **The isolated `HOME` works.** With `HOME` pointed at
  `.agent/private/fx-home/`, `fx status` reads the provider, model and
  `permission_mode` from `.fx/settings.json`, and `fx mcp path` resolves to
  `.fx/mcp.json` there. `FX_AUTO_UPGRADE=0` is set so fx makes no network
  call beyond the engine.
- **Profile MCP servers need no trust step.** `fx status` lists a server
  from `~/.fx/mcp.json` as `scope=profile admission=not_applicable`;
  `fx mcp trust approve` applies only to a project's `.mcp.json`. Nothing
  was added to the wiring.
- **Reasoning must be asked for by the proxy.** fx 0.0.14 records
  `effort` (`fx status` shows it) but sends no `reasoning_effort` to a custom
  `openai-chat-completions` provider: its log says
  `reasoning=unsupported_or_missing`, and the request body carries only
  `model`, `stream`, `stream_options`, `messages`, `tools`, `max_tokens`.
  Sushi's default effort is `off`, so the first preflight failed with
  `reasoning=false`. The proxy now fills `reasoning_effort` from the
  ledger's `thinking` level (`Proxy::request_defaults`, absent fields only);
  Sushi then streams `reasoning_content`, which fx relays as
  `agent_thought_chunk`, and reports `completion_tokens_details.reasoning_tokens`.
- **Preflight passes**: reasoning (587 thought chars), prefix cache
  (8,292 cached tokens on the second turn), vision (`read` of the probe PNG
  answered "red blue"), 18.5 s, 4 proxied requests all matched to Sushi's
  Prometheus counters.
- **Per-request usage arrives through the proxy** while fx reports only one
  `usage_update` per turn: every engine request becomes one `message_end`
  with prompt, cached, output and reasoning tokens in
  `.agent/private/requests.jsonl`, with the engine's prefill and decode
  timings attached.
- **Permissions in `ask` mode** reach the runner as `session/request_permission`
  for every tool call (`shell`, `read_file`, `glob_files`, `write_file`…),
  and the runner's `allow_once` answer lets the call run.
- **The engine's finish reason is now kept.** The proxy reports
  `finish_reason`; the ACP adapter maps it (`stop`, `tool_calls`, `length`)
  instead of assuming a tool call. Seen in the run: a bootstrap response of
  4,848 tokens, all reasoning, that ended with `stop` and no tool call; fx
  ended the turn, the session settled with nothing written and the runner's
  bootstrap validation sent it back for another attempt.
- **Client timings** in `requests.jsonl` were near zero because the
  orchestrator sampled its clock before waiting for the next event; it now
  samples it when the event arrives.
- **Steering over ACP works** (2026-10-18, run `local-runs/fx-steer-20261018/`).
  fx advertises `agentCapabilities._meta.fx.steering: true` at initialize;
  the adapter records the flag and, while a prompt is in flight, sends the
  watchdog's steer as a text `session/prompt` with `_meta: {"fx": {"steer":
  true}}` through `cx.send_request`, outside the crate's `send_prompt`, so
  the steer's own `PromptResponse` is only logged and never becomes a second
  `Settled`. Without the flag the steer stays a no-op. Observed with the
  idle rule lowered to 2 turns (`--set idleSteer=2`) in the bootstrap
  session: the runner logged `001-bootstrap: steer:idle` at 22:56:58Z, fx's
  log shows `[acp] queued steering prompt bytes=175` in the same
  millisecond (the 175-byte `[orchestrator] You have gone 2 turns …`
  message) while step 2's `shell` call was waiting for its permission; fx
  did not cancel the call, and the next model boundary (step 3, 0.6 s
  later) went out with 15 messages where step 2's 12 plus its one tool call
  account for 14: the steer went in as the extra message. The model's step
  3 answer was "Skeleton is empty … Writing the harness, spec, map, tasks,
  and notes now" followed by a `write_file`, which is what the steer asked
  for. The steer did not end the turn: the session went on for 23 turns
  (peak context 35,637) and settled at 23:21:07Z with exactly one
  `settled` event in its `.rpc.jsonl`; the adapter's debug log shows the
  steer's own `PromptResponse` (`EndTurn`) arriving in the same millisecond
  as fx's `prompt_finish`, 0.7 s before the turn's `Settled`, and it was
  only logged. The run was then paused with Ctrl-C during `002-bootstrap`.
  Sushi was shared with the still-running `fx-smoke-20261017` run and
  others (4 requests in flight), so step 3's 7,367-token answer took 659 s.
- **fx aborts on a relative `--log-file`** (`panic: reached unreachable
  code`, before initialize, whatever the current directory). `tenaz run
  --project ./project` produced exactly that; `FxLaunch::command_args` now
  passes the log path absolute.

### The run (`local-runs/fx-smoke-20261017/`)

Brief: a Python `wordstats.py` with tests, five requirements. Sushi was
shared with the owner's own task-loop run the whole time, so wall times are
about half of what the model does alone (decode about 23 tokens/s per
request, 2 requests running). `tenaz init` pinned fx 0.0.14; `tenaz run` went
through the preflight, two bootstrap sessions, the T001 build, verify,
commit and review, and on into T002. The run was paused with Ctrl-C during
the T002 builder (the session was aborted, the pause journaled) and resumed
with the rebuilt binary without `--resume`; the ledger, journal and git
history carried over.

| Session | Turns, tool calls, writes | Wall | Tokens: input / cached / output (reasoning) | Peak context | Outcome |
|---|---|---|---|---|---|
| 001-bootstrap | 3, 2, 0 | 279 s | 9,858 / 34,810 / 5,171 (4,920) | 21,300 | settled; a 4,848-token reasoning-only answer, nothing written; bootstrap validation failed (4 problems) |
| 002-bootstrap | 23, 22, 16 | 704 s | 16,174 / 507,423 / 12,438 (5,480) | 28,813 | accepted: `verify` with 2 `T000.` checks, `spec.md` (14 requirements), `map.md`, 7 task cards, `BOOTSTRAP.md` |
| 003-builder-T001 | 10, 9, 4 | 250 s | 13,726 / 173,901 / 4,788 (2,122) | 22,022 | `./verify --all` 4 passed, committed `task T001: … (attempt 1)` |
| 004-reviewer-T001 | 6, 7, 1 | 202 s | 10,258 / 109,562 / 3,866 (1,832) | 22,818 | `ACCEPT` with three minor notes; T001 done in 1 attempt |
| 005-builder-T002 | 3, 4, 0 | 77 s | 3,985 / 44,493 / 362 (110) | 17,304 | aborted by Ctrl-C; the run paused and resumed as 006 |
| 006-builder-T002 (rebuilt binary) | 16, 16, 6 | 370 s | 17,721 / 321,658 / 5,972 (2,785) | 25,981 | `./verify --all` 6 passed, committed; 15 requests ended `tool_calls`, the last `stop` |

The run was left going in the background after T002's review started
(2026-10-17T22:31Z); the journal shows how far it got.

Per request (`requests.jsonl`): prompt 13–18k tokens of which 12–16k cached
once the prefix is warm, first-token 0.1–1.5 s, tool-call turns of 70–230
output tokens take 8–30 s wall under the shared load; the three bootstrap
answers that wrote files were 1,170–4,848 output tokens. fx's log
(`fx-acp.log`) shows every tool call going through `permission_requested`
→ `after_permission_decision decision=once approval_source=interactive_once`
and `effort=medium reasoning=unsupported_or_missing` on every request.


## Verified against Pi 1.1.0 and Sushi (2026-10-18)

Pi 1.1.0 (`pi --mode rpc`, fnm node 24) against Sushi 1.2.4 serving
`Qwen3.8-Flash-Next-Sushi-2.6bpw`, `tenaz init --harness pi --provider sushi`
and `tenaz run --no-proxy`, with a three-requirement shell brief (`greet.sh`,
`VERSION`, `verify`). Sushi was shared with other sessions' runs the whole
time (2–3 requests running, decode down to about 7 tokens/s), so wall times
are long and `engine_matched` is mostly false. The runs are kept,
uncommitted, in `local-runs/pi-smoke-20261018/` (its `README.md` lists them).

- **Preflight passes** through Pi: `reasoning=true cache=true` with thinking
  on (it fails on reasoning with `--thinking off`, as it should; those runs
  used `--allow-weak-engine`).
- **Per-message usage** comes from each `message_end`: `input`, `output`,
  `cacheRead`, `reasoning` (Sushi's reasoning tokens), and the stop reason.
  `requests.jsonl` now has `reasoning_tokens` per row next to prompt,
  cached and output tokens; the journal's session usage sums all four.
- **Steer reaches the real process.** With `--set turnBudget=2`, the
  `steer` command was answered `{"disposition":"queued"}`, Pi emitted
  `queue_update`, and delivered the text as a user message after the
  turn's tool call, before the next model call. In the COMPLETE run below,
  four natural steers (`loop`, `repeat`) were all queued and delivered.
- **Abort reaches the real process.** `clear_queue` (answered with the
  emptied queues), then `abort`: Pi ends the running request with an
  assistant `message_end` of `stopReason: "aborted"`, `errorMessage:
  "Request was aborted"` and zero usage (the tokens the engine spent on the
  cut-off answer are not reported), then `turn_end`, `agent_end`,
  `agent_settled {aborted: true}`, and only then the `abort` response.
  Seen for the turn-budget abort, a `repeat` abort and Ctrl-C pauses.
- **Builder and reviewer sessions settle.** From a hand-written bootstrap
  (spec, map, two cards, a `verify` with one `T000.` check; ledger set to
  `LOOP`): builder T001 (aborted by the `repeat` rule, `verify` passing,
  accepted), reviewer T001, builder T002, reviewer T002 and the final review
  all ran through Pi, and the run finished **COMPLETE** with `./verify
  --all` passing 4 checks.

| Session (seeded run, thinking off) | Turns, tool calls, writes | Wall | Tokens: input / cached / output | Peak context | Outcome |
|---|---|---|---|---|---|
| 001-builder-T001 | 22, 21, 12 | 166 s | 4,940 / 107,840 / 2,328 | 6,487 | 2 `loop` + 2 `repeat` steers, aborted (same `bash` call 6 times); `verify` passed; done in 1 attempt |
| 002-reviewer-T001 | 6, 6, 2 | 57 s | 2,949 / 28,523 / 813 | 6,012 | settled, accepted |
| 003-builder-T002 | 15, 14, 6 | 137 s | 4,408 / 78,580 / 2,020 | 7,227 | settled, committed |
| 004-reviewer-T002 | 5, 4, 1 | 80 s | 3,053 / 25,470 / 956 | 6,665 | settled, accepted |
| 005-final-reviewer | 6, 5, 1 | 127 s | 4,973 / 30,424 / 1,628 | 8,050 | settled; COMPLETE |

### Defects found and fixed

- **Pi wrote its sessions under `<project>/<project>/`.** With a relative
  `--project`, `--session-dir` (and `--skill`) were passed relative to the
  runner's directory while Pi runs in the project. `PiLaunch::args` now
  makes them absolute (test: `relative_paths_are_resolved_against_the_runners_directory`).
- **`verify` could not start under a relative `--project`** ("cannot run
  verify: No such file or directory"): the program path was joined to the
  root and then resolved from the root again. It is now spawned as
  `./verify` in the root (test: `runs_verify_under_a_relative_root`), and
  `tenaz init`/`run` make `--project` absolute once (the fake-harness CLI
  test now runs with a relative project).
- **A pause dropped the aborted turn and gave no grace.** After a long wait
  for the model, Ctrl-C (and the `sessionMinutes` limit) set the 5 s kill
  deadline from the clock reading taken before the wait, so it had already
  passed: the loop closed the session at once, Pi was killed, and the
  journal kept the previous stop reason. Both deadlines now start when the
  wait ends; the paused session records `aborted` (test:
  `crates/adapters/tests/stop_grace.rs`; Pi's real abort stream is in
  `an_aborted_stream_as_pi_1_1_0_ends_it`).

### Model and engine behaviour seen (not the adapter)

- **Reasoning-only stops.** With thinking `medium` or `high`, all 6
  bootstrap sessions that were not paused ended after 2–3 turns on an
  assistant message holding only reasoning, cut off mid-sentence with
  `finish_reason: stop` (35–1,914 tokens, all reasoning). The bootstrap was
  never written; the `medium` run blocked on it. The fx run saw the same
  once.
- **Tool-call arguments lost by the engine.** In builder T001, Sushi
  streamed `"{}"` as the arguments of 7 `bash`/`write` calls (one message
  reported 101 output tokens for a `{}` call); Pi's validation rejected
  them ("must have required properties command") and the watchdog's
  `repeat` rule stopped the session.
- **A self-recursive `verify`.** With thinking off, the bootstrap sessions
  wrote spec, map and four cards in 25 turns each, but a later one made
  `verify --all` call itself; one bash call grew to about 2,200 `./verify`
  processes before it was killed by hand. Nothing in the runner limits
  processes a tool call starts.

## Not yet verified against the real thing

- **Sushi load and unload** through the lease in a live session (the adapter
  is tested against a mock), and a cold load after a media job.
- **ComfyUI** with a real workflow: the tool needs an API-format workflow
  file with `{{prompt}}`, `{{negative}}`, `{{seed}}`, `{{width}}` and
  `{{height}}` placeholders.
- **Long media calls through fx.** The runner offers its MCP server both in
  `session/new` and in the fx profile with a long `operation_timeout_ms`,
  and sets `_meta.fx.profileMcpServers`. Which route fx takes for a 10-minute
  call is to be measured.
- **A bootstrap the model itself gets accepted under Pi.** Every
  bootstrap with Pi and this model failed for model or engine reasons
  (above); the Pi COMPLETE run started from a hand-written bootstrap.
- **Pi behind the proxy, and media in a Pi session.** Pi reads its base URL
  from `~/.pi/agent/models.json`, so the proxy's hold, its request log and
  the lease do not cover it, and with `--no-mcp` it has no image tool.
- **Linux and NVIDIA memory probe.** Not started; the probe port is where it goes.

## Fixed after the Pocket Ledger run (2026-10-18)

The run with tenaz, fx and Sushi on Pocket Ledger
(`local-model-benchmarks`, report
`benchmarks/reports/2026-10-18-qwen38-flash-next-sushi-v124-smr-pocket-ledger.md`)
completed and reported three runner issues:

- **A harness exit was charged as a task attempt.** fx exited with code 1
  mid-turn while a `shell` call waited for permission. A session that ends
  before its turn settles, without the runner stopping it, is now journaled
  with `harness_exit` (exit code and the tail of its stderr), retried like an
  engine failure (`engine-retry` with `cause: "harness"`), and not charged.
  The ACP harness now appends the agent's stderr, and the error its
  connection ended with, to `.agent/private/<label>.stderr.log`. Tested end
  to end with the scripted harness (`crates/adapters/tests/harness_exit.rs`)
  and with a real agent process that dies on the prompt
  (`crates/acp/tests/subprocess_exit.rs`).
- **fx's log was overwritten by every session.** Now one log per session (above).
- **No draft counts in `requests.jsonl`.** Each row now has `server`, the
  engine's own `timings` for that request when its response carries them:
  `prompt_ms`, `decode_ms`, `cached_tokens`, and `drafted`/`accepted` in
  llama.cpp's `draft_n`/`draft_n_accepted` layout. `status` prints the draft
  acceptance when any were reported. **Sushi 1.2.4 does not report drafts
  this way**: its `timings` object has prefill and decode figures only, and
  its `/metrics` has no draft series; the MTP counts appear only in its log
  (`[spec-stats] drafted=… accepts=…`). For Sushi, draft acceptance still
  comes from the log until Sushi adds `draft_n` to `timings`.

Not addressed: the preflight took 2 min 10 s in that run against 18.5 s
here, with the engine idle. It is reported as an observation; no cause is
known yet.

## Known limits

- The orchestrator calls git synchronously inside the async loop; fine for
  short commands, to be moved to a blocking pool if it ever shows.
- `verify`'s time limit kills the process but not grandchildren it left.
- A pause during a bootstrap session counts as one of its attempts, and
  `--resume` does not reset the count: a run blocked in bootstrap cannot be
  continued without editing `maxBootstrapAttempts` in `.agent/run.json`.
- A Ctrl-C during the preflight ends the run with "Engine preflight
  failed" instead of a pause.
