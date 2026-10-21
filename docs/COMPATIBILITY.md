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
| | [Pi](https://github.com/earendil-works/pi) 1.1.0 in RPC mode | Tested end to end to a COMPLETE run with the user's own `models.json`. The generated configuration, through the proxy, is tested against a mock engine only. No media tools (see below). |
| | Other ACP agents | Should work through the same adapter; fx-specific settings are only applied to fx. Untested. |
| Engine | Sushi 1.2.4 (`--engine sushi`) | Tested for inference. Load and unload through its API are tested against a mock only. |
| | Any OpenAI-compatible server (`--engine generic` with `--engine-start` / `--engine-stop`): llama.cpp `llama-server`, LM Studio, Ollama's `/v1`, llama-swap, vLLM | Tested against a mock only. |
| Model | Any model the engine serves, with tool calling | Tested with `Qwen3.8-Flash-Next-Sushi-2.6bpw`. |
| Media | ComfyUI images (`generate_image` over MCP) | Tested against a mock. Needs an API-format workflow with `{{prompt}}`, `{{negative}}`, `{{seed}}`, `{{width}}` and `{{height}}` placeholders. |
| | TTS, STT, video | Not built. |

## What the model needs

The preflight runs before the first session and checks:

- **Tool calling** through the harness's own tools: the model must run
  `cat probe.txt` with the agent's shell tool and report the word in the
  file. A model that calls tools with malformed arguments fails here, not
  three bootstrap sessions later. Same flag.
- **Reasoning output.** A model that cannot think fails the preflight; pass
  `--allow-weak-engine` to record the failure and run anyway.
- **Prefix caching.** Without it every turn pays the whole prompt again.
  Same flag.
- **Vision**, only when the project was initialised with `--vision`.

Some engines' tool-call parsers hand an object argument back as JSON
text when its schema has no `type` (a `oneOf` of objects, as fx declares
`shell.request`); TensorFold does (ashhart/TensorFold#1121), and fx then
rejects every shell call. The engine proxy repairs exactly that: an
argument whose schema accepts only objects and whose value is a string
holding a JSON object is parsed back, in streamed and whole responses;
anything else passes through untouched. Each repair is logged in
`.tenaz/private/tool-repairs.jsonl` and counted by `tenaz status`;
`--set repairToolArgs=false` turns it off. Pi's tools take flat
arguments, so nothing of theirs is repaired; with `--no-proxy` nothing
is.

Fresh sessions per task keep contexts short (6–35k tokens in the runs
above), so a 32k context is the practical floor (`contextFloor`).

## Ports and processes

- Tenaz binds no fixed port. The engine proxy and the media MCP server
  listen on `127.0.0.1` on ports the OS picks.
- The engine is wherever `--base-url` points. Without it the default is
  `http://127.0.0.1:12345/v1` (Sushi's default).
- ComfyUI is wherever `--comfyui` points, usually `http://127.0.0.1:8188`.
- `tenaz init`'s wizard looks for engines on 12345 (Sushi), 8080
  (llama.cpp, llama-swap), 1234 (LM Studio), 11434 (Ollama) and 8000 (vLLM),
  and for ComfyUI on 8188, with an 800 ms timeout each. The context window
  comes from the engine where it reports one (Sushi, vLLM, llama.cpp's
  `/props`, LM Studio's `/api/v0/models`); Ollama does not, so the wizard
  asks. Detection is tested against recorded answers, not live engines.
- fx runs with an isolated `HOME` under `.tenaz/private/fx-home/`, so your
  own fx settings and MCP servers are not touched. Pi runs with its own
  agent directory, `.tenaz/private/pi-agent/` (`PI_CODING_AGENT_DIR`),
  holding a `models.json` with the run's one model, pointed at the proxy,
  and a `settings.json` with a 15-minute HTTP idle timeout and install
  telemetry off. A model your `~/.pi/agent/models.json` lists is copied
  from there with its provider settings; one it does not list is
  described as an OpenAI-compatible endpoint with `system` rather than
  `developer` messages, no `store` and `max_tokens`. Your own Pi
  settings, credentials (`auth.json`), extensions and context files are
  not used: a provider you signed in to with `/login` needs its `apiKey`
  in `models.json` instead.
- Skills: by default the agent sees only the skills pinned with
  `init --skill`, and none without them. Pi gets `--no-skills` plus each
  pinned skill. fx's skill catalog gets a 0-byte budget, because fx also
  lists the skills of every folder above the project (`~/.claude/skills`
  and `~/.agents/skills` for a project under the home folder); pinned
  skills are linked into its isolated `HOME`, and with them fx lists those
  folders' skills again. `--set agentSkills=true` lets the agent load the
  skills it finds by itself.

## Known gaps

- **Pi and the proxy.** Pi's requests go through the proxy, so the engine
  hold covers them, but the media tools are fx only (Pi runs with
  `--no-mcp`), and Pi's request records take their usage from Pi rather
  than the proxy, so they carry no server timings (draft acceptance).
  `--no-proxy` points Pi straight at the engine.
- **Media through fx with long jobs.** Which route fx takes for a
  10-minute tool call has not been measured.
- **Linux and NVIDIA memory.** Memory pressure, swap and the engine's
  footprint are sampled on macOS only.
- **The sandbox** (`sandbox=true`) has been tested on macOS, with `verify`
  and a Pi session against a local model. The Landlock version for Linux
  compiles and its rules are unit-tested, but it has not run on a Linux
  kernel yet; neither has fx in the sandbox.
- `verify`'s time limit kills the process but not grandchildren it left; a
  tool call that forks without limit is not stopped by the runner.
- A pause during a bootstrap session counts as one of its attempts. An
  attempt that leaves fewer problems than the one before does not count,
  up to `maxBootstrapAttempts` such attempts. `--resume` on a run blocked
  in bootstrap starts both counts again. Task cards a model wrapped in a
  code fence are unfenced before they are checked, and task ids listed
  under `requires` are read as dependencies. When cards a planner wrote
  later cannot be read, a replan session gets the problems to fix, twice
  in a row at most, before the run blocks.
- **Existing projects.** `init` refuses a project directory that holds
  files of a larger repository (a monorepo package, for example): point
  it at the repository root, or pass `--in-place` to make the directory a
  nested repository of its own. A directory inside a repository that
  tracks nothing in it (ignored, or not added yet) is a new project and
  becomes a nested repository without `--in-place`. `tenaz finish` authors the
  delivered commit as Tenaz rather than as you (amend it with
  `git commit --amend --reset-author` if needed), and with
  `--remove-worktree` it removes the run's worktree without asking for
  confirmation. The existing-project mode (`.tenaz/verify` and
  the bootstrap section) has only been exercised with the scripted harness,
  not yet with a real model.
- Whether fx loads the project's `AGENTS.md` by itself has not been
  checked; Pi does. The Tenaz protocol is in every prompt either way.

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
