# ADR 0010: Heavy jobs between sessions

Status: proposed, 2026-10-29. Not implemented.

## Context

In the Lotion Pump run (2026-10-28 and 29, Blender 5.2 on Metal, Sushi
1.4.0 with Qwen3.8 Flash Next 2.6bpw, fx 0.0.15) the last task, T017, had
to render the 180 frames of the turntable again: about 25 minutes of
Cycles on the GPU. The model could not do it:

- A tool call has 10 minutes (`toolMinutes`, ADR 0004), so the render had
  to run in the background while the session went on asking the engine
  for turns.
- Sushi keeps about 48 GiB resident. With Blender rendering next to it,
  Blender failed with `Insufficient Memory` and Sushi cut requests with
  `GenerationOutOfMemory` (14 times over the run); fx then exited.
- Tenaz reran the session six times without charging an attempt, waiting
  up to 900 s, and after seven engine failures in a row blocked the run as
  "engine unavailable".
- The model saw the problem and wrote `render_loop.sh`, a render that
  resumes after a crash. It could not avoid the clash, which happens inside
  its own session.

The operator ran that script with the run blocked, when no request reached
the engine. It finished without a memory error, and the run was resumed to
verify and close T017. The work stayed the model's; only the moment was
the operator's.

Tenaz already knows how to give the GPU to something else: the lease
(ADR 0002, `crates/core/src/lease.rs`) closes the proxy's gate so LLM
requests wait, unloads the LLM where the engine can, runs a media job,
and loads the LLM again. It is used only for images, through the
`generate_image` tool.

Blender is one case. A training run, a large build, a long FFmpeg encode
or a game engine's bake would meet the same two walls: the tool time limit
and the memory the engine holds.

## Options

### A. Leave it

- An operator does by hand what the Lotion Pump operator did, and writes
  it down as an intervention.
- An unattended run blocks on the first job of this kind.

### B. A tool per program, as for images (`render_blender`)

- Tenaz runs the program under the lease, as it runs ComfyUI.
- Each program needs its adapter, its options and its tests; Tenaz learns
  Blender, then the next one.

### C. A generic tool that runs a command under the lease, in the session

- `run_heavy_job(command)` over MCP: the gate closes, the LLM is unloaded,
  the command runs, the LLM comes back, the call returns.
- The session waits inside one tool call for as long as the job takes, past
  `toolMinutes` and the watchdog's idle rules, and fx must keep that call
  open. A pause or a crash during the job loses the session.

### D. A job the builder declares, run between sessions

- The builder writes `.tenaz/jobs/<name>.json`: the command, why it is
  heavy, and its time limit. It stops.
- Tenaz validates it (the command runs from the project, under the sandbox
  when the run has one, with a time limit), then runs it with the lease:
  the gate closed and the LLM unloaded where the engine can. Its log goes
  to `.tenaz/private/jobs/<name>.log`.
- The next session of the same attempt starts with the result: exit code,
  duration and the log's tail. Like an engine rerun, the job does not
  charge an attempt; the attempt's builder sessions still count toward
  its limits.
- A pause during the job is resumed by running it again: the job must be
  safe to rerun, as `render_loop.sh` was. The protocol says so.
- It is ADR 0009's shape: a file the builder writes, which Tenaz checks
  and acts on.

## Recommendation

**D.** It does what the operator did, at the moment the operator chose,
with the parts Tenaz already has. With it, after an engine
out-of-memory (classified since `357cb91`) the rerun session gets a note:
the engine ran out of memory while other work used the GPU; do not leave
heavy work in the background, declare a job.

## Open questions

- **Engines that cannot unload.** With `--engine generic` and no
  stop/start scripts, the LLM stays resident and the job runs next to it,
  as a media job does. That may still fail for memory; the gate at least
  removes the requests.
- **How long a job may run.** A job limit separate from `toolMinutes`, with
  a default (an hour?) and a parameter.
- **`verify` itself.** Tenaz runs `verify --all` between sessions too, with
  the LLM resident. If a project's checks render (Lotion Pump's stills),
  they can clash the same way. Whether `verify` should run under the lease
  as well, always or by a parameter, is left open.
- **Benchmarks.** A job is run by Tenaz, not by the model, but the model
  writes and asks for it, as with a tool call. Runs that use jobs should
  say so in their report.

## Consequences if D is taken

- New file and role scope (`.tenaz/jobs/<TASK>-<name>.json`, writable by
  the builder of its task), a validator, a lease method for a command job,
  journal records (`job-started`, `job-finished` with exit code and
  duration), the rerun note, and the protocol, builder prompt, SKILL.md,
  README and COMPATIBILITY updated.
- Tested with the scripted harness and a fake engine adapter; whether a
  small model declares jobs at the right times is unknown until a run.
