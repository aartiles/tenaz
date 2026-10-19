# Compatibility

What Tenaz has been run against, what is built but only tested against
fakes, and what is not there yet. `cargo test` needs none of the tools
below; this page is about the real ones.

## At a glance

| Piece | Works with | Status |
|---|---|---|
| Platform | macOS arm64 | Tested. |
| | Linux x86_64 | Builds and passes the test suite in CI. Not run against a real engine yet; no memory sampling (the probe is macOS only). |
| | Windows | Not supported. |
| Harness | [fx](https://github.com/vercel-labs/fx) 0.0.14 over ACP | Tested end to end: preflight, bootstrap, build, review, steering, pause and resume. |
| | [Pi](https://github.com/earendil-works/pi) 1.1.0 in RPC mode | Tested end to end to a COMPLETE run. Talks to the engine directly, so no proxy and no media tools (see below). |
| | Other ACP agents | Should work through the same adapter; fx-specific settings are only applied to fx. Untested. |
| Engine | Sushi 1.2.4 (`--engine sushi`) | Tested for inference. Load and unload through its API are tested against a mock only. |
| | Any OpenAI-compatible server (`--engine generic` with `--engine-start` / `--engine-stop`): llama.cpp `llama-server`, LM Studio, Ollama's `/v1`, llama-swap, vLLM | Tested against a mock only. |
| Model | Any model the engine serves, with tool calling | Tested with `Qwen3.8-Flash-Next-Sushi-2.6bpw`. |
| Media | ComfyUI images (`generate_image` over MCP) | Tested against a mock. Needs an API-format workflow with `{{prompt}}`, `{{negative}}`, `{{seed}}`, `{{width}}` and `{{height}}` placeholders. |
| | TTS, STT, video | Not built. |

## What the model needs

The preflight runs before the first session and checks:

- **Tool calling** through the OpenAI chat completions API.
- **Reasoning output.** A model that cannot think fails the preflight; pass
  `--allow-weak-engine` to record the failure and run anyway.
- **Prefix caching.** Without it every turn pays the whole prompt again.
  Same flag.
- **Vision**, only when the project was initialised with `--vision`.

Fresh sessions per task keep contexts short (6–35k tokens in the runs
above), so a 32k context is the practical floor (`contextFloor`).

## Ports and processes

- Tenaz binds no fixed port. The engine proxy and the media MCP server
  listen on `127.0.0.1` on ports the OS picks.
- The engine is wherever `--base-url` points. Without it the default is
  `http://127.0.0.1:12345/v1` (Sushi's default).
- ComfyUI is wherever `--comfyui` points, usually `http://127.0.0.1:8188`.
- fx runs with an isolated `HOME` under `.agent/private/fx-home/`, so your
  own fx settings and MCP servers are not touched. Pi uses your
  `~/.pi/agent/models.json` for the provider.

## Known gaps

- **Pi bypasses the proxy.** The engine hold while a media job runs, the
  per-request log and the media tools cover fx only.
- **Media through fx with long jobs.** Which route fx takes for a
  10-minute tool call has not been measured.
- **Linux and NVIDIA memory.** Memory pressure, swap and the engine's
  footprint are sampled on macOS only.
- `verify`'s time limit kills the process but not grandchildren it left; a
  tool call that forks without limit is not stopped by the runner.
- A pause during a bootstrap session counts as one of its attempts, and
  `--resume` does not reset the count: a run blocked in bootstrap needs
  `maxBootstrapAttempts` raised in `.agent/run.json`.
- Ctrl-C during the preflight ends the run instead of pausing it.

## Model and engine behaviour seen in testing

These are not runner bugs, but they decide whether a run gets far:

- **Reasoning-only answers.** With thinking at `medium` or `high`, the
  tested model often ended a bootstrap turn with reasoning only and no
  tool call. The runner retries the bootstrap, but a run can block there.
- **Lost tool arguments.** The engine sometimes streamed `{}` as a tool
  call's arguments; the harness rejects them and the watchdog stops the
  session.
- **Engines that report draft acceptance** in llama.cpp's
  `draft_n` / `draft_n_accepted` layout get it in `requests.jsonl` and
  `tenaz status`. Sushi 1.2.4 reports it only in its log.
