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
| Engine | Sushi 1.2.4 and 1.3.0, and mlx-serve 26.10.3 (`--engine load-api`) | Tested for inference. Sushi 1.3.0 unloaded and reloaded the LLM through its API for each of 10 images in one run (about 8 s per reload). mlx-serve's load and unload through the API are tested against a mock only. |
| | Any OpenAI-compatible server (`--engine generic` with `--engine-start` / `--engine-stop`): llama.cpp `llama-server`, LM Studio, Ollama's `/v1`, llama-swap, vLLM | Tested against a mock, and with TensorFold native 1.1.4 stopped and started by its own scripts around each of 8 images in one run. The servers named here are untried. |
| | MLX's `mlx_lm.server`, TensorFold (`--engine generic`) | Untested. `tenaz init` does not look for either: `mlx_lm.server` listens on 8080 by default, where the wizard expects llama.cpp, so it may be found under that name; otherwise pass `--base-url`. TensorFold needs the proxy's argument repair (see below). |
| Model | Any model the engine serves, with tool calling | Tested with `Qwen3.8-Flash-Next-Sushi-2.6bpw`. |
| Media | ComfyUI images (`generate_image` over MCP) | Tested against a mock, and in real runs with FLUX models next to TensorFold and Sushi (8 and 10 images). Needs an API-format workflow with `{{prompt}}`, `{{negative}}`, `{{seed}}`, `{{width}}` and `{{height}}` placeholders. |
| | OpenAI Images API (`--images`, `POST /v1/images/generations`): mlx-serve, Rapid-MLX, mlx-omni-server, LocalAI, Atomic Chat and others | Tested against a mock, and by hand against mlx-serve 26.10.3 (see Known gaps). Sends `model`, `prompt`, `size`, `n` and `response_format: b64_json`, plus `seed` and `negative_prompt`, which servers that do not know them may ignore. Does not unload the image model after a job. On the LLM's own origin the LLM stays loaded. |
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
anything else passes through untouched. It also adds the `action` that
models drop from fx's `shell` call: when a call sends a `command` without
an `action` and every variant of the argument that takes a `command` fixes
the same `action` (`run` in fx 0.0.14, whose two `run` variants differ in
the shell), the proxy sets it. Properties sent beside `request` instead of
inside it (`{"request": {...}, "timeout_ms": 5000}`) are moved in when the
variant its `action` selects takes them all and `request` does not already
hold them, and an integer outside the range the variant declares (a
`yield_time_ms` above 30000) is clamped to it. Each repair is logged in
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
- `tenaz init`'s wizard and `tenaz doctor` look for engines on 12345
  (Sushi), 11234 (mlx-serve), 8080 (llama.cpp, llama-swap), 1234 (LM Studio), 11434 (Ollama)
  and 8000 (vLLM), and for image engines (an engine on those ports that
  lists an image model, a server on 1337 (Atomic Chat) that does, and
  ComfyUI on 8188), with an 800 ms timeout
  each. mlx-serve 26.10.3 lists its image models at `/v1/models` and
  serves `/v1/images/generations`. With
  `TENAZ_NO_PROBE` set, neither looks at these ports: only the addresses
  the flags give are used. An image model is
  recognised by its name (FLUX, SDXL, SD3, Z-Image, Qwen-Image and
  similar); one with another name is not offered. The context window
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
- Skills: by default Pi sees only the skills pinned with `init --skill`,
  and none without them: it gets `--no-skills` plus each pinned skill.
  fx's skill catalog gets a 0-byte budget, so its prompt lists no skills,
  but see the known gap below; pinned skills are linked into its isolated
  `HOME`, and with them fx lists the skills of the folders above the
  project again. `--set agentSkills=true` lets the agent load the skills
  it finds by itself.

## Known gaps

- **fx and the skills above the project.** fx looks for skills from the
  project upward and stops only at `HOME`. Tenaz gives fx a `HOME` inside
  the project, so for a project under your home folder fx goes on past it
  and finds the skills you installed for other agents (`~/.claude/skills`,
  `~/.agents/skills`, `~/.codex/skills`, and a repository's `skills/` when
  the run is in a worktree beside it). The 0-byte catalog keeps them out
  of the prompt, but fx 0.0.14 still returns them from `capability_search`
  and loads any of them with its `skill` tool, without a permission
  request, so Tenaz cannot refuse it. Seen with a scripted model over
  `fx acp`: 354 skills under the home folder, none for a project in
  `/tmp`. In the image-landing run of 2026-10-25 one `capability_search`
  returned a cloud media skill, which the model did not load. With skills
  pinned, the catalog is on: in the explainer run of 2026-10-27 fx listed
  about 350 skills next to the 5 pinned ones, and the model loaded only
  pinned ones. A project
  outside your home folder avoids it; a fix needs fx
  ([vercel-labs/fx#1458](https://github.com/vercel-labs/fx/issues/1458)).

- **Pi and the proxy.** Pi's requests go through the proxy, so the engine
  hold covers them, but the media tools are fx only (Pi runs with
  `--no-mcp`), and Pi's request records take their usage from Pi rather
  than the proxy, so they carry no server timings (draft acceptance).
  `--no-proxy` points Pi straight at the engine.
- **mlx-serve serving the LLM and the images.** When `--images` is the
  LLM's own server, Tenaz keeps the LLM loaded and leaves the memory to
  the server (`lease llm kept` in the journal). mlx-serve 26.10.3 does
  not make room by itself with its defaults: up to 3 models stay loaded,
  and its free-memory check refused to load Qwen-Image 2.1 8-bit next to
  Qwen3.8 27B 4-bit (`503 … InsufficientMemory`) in the image-landing run
  of 2026-10-26. Set "Max models loaded at once" to 1
  (`--max-resident-models 1`) and restart the server: an image request
  then unloads the LLM and loads the image model, and the next chat
  request swaps them back. Checked by hand: a 1536×864 image in 147 s,
  the LLM back in 2 to 4 s, the size and seed respected. The first chat
  request after each image computes its prompt again, since the prefix
  cache goes with the LLM.
- **Media through fx with long jobs.** Which route fx takes for a
  10-minute tool call has not been measured.
- **Linux and NVIDIA memory.** Memory pressure, swap and the engine's
  footprint are sampled on macOS only.
- **The sandbox** (`sandbox=true`) has been tested on macOS, with `verify`
  and a Pi session against a local model. The Landlock version for Linux
  compiles and its rules are unit-tested, but it has not run on a Linux
  kernel yet; neither has fx in the sandbox.
- **fx and output that is not UTF-8.** fx 0.0.14 exits with "stream did
  not contain valid UTF-8" when a shell command prints such bytes (`head -c`
  cutting a character in half, for example). Tenaz charges the attempt and
  tells the next session not to cut output by bytes; the session's work in
  progress is lost. The fake harness reproduces the crash; it has not been
  rerun against fx itself.
- **Replies cut at the output limit.** fx is given a 16384-token output
  limit; a reply that reasons past it ends the session (`OutputTruncated`).
  Tenaz charges the attempt, says why in the next briefing, and lowers the
  reasoning effort for the next session through the proxy, so without the
  proxy (`--no-proxy`) the effort stays as configured. Tested with the fake
  harness only. TensorFold ended a reply cut inside a tool call's
  arguments with `finish_reason: tool_calls` at 16384 of 16384 tokens, and
  fx reported `InvalidToolArguments`; the proxy now reports any reply that
  reaches the request's output limit (16384 when the request does not say)
  as `length`. Tested against a fake engine only: no reply reached the
  limit in the run since.
  Each fx session's reasoning text is kept in
  `.tenaz/private/<label>.thinking.md` to show what such a reply was
  doing; fx 0.0.14 sends it as ACP thought chunks, seen in the
  image-landing runs of 2026-10-25.
- `verify`'s time limit kills the process but not grandchildren it left; a
  tool call that forks without limit is not stopped by the runner.
- The bootstrap runs in three steps, one session each (ADR 0003): the
  spec and map, then a minimal `verify`, then the task cards. A
  model that writes more than its step has the later steps passed by
  their checks. Run once with fx and Qwen3.8 27B on TensorFold
  (image-landing, 2026-10-25): three sessions, 13 minutes, each step
  passed on its first attempt. Not yet run with Pi or on an existing
  project.
- `maxBootstrapAttempts` applies to each bootstrap step. A pause during a bootstrap session counts as one of its attempts. An
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
- **Install script and Homebrew formula.** Both installed `tenaz` 0.1.0
  from the published v0.1.0 release on macOS (Apple silicon): the install
  script into a scratch folder, `brew install aartiles/tap/tenaz` into
  Homebrew. Neither has been tried on Linux. The install script skips the
  checksum check on macOS, where there is no `sha256sum`. The job that
  pushes the formula needs `HOMEBREW_TAP_TOKEN` to have write access to
  `aartiles/homebrew-tap` (Contents: read and write on that repository).
- **`tenaz doctor`'s advice** has been checked on macOS with Sushi running
  and with nothing on `PATH`. The install commands it gives for Pi and fx
  come from their READMEs and were not run; the context settings it gives
  for LM Studio, Ollama, llama.cpp and vLLM were not tried against those
  engines.

## Model and engine behaviour seen in testing

These are not runner bugs, but they decide whether a run gets far:

- **Reasoning-only answers.** With thinking at `medium` or `high`, the
  tested model often ended a bootstrap turn with reasoning only and no
  tool call. The runner retries the bootstrap, but a run can block there.
- **Drafting everything before writing.** Qwen3.8 27B on TensorFold, given
  the whole bootstrap in one session, drafted every file in its reasoning
  and hit the 16384-token output limit before writing any, even at low
  effort; the bootstrap in steps (ADR 0003) is the answer to that. It
  still reasons long in reviews: in the image-landing run the reviewers
  took about 88 minutes against 106 for the builders.
- **Steered reviewers and MINOR findings.** In the same run, reviewers
  steered for context wrote handoff notes they may not write, and one
  asked for changes over a MINOR finding. Steers now ask each role for
  the file it owns, and a `CHANGES` with MINOR findings only is accepted;
  neither has been run against a real model yet.
- **Lost tool arguments.** The engine sometimes streamed `{}` as a tool
  call's arguments; the harness rejects them and the watchdog stops the
  session.
- **Engines that report draft acceptance** in llama.cpp's
  `draft_n` / `draft_n_accepted` layout get it in `requests.jsonl` and
  `tenaz status`. Sushi 1.2.4 reports it only in its log.
