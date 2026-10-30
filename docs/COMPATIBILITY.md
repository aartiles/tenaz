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
| | Any OpenAI-compatible server (`--engine generic` with `--engine-start` / `--engine-stop`): llama.cpp `llama-server`, LM Studio, Ollama's `/v1`, llama-swap, vLLM | Tested against a mock, and with TensorFold native 1.1.4 stopped and started by its own scripts around each of 8 images in one run, before the start was skipped for an engine that still serves the model (see Known gaps). The servers named here are untried. |
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
  Same flag. An engine whose usage never names its cached tokens (LM
  Studio 0.4) cannot be checked: the preflight passes with a warning, and
  `tenaz status` shows its cache as "not reported" (ADR 0007). With
  `--no-proxy` Tenaz cannot see that, and such an engine fails.
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
- fx's `settings.json` is written again before each session, for that
  session's model.
- Skills: by default Pi sees only the skills pinned with `init --skill`,
  and none without them: it gets `--no-skills` plus each pinned skill.
  fx's skill catalog gets a 0-byte budget, so its prompt lists no skills,
  but see the known gap below; pinned skills are linked into its isolated
  `HOME`, and with them fx lists the skills of the folders above the
  project again. `--set agentSkills=true` lets the agent load the skills
  it finds by itself.

## Known gaps

- **A model per role** (`--model-for`, ADR 0011). fx is launched with each
  session's model: its `settings.json` is rewritten before the session
  starts. Run once with fx 0.0.15 and mlx-serve 26.10.3 holding two
  models on a 64 GB Mac (2026-10-29, stopped blocked at T003): a Ternary
  Bonsai 2 27B (2-bit) builder with a 131072 window, and Qwen3.8-27B for
  the other roles with 65536. In 31 sessions the journal names each
  role's model, with a budget of 85196 tokens for the builder and 42598
  for the rest. An earlier try with Flint int4 as the builder did not fit
  beside Qwen3.8-27B and mlx-serve's prefix cache, and mlx-serve once
  unloaded Qwen to load Flint (seen by the operator, not journaled). Not
  checked whether fx writes `settings.json` itself. The check that every
  model is loaded passed against that mlx-serve; the lease freeing and
  reloading them around an image is tested with fake engines and a
  stand-in server for the generic engine's commands, not against Kapitsa,
  mlx-serve or a generic engine holding two models; so is a heavy job,
  which goes through the same unload and reload. TensorFold's scripts
  have not been run again since a generic engine that still serves its
  model after an image is no longer started. The preflight's probes
  (reasoning, cache, a tool call, an image) run on `--model` only, so a
  role's model is never probed; and every role's model takes the run's
  `--vision`, so a reviewer that cannot read images is still sent them
  when the run's model can. Likewise one `diffLimitBytes` serves every
  reviewer, whatever its window: in that run the reviewer's first request
  was 30245 tokens of its 42598 budget, and the diff was half of Tenaz's
  prompt (40.6k of 79.4k characters). Not with Pi: `init` refuses the
  map, and Pi refuses a session on another model than the one it was set
  up for.

- **Heavy jobs** (ADR 0010). Tested with the scripted harness and a fake
  job runner; the lease cycle with fake engines, and whole runs with a
  lease over a fake LLM whose reload after the job fails or that cannot
  unload; the job runner itself with `sh`. No real model has declared a job yet, so whether a
  small model declares one at the right moment, rather than running the
  work in the background, is unknown. With an engine that cannot unload
  (`--engine generic` without stop/start scripts) the job runs next to the
  resident LLM, with only the request gate closed, and may still fail for
  memory. `verify` still runs with the LLM loaded, so checks that render
  can clash the same way. No job has yet run under the sandbox launcher
  (sandbox-exec, bwrap), with a GPU program, or around a real engine's
  unload and reload (Sushi). A crash of Tenaz during a job leaves the job
  running, in its own process group: stop it before resuming the run.

- **An engine error inside a stream** (Sushi's `GenerationOutOfMemory`),
  with fx; Pi reports engine errors itself. Its shape, `data: {"error":
  {"message":…},"choices":[{"finish_reason":"error"}]}`, was read from
  Sushi 1.4.0's binary, not captured from a real failure, and no real
  out-of-memory failure has gone through the proxy yet. If Sushi sends it
  otherwise, the session is rerun as before, as a harness exit. The proxy
  reports the error when the chunk arrives, before fx can exit on it.
  Only that shape, an `error` object or string in a `data:` chunk, is
  read; how llama.cpp, vLLM or Ollama send an error inside a stream has
  not been checked. With Pi, an out-of-memory error is tested only with
  the scripted harness's own wording; no real one has been seen.
- **A prompt the engine cannot fit.** mlx-serve 26.10.3 refuses a request
  whose prefill does not fit in free GPU memory ("This prompt (30245
  tokens) does not fit in GPU memory even after freeing the prefix cache
  … It was refused before any work started."), which fx reports as
  `ProviderError`; fx refuses a prompt it cannot compact into the model's
  window with `ContextCapacityExceeded`. Tenaz matches neither message: it
  journals the first as an engine error and the second as a harness exit,
  and sends the same prompt again after each wait, up to `engineRetries`,
  then stops the run as engine unavailable. Seen twice in the Digger 3D
  run of 2026-10-29, seven sessions each time: reviewers on a
  281k-character prompt that fx refused, then, with the lists of changed
  files clipped, reviewers that mlx-serve refused at 39468 tokens after
  three turns and then at 30245 tokens, before reading a file. The same
  refusal also came mid-session in two earlier reviews: one session
  finished anyway, and the other's rerun ran to the end.
- **An engine that does not report cached tokens** (ADR 0007). Tested with
  recorded usage shapes, LM Studio's among them, and the fake harness; the
  preflight has not yet been run against LM Studio with this change.

- **A tool call over `toolMinutes`.** Tenaz aborts the turn and prompts
  the same session again (ADR 0004). Tested with the fake harness only:
  that Pi takes a prompt after `abort` with its context, and that fx stays
  usable after `session/cancel`, have not been run against the real tools.
- **Streamed replies through the proxy.** The proxy decodes a streamed
  reply one whole event at a time, so a character the network splits
  between two chunks arrives intact; that is tested with unit tests, not
  seen against a real engine. Events are framed on a blank line (`\n\n`):
  an engine that ends them with `\r\n\r\n` would reach the harness only
  when the stream ends. No engine has been checked for that.
- **The pause after a capacity error** (`capacityRetryWaitSec`), and a
  stop during it, are tested with the fake harness only, not against an
  engine that runs out of memory.
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
- **Release attestations.** The release workflow attests each archive
  (`actions/attest`, generated by cargo-dist 0.32). It has not run yet:
  the first release after this change is the first that can be checked
  with `gh attestation verify`. Only the archives for each platform and
  their `.sha256` files are attested; the installer script, the Homebrew
  formula, the source tarball and `sha256.sum` are not. The installer and
  the formula download attested archives.
- **Media through fx with long jobs.** Which route fx takes for a
  10-minute tool call has not been measured.
- **Linux and NVIDIA memory.** Memory pressure, swap and the engine's
  footprint are sampled on macOS only.
- **The sandbox** (`sandbox=true`) has been tested on macOS, with `verify`
  and a Pi session against a local model, and with fx in the Lotion Pump
  run (2026-10-28, Blender 5.2 on Metal). Before that run, with the
  default write roots, Blender could not write Metal's shader cache in the
  per-user cache (`getconf DARWIN_USER_CACHE_DIR`) and every `./blender`
  call took about 10 s longer (a 256-pixel, 16-sample render: 11 s against
  0.9 s); the run added the directory to `sandboxAllowWrite`, which brought
  it back to 0.9 s. That directory is now a default write root on macOS,
  but Blender has not run with the new default yet.
  The Landlock version for Linux compiles and its rules are unit-tested,
  but it has not run on a Linux kernel yet.
- **Writes outside the project** without the sandbox (ADR 0008). Only the
  write tools' own path argument is checked (`path`, `file_path`); a shell
  command that writes elsewhere is not seen. Symbolic links are not
  followed. Tested with the scripted harness; not yet seen against Pi or fx
  with a real model.
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
- **The engine running out of memory part way through a reply.** In the
  Lotion Pump run (Sushi 1.4.0, fx 0.0.15, Blender rendering on the same
  GPU) seven sessions ended this way: Sushi logged `Command buffer
  execution failed: Insufficient Memory` and `streaming error:
  error.GenerationOutOfMemory`, and fx stopped at once (`ProviderError`,
  `saw_content=false`, no retry) and exited. The run log said `failed
  (toolUse)`. Each time the model had just started a render or a
  `verify` that renders, often left running in the background. The proxy
  now reads an error the engine sends inside a stream that started with
  status 200, and the session says `failed (engine out of memory)` and is
  rerun as an engine failure (journal `engine-retry` cause `engine`). See
  "Known gaps" for what is not verified.
- **Engines that report draft acceptance** in llama.cpp's
  `draft_n` / `draft_n_accepted` layout get it in `requests.jsonl` and
  `tenaz status`. Sushi 1.2.4 reports it only in its log.
