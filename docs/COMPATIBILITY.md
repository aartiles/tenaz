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
| Harness | [fx](https://github.com/vercel-labs/fx) 0.0.14 and 0.0.15 over ACP | Tested end to end: preflight, bootstrap, build, review, steering, pause and resume. |
| | [Pi](https://github.com/earendil-works/pi) 1.1.0 and 1.4.0 in RPC mode | Tested end to end to a COMPLETE run: 1.1.0 with the user's own `models.json`, 1.4.0 with the generated configuration through the proxy and two models (see Known gaps). No media tools (see below). |
| | [OpenCode](https://github.com/sst/opencode) 1.19.5 (`--harness opencode`) and [Qwen Code](https://github.com/QwenLM/qwen-code) 0.26.6 (`--harness qwen`), both over ACP | One real run each with Qwen3.8-27B on mlx-serve: OpenCode to COMPLETE, Qwen Code to BLOCKED by a replan the run rejected, not by the agent; Tenaz now accepts that replan (ADR 0018). Less tried than fx and Pi (see Known gaps). |
| | Other ACP agents (`--harness acp`) | The command `--acp-bin` names, with `--acp-args`, told about each session in the file `TENAZ_AGENT_SESSION` names. Tested with unit tests only; no real agent run through it yet. |
| | [DeepSeek Harness](https://www.npmjs.com/package/@deepseek-ai/dsh) 0.2.2-rc.1 (`--harness dsh`) over ACP | Its patch checked with `dsh --profile acp --dump-config` in a fresh `DSH_HOME`, and its ACP behaviour by a probe through the fx path on 2026-11-07 (preflight with Qwen3.8-Flash-Next on Sushi 1.5.1). Two runs to COMPLETE with Qwen3.8-Flash-Next on Sushi 1.5.1: a small goal on 2026-11-07 (17 sessions) and the raycaster game brief on 2026-11-08 (19 h 20 min, 60 sessions, 19 tasks), preflight `tools=true` in both (see Known gaps). |
| Engine | Sushi 1.2.4 and 1.3.0, and mlx-serve 26.10.3 (`--engine load-api`) | Tested for inference. Sushi 1.3.0 unloaded and reloaded the LLM through its API for each of 10 images in one run (about 8 s per reload). mlx-serve 26.10.3 loaded and unloaded two models through its API in two runs that swapped them (see Known gaps). |
| | Any OpenAI-compatible server (`--engine generic` with `--engine-start` / `--engine-stop`): llama.cpp `llama-server`, LM Studio, Ollama's `/v1`, llama-swap, vLLM | Tested against a mock, and with TensorFold native 1.1.4 stopped and started by its own scripts around each of 8 images in one run, before the start was skipped for an engine that still serves the model (see Known gaps). The servers named here are untried. |
| | MLX's `mlx_lm.server`, TensorFold (`--engine generic`) | Untested. `tenaz init` does not look for either: `mlx_lm.server` listens on 8080 by default, where the wizard expects llama.cpp, so it may be found under that name; otherwise pass `--base-url`. TensorFold needs the proxy's argument repair (see below). |
| Model | Any model the engine serves, with tool calling | Tested with `Qwen3.8-Flash-Next-Sushi-2.6bpw`. |
| Media | ComfyUI images (`generate_image` over MCP) | Tested against a mock, and in real runs with FLUX models next to TensorFold and Sushi (8 and 10 images). Needs an API-format workflow with `{{prompt}}`, `{{negative}}`, `{{seed}}`, `{{width}}` and `{{height}}` placeholders. |
| | OpenAI Images API (`--images`, `POST /v1/images/generations`): mlx-serve, Rapid-MLX, mlx-omni-server, LocalAI, Atomic Chat and others | Tested against a mock, and by hand against mlx-serve 26.10.3 (see Known gaps). Sends `model`, `prompt`, `size`, `n` and `response_format: b64_json`, plus `seed` and `negative_prompt`, which servers that do not know them may ignore. Frees the image model after a job through `POST /v1/unload-model` where the server has it (mlx-serve), also on the LLM's own origin, where the LLM stays loaded. |
| | Image-to-3D API (`--meshes`, `POST /v1/3d/generations`): mlx-serve 26.11.1 | Tested against a mock, and by hand through the adapter against mlx-serve 26.11.1 with Idol and TRELLIS.2 at 512 (see Known gaps). Sends `model`, the picture as base64 `image`, and `resolution`, `seed`, `steps` and `mode` when given. Frees the 3D model after each job through `POST /v1/unload-model`. |
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

When the engine cannot take the preflight's requests (it runs out of
memory, answers 502, 503 or 504, or is not reached), the checks did not
measure the model: the run stops with the engine's error, whatever
`--allow-weak-engine` says (ADR 0016), and `run.json`'s `preflight` keeps
the errors in `engine_errors` (absent when there were none). Free what
the engine needs (out of memory: free memory or reload the model) and
run again. Other engine errors (a refused image, a 400 or 500 that is
not about memory) are left to the checks.

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
  holding a `models.json` with the run's models, pointed at the proxy,
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

- **Shell reads count for the idle rule from text alone** (ADR 0029). The
  words of a command that look like paths are taken without a shell
  parse and without checking the disk, and a `cd` is not followed; the
  ADR lists what is missed (a quoted path, a dotfile, a name without an
  extension). Checked by replaying the four sessions the rule stopped in
  the 2026-11-08 raycaster run (dsh and Qwen3.8-Flash-Next on Sushi
  1.5.1) through the watchdog: at most 9 idle turns where they had 20. Not yet seen in a real run, and not
  measured on whether a looping model now runs longer.

- **The generic ACP agent is untried.** `--harness acp` is covered by
  unit tests only; no real agent has run through it. Its command must
  answer `--version`, and Tenaz isolates nothing: the agent keeps the
  user's `HOME`, settings, skills and memory, and gets the pinned skills
  only if the user's script gives them. In a sandboxed run it can write
  only where the sandbox allows, so an agent that keeps its settings or
  sessions under `~` needs that folder in `sandboxAllowWrite`.

- **DeepSeek Harness has finished two runs.** `--harness dsh` is built
  from a probe on 2026-11-07 (dsh 0.2.2-rc.1, ACP protocol 1, agent
  `deepseek-harness-acp`) and unit tests. With Qwen3.8-Flash-Next on
  Sushi 1.5.1 it ran a one-file shell goal to COMPLETE on 2026-11-07
  (17 sessions, six tasks, a replan) and the raycaster game brief on
  2026-11-08 (19 h 20 min, 60 sessions, 19 tasks, two final reviews).
  The second run changed builds and settings during pauses, so it is
  evidence that dsh works through a long run, not a clean measurement.
  It showed: dsh reads images with `read_image` only when the run has
  `--vision` (without it, it refused all 16 reads); dsh summarizes its
  own context in a separate request (71 times), and the request after a
  summary lost most of its cached prefix in 61 of 70 cases. Tenaz's
  patch now turns that compaction off and the proxy refuses a summary
  request that still comes (ADR 0030); the patch was checked with
  `dsh --profile acp --dump-config`, the refusal only against dsh's
  source, and no run has used either yet. dsh now gets the user's
  Playwright and npm caches: in a 0.6.0 raycaster run it installed a
  browser under its own `HOME`, where `verify` did not find it. A
  sandboxed run writes them only while `~/Library/Caches` (or
  `~/.cache`) and `~/.npm` stay in `sandboxAllowWrite`, or the user's
  own paths are added there. A `cache` set in `~/.npmrc` is not read:
  dsh gets `~/.npm` while `verify` uses that cache. The other agents
  with a `HOME` of their own (fx, OpenCode, Qwen Code) do not get these
  caches yet (OpenCode gets npm's). No run has used this yet. What the
  probe showed: dsh did not send permission requests, so its tools run
  without the runner's approval and the watchdog sees a call only once
  it runs; its `usage_update` has no cache counts; it sends its answer after the
  model request ends (kept since the late-text fix, which logged 51
  `late_text` events in the raycaster run); web search needs a DeepSeek
  key; its system prompt is about 5,700 tokens. The media MCP server
  and the pinned skills are not given to it, and whether a fresh
  `DSH_HOME` needs the network on first start is not checked. The wizard
  and `tenaz doctor` do not detect it: a `dsh` on PATH may be the
  distributed shell.

- **A refused request continues in a fresh session.** A 400, 413, 415
  or 422 from the engine, other than an image, a prompt too long or the
  engine's memory, is journaled as `refused-continuation` and a fresh
  session goes on with a note (ADR 0025). A refusal that the fresh
  session meets again, such as a template or tool-schema error, uses up
  the continuations and is then retried as an engine failure, as before.
  Tested with unit tests only, not yet in a run.
- **A video file sent as a note** (ADR 0024). Qwen Code 0.26.6 sends a
  video it read as a `data:video/mp4` part, which Sushi refuses; the
  proxy puts a note in its place. Seen once in the articulated-cabinet
  run with Qwen3.8-Flash-Next on Sushi (2026-11-06): the reviewer of
  T014 read `outputs/motion.mp4`, got no 400, and finished in 20 turns,
  where the seven before it had each got a 400 on that video. Other agents
  and engines are not checked.
- **Earlier reasoning dropped by default** (ADR 0023). Until ADR 0028
  the proxy sent only the newest assistant message's reasoning; it now
  removes the earlier reasoning in steps (next item). `--set
  keepPastReasoning=true` sends it all. In the articulated-cabinet run
  with Qwen Code 0.26.6 and Qwen3.8-Flash-Next on Sushi (2026-11-05),
  two builders of T027 peaked at 45k and 51k tokens after 32 and 24
  turns, where most builders before it reached the 98k stop after 16 to
  56 turns.
  That is one task, not a measurement: how much longer a session lasts,
  and whether the model redoes work it reasoned out before, is not
  measured.
- **Earlier reasoning removed in steps** (ADR 0028). Removing it every
  turn cut the engine's cached prefix on every request: in the raycaster
  run (2026-11-08, dsh and Qwen3.8-Flash-Next on Sushi 1.5.1, its first
  679 requests) 83.7% of prompt tokens were cached and the median time to
  first token was 3.48 s, against about 95.7% and 2.2 s in three Sushi
  runs before ADR 0023. The proxy now keeps up to `pastReasoningTokens`
  (8192) of it, then removes all of it except the newest message's, in
  one step. A
  replay of that run's first 713 requests puts the cache at 90.3%, against
  84.9% for ADR 0023 in the same model; the replay models the engine's
  cache. Seen in the same run after the change: sessions 035-041 and
  046-060 (644 requests, one prefix-cache entry) cached 88.2% and
  90.8%, with a 1.46 s median time to first token in both, against
  84.5% and 3.58 s before it; sessions 042-045 ran on a Sushi with two
  entries and cached 84.3%. The parts had different tasks, so this is
  not a controlled comparison; whether 8192 suits other models and
  engines is not checked.
- **Qwen Code's compaction refused** (ADR 0022). The proxy knows Qwen
  Code's summarizing request by the opening of its prompt in 0.26.6. In
  the articulated-cabinet run (2026-11-03 and 04) it compacted 25 times
  before this, each wasted. Seen in the same run from 2026-11-04: the
  proxy refused Qwen Code's request 60 times, and each builder went on
  in a fresh session. A new Qwen Code version must be checked for
  that prompt.
- **A blocked task reopened behind an amend** (ADR 0018). In a run with
  Qwen Code 0.26.6 and Qwen3.8-27B (2026-11-03) a replan set a blocked
  task back to `todo` behind the card that amends its conflict's check;
  Tenaz rejected it twice and the run ended BLOCKED. Tenaz now accepts
  that backlog. Tested with the scripted harness only, not yet in a run
  with a real model. In the articulated-cabinet run with Qwen3.8-Flash-Next
  (2026-11-05) a replan reopened a task with no amending card, Tenaz
  blocked it again in silence, and the run later blocked with no task
  ready. Tenaz now tells the replan before it ends. Seen once:
  after `tenaz run --resume`, the next replan was told, dropped the task
  and gave its work to a new card, which was built in one attempt. The
  replan itself ran out of turns; its cards were kept.
- **`--resume` after no task was ready.** A run blocked with no task
  ready gets one more replan on `--resume`. Tested with a
  scripted-harness scenario, and seen once in that run (2026-11-05):
  the replan ran, hit its turn budget, and the run went on.
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
  when the run's model can. In that run one `diffLimitBytes` served every
  reviewer, whatever its window: the reviewer's first request was 30245
  tokens of its 42598 budget, and the diff was half of Tenaz's prompt
  (40.6k of 79.4k characters). The diff now takes `diffShare` of the
  reviewer's own budget (ADR 0012), tested with the scripted harness; no
  real run has used it yet. With Pi, each session's Pi process is
  launched with its role's model, and the run's `models.json` lists every
  model of the map; a role's window and inputs come from the user's
  `~/.pi/agent/models.json` when it lists the model. Run once with Pi
  1.4.0 and two models on mlx-serve 26.10.3, swapped (2026-11-02, see
  below): Pi's own session files name the builder's model for the
  builder's sessions and the run's model for the others, and mlx-serve's
  log shows no load Tenaz did not ask for. Not run with a role's model
  listed in the user's `models.json` under the run's provider.

- **Swapping the role models** (`--swap-models`, ADR 0015). Run twice on
  2026-11-02 with mlx-serve 26.10.3 on a 64 GB Mac, with Flint int4
  and Qwen3.8-27B 4-bit (21 GB and 17 GB resident, as mlx-serve reports
  them), on a one-page goal for a small Python package:
  - fx 0.0.15, Qwen3.8-27B for the run and Flint as the builder: 11
    sessions, paused by hand during the eleventh, a reviewer. The check
    at the start loaded each model in turn (Flint 3.5 s, Qwen 6.1 s) and
    left Qwen resident; four swaps went through, before the first
    builder, the split, the first builder after it and a reviewer. Flint
    failed its task three times, and the task was split; the first new
    task took three builder sessions too.
  - Pi 1.4.0, Flint for the run and Qwen3.8-27B as the builder: COMPLETE
    in 28 minutes and 8 sessions, both tasks done at the first try.
    Four swaps went through, one before each builder and reviewer; the
    final review needed none.

  An unload took 37 to 84 ms, a load of Flint 2.1 to 3.5 s and of Qwen
  3.7 to 6.1 s. No builder after a builder swapped, no run journaled
  `evicted`, memory pressure stayed normal, and mlx-serve refused no
  prompt for memory. Not run against Sushi, which served neither model
  here; whether Sushi loads a model by itself when a request names one
  that is not loaded is not known. Not run with a model that barely
  fits alone either: Qwen3.8-Flash loaded in mlx-serve only with
  `--skip-mem-preflight`, and mlx-serve then refused the preflight's
  three prompts for memory (see the next entry). A swap that fails,
  and a stop during a swap, are tested with fake engines only. Only the
  load-api engine swaps; a generic engine is refused, since its stop
  ends every model.

- **The preflight and an engine's refusal.** On 2026-11-02 mlx-serve
  26.10.3 refused the three preflight prompts for memory (HTTP 503) under
  Pi 1.4.0, with Qwen3.8-Flash loaded through `--skip-mem-preflight`.
  The run stopped saying there was no reasoning, no reused prefix and
  no tool call, advised another model, and pointed to
  `.tenaz/private/preflight.*`, which Pi never writes. The preflight now
  stops with the engine's error when the engine ran out of memory,
  answered 502, 503 or 504, or was not reached, and names only files the
  harness writes. Tested with the scripted harness and fakes only: not yet
  run against a refusing engine with fx or Pi. That Pi leads a 503 with
  `503: ` is assumed from its 400s; a Pi message without the status is
  still told by its memory wording, but a busy engine is not. With fx,
  an engine error that came with an error status now starts with it
  wherever a session records it; that too is tested against the proxy's
  fake engine only.

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
  reports the error when the chunk arrives, before fx can exit on it,
  whether the engine ends its events with `\n\n` or `\r\n\r\n`.
  Only that shape, an `error` object or string in a `data:` chunk, is
  read; how llama.cpp, vLLM or Ollama send an error inside a stream has
  not been checked. The same chunk in the body of an error status gives
  that error's message after the status; no engine has been seen to send
  one. With Pi, an out-of-memory error is tested only with the scripted
  harness's own wording; no real one has been seen.
- **A prompt the engine cannot fit.** mlx-serve 26.10.3 refuses a request
  whose prefill does not fit in free GPU memory ("This prompt (30245
  tokens) does not fit in GPU memory even after freeing the prefix cache
  … It was refused before any work started."), which fx reports as
  `ProviderError`; fx refuses a prompt it cannot compact into the model's
  window with `ContextCapacityExceeded`. Tenaz reads the first as the
  engine running out of memory (ADR 0012), and still sends the same
  prompt again after each wait, up to `engineRetries`, then stops the run
  as engine unavailable, saying to free memory or reload the model. The
  matcher is tested with the recorded message, not yet against mlx-serve
  itself; the error body around it (`error.message`) is assumed, not
  captured, and so is whether it comes as an HTTP error or inside the
  stream. fx's refusal before any reply blocks the run at once (`prompt
  too large:`), since the same prompt would be refused again; this is
  tested with the scripted harness and the stderr fx left in that run,
  not yet against fx itself. Seen twice in the Digger 3D run of
  2026-10-29, seven sessions each time: reviewers on a 281k-character
  prompt that fx refused, then, with the lists of changed files clipped,
  reviewers that mlx-serve refused at 39468 tokens after three turns and
  then at 30245 tokens, before reading a file. The same refusal also came
  mid-session in two earlier reviews: one session finished anyway, and
  the other's rerun ran to the end. Before each session Tenaz now
  estimates the first request (3 bytes per token plus
  `harnessReserveTokens`) and blocks when it is over the role's budget.
  The 8192-token reserve is fx 0.0.15's measured 6750 rounded up; Pi's
  own prompt and tools were not measured, and a newer fx may add more.
  The check is tested with the scripted harness only, not yet in a run
  against fx or mlx-serve.
- **An engine that does not report cached tokens** (ADR 0007). Tested with
  recorded usage shapes, LM Studio's among them, and the fake harness; the
  preflight has not yet been run against LM Studio with this change.

- **A tool call over `toolMinutes`.** Tenaz aborts the turn and prompts
  the same session again (ADR 0004). Tested with the fake harness only:
  that Pi takes a prompt after `abort` with its context, and that fx stays
  usable after `session/cancel`, have not been run against the real tools.
  An ACP call announced as pending is timed from when it is about to run:
  its permission request or `in_progress`. An agent that sends neither
  for a call reports it only when it ends, so `toolMinutes` cannot stop
  it, and an agent that exits during such a call is taken for an engine
  failure rather than a crash in the call. Seen by hand with a minimal
  ACP client, not recorded: fx 0.0.16 (newer than 0.0.14 and 0.0.15,
  the versions in the table above) in the `ask` mode Tenaz sets asks
  permission for a shell call, and OpenCode 1.19.5 and Qwen Code 0.26.6
  send `in_progress` with the call's input. A call announced while the
  reply streams now usually starts after that reply's turn ends, so its
  time and its loop check fall in the next turn. An update or an end for
  a call the agent never announced with `tool_call` is not reported at
  all.
- **Streamed replies through the proxy.** The proxy decodes a streamed
  reply one whole event at a time, so a character the network splits
  between two chunks arrives intact; that is tested with unit tests, not
  seen against a real engine. An event ends at a blank line, its lines
  ending with `\n` or `\r\n`; a bare `\r` does not end a line. A stream
  passes as the engine sent it, unless the request offers a tool whose
  arguments the proxy repairs and the engine answers with a success
  status: then each event it sends ends with `\n\n`. An event the
  repair passes on keeps its own lines; one it rewrites becomes a single
  `data:` line. `\r\n` framing is tested with unit tests, and the error
  report also with the fake engine; no engine has been seen to send it.
- **The pause after a capacity error** (`capacityRetryWaitSec`), and a
  stop during it, are tested with the fake harness only, not against an
  engine that runs out of memory.
- **A paused session's continuation** is kept through reruns and blocks
  until a session that continues it is done; that is tested with the
  scripted harness only, for builder sessions, not with fx or Pi. The
  flag does not say which session it is owed to: the first session after
  the pause gets the note, whatever its role or task; only a paused task
  review is taken back without it. The note says the session ran out of
  context even when a pause stopped it.
- **A split child correcting its ancestors' notes** (ADR 0013) is tested
  as an ownership rule only. No run has had a builder keep such an edit
  yet; the Digger below the Mine run of 2026-10-30, whose T021dd asked for
  one, ran a binary from before the change.
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
  hold covers them, but Pi gets no media tools (it runs with
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
- **mlx-serve and 3D** (ADR 0020). Checked by hand on 2026-11-03 against
  mlx-serve 26.11.1 on a spare port, with Tenaz's `MeshApi` adapter and a
  512×512 drawn cutout of a cabinet: Idol (seed 7, `mode: synthetic`)
  wrote a 27 MB GLB in 76 to 80 s; TRELLIS.2 at 512 (seed 7) wrote a
  10 MB GLB (300000 faces) in 50 s. Both files start with the `glTF`
  magic. mlx-serve keeps a 3D model loaded after its job: with Idol still
  resident (4 GB), TRELLIS.2 was refused with `503 … out_of_memory`,
  since its free-memory check asks for about 26 GB even at 512, and
  worked once Idol was unloaded (`POST /v1/unload-model`). In the first
  real run TRELLIS.2 stayed loaded for hours after its job, next to the
  LLM, so Tenaz now unloads the 3D model (and an `--images` model) after
  each job. Checked by hand on 2026-11-04 through the adapters against
  mlx-serve 26.11.1 on a spare port with no model loaded:
  `/v1/unload-model` answers 200 `state: unloaded` for a known model and
  404 `model_not_found` for an unknown one, which Tenaz journals as a
  failed unload. Freeing a loaded TRELLIS.2 through it was seen by the
  operator (HTTP 200), not through Tenaz: a run was using the memory.
  The unload is not checked against `--images` servers other than
  mlx-serve: one that answers an unknown route with something other than
  404, 405 or 501 gets a `failed unload` in the journal after each job
  (the job is kept).
  Not verified: a `generate_3d` call from an agent in a real run (the MCP
  tool is tested only against fakes), TRELLIS.2 at 1024 or 1536 (about
  6.7 minutes at 1024 measured outside Tenaz), Hunyuan3D and Pixal3D,
  and a run whose LLM is on the same mlx-serve. The tool refuses a picture
  that is not a PNG or JPEG, and paths that leave the project, also
  through a symbolic link, or go into `.git` or `.tenaz`; these checks run
  before the call waits for the lease, so a file or link swapped in
  between is not seen (the agent's own shell can reach as much).
- **A capacity ceiling tried again** (ADR 0014). Covered by unit tests of
  the ledger only. No run has yet held the ceiling for twice the wait and
  raised the budget past it, with an engine that held the raise or
  overflowed it.
- **A peak the engine held raises the cap** (ADR 0026), journaled as
  `context-cap-overrun`. Seen once, in the
  articulated-cabinet run resumed on 6 November: from 44942 the cap rose
  session by session to 82232 within a night, with no engine error. The
  first two replans after the resume were still cut. Only seen
  with Qwen Code on Sushi.
- **Release attestations.** The release workflow attests each archive
  (`actions/attest`, generated by cargo-dist 0.32); `gh attestation
  verify` passed for both archives of v0.3.0. Only the archives for each
  platform and their `.sha256` files are attested; the installer script,
  the Homebrew formula, the source tarball and `sha256.sum` are not. The
  installer and the formula download attested archives.
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
  passed on its first attempt. On 2026-11-02, in the two runs that
  swapped role models (fx 0.0.15 with Qwen3.8-27B, Pi 1.4.0 with Flint
  int4, on mlx-serve 26.10.3; see Known gaps), it ran on an existing
  project, a small Python package: three sessions each, every step
  passed on its first attempt.
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
  confirmation. The existing-project mode (`.tenaz/verify` and the
  bootstrap section) ran with fx 0.0.15 and Pi 1.4.0 on a small Python
  package (2026-11-02, see the bootstrap above); not yet on a large
  repository.
- Whether fx loads the project's `AGENTS.md` by itself has not been
  checked; Pi does. The Tenaz protocol is in every prompt either way.
- **OpenCode and Qwen Code: the runs** (`--harness opencode`,
  `--harness qwen`, ADR 0019). Each ran once on the same word-count brief
  with Qwen3.8-27B on mlx-serve. OpenCode 1.19.5 reached COMPLETE in 19
  sessions (six tasks). Qwen Code 0.26.6 finished two of seven tasks in
  11 sessions, then stopped BLOCKED: its builder declared a conflict with
  a done check, and both replans wrote the amending card but set the
  blocked card back to `todo` instead of `dropped`, and Tenaz rejected
  that backlog. Tenaz now accepts it (ADR 0018); this run has not been
  repeated. Before these kinds existed, each agent reached COMPLETE
  through a script given as `--fx-bin`
  (OpenCode: 18 sessions, Qwen Code: 21). Most sessions that failed in
  these runs were mlx-serve running out of memory, which Tenaz ran
  again; Qwen Code's last replan reached the output limit. After the
  runs, Qwen Code's `HOME` held a process registry and undo copies of
  edited files, but no conversations or memories.
- **Qwen Code: reads and writes.** A run with Qwen3.8 Flash on Sushi
  (2026-11-03) stopped a session that had read 20 new files with
  `ReadFile` as idle, and journaled `writes: 0` for every session: the
  watchdog knew only Pi's and fx's tool names. It now takes the effect
  from ACP's `kind` and knows Qwen Code's names (ADR 0021). Checked
  against Qwen Code 0.26.6's sources (its tool names and that it sends
  `kind`) and unit tests; no run has used it yet. OpenCode's `kind` was
  not checked: no OpenCode session recorded it.
- **OpenCode and Qwen Code: not checked.** Media tools over MCP and their
  time limit (OpenCode's MCP `timeout` may only bound listing the tools),
  pinned skills, `--model-for`, pause and resume, a run in the OS sandbox,
  and whether Qwen Code knows the model reads images (its settings do not
  say). Tenaz removes the context files Qwen Code reads from its `HOME`
  before each session; no run has written one. Neither agent takes a
  steering message: Tenaz steers only fx and Pi, so a watchdog stops their
  sessions without a warning. OpenCode installs its plugin package (about
  60 MB) into its `HOME` on the first run, from npm's cache in `~/.npm`: a
  `sandboxAllowWrite` without `~/.npm` should break it. Qwen Code starts
  with a system prompt of about 21k tokens (OpenCode about 7k), so a small
  window fills sooner.
- **Install script and Homebrew formula.** Both installed `tenaz` 0.1.0
  from the published v0.1.0 release on macOS (Apple silicon): the install
  script into a scratch folder, `brew install aartiles/tap/tenaz` into
  Homebrew. Neither has been tried on Linux. The install script skips the
  checksum check on macOS, where there is no `sha256sum`. The job that
  pushes the formula needs `HOMEBREW_TAP_TOKEN` to have write access to
  `aartiles/homebrew-tap` (Contents: read and write on that repository).
- **`tenaz doctor`'s advice** has been checked on macOS with Sushi running
  and with nothing on `PATH`. The install commands it gives for Pi, fx,
  OpenCode and Qwen Code come from their READMEs and were not run; the
  context settings it gives for LM Studio, Ollama, llama.cpp and vLLM were
  not tried against those engines.

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
