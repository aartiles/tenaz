# ADR 0015: Swapping the role models

Status: accepted (1A, 2A, 3A, 4A, 5A), 2026-11-01. Amends ADR 0011,
where every model of the map is resident at once.

## Context

ADR 0011 gave each role a model of its own, with every model of the map
resident at once, and deferred option C: swapping them through the lease
when they do not fit together. A machine now needs it.

Around the first run with a map (Digger 3D, 2026-10-29, fx 0.0.15 and
mlx-serve 26.10.3 on a 64 GB Mac; see COMPATIBILITY):

- In an earlier try, Flint int4 as the builder did not fit beside
  Qwen3.8-27B and mlx-serve's prefix cache, and mlx-serve once unloaded
  Qwen by itself to load Flint. The operator saw it; nothing journaled
  it. The run went on with a smaller builder, Ternary Bonsai 2 27B at 2
  bits.
- With both models resident, mlx-serve refused ten reviewer prompts for
  memory (ADR 0012). A model alone in memory has all of it for its prompt
  and its cache.

What engines do on their own: mlx-serve with "Max models loaded at once"
set to 1 (`--max-resident-models 1`) unloads one model when a request
names another, and loads the first again on the next request that names
it. It was checked by hand with an image model, the LLM back in 2 to 4 s
(COMPATIBILITY). llama-swap and LM Studio's JIT loading are documented to
do the same; neither has been tried. Whether Sushi does is not known.
Sushi 1.3.0 reloaded its LLM through its API in about 8 s for each of 10
images.

What Tenaz has today:

- The lease (`crates/core/src/lease.rs`) closes the proxy's gate, unloads
  the LLMs, lets a media job or a heavy job (ADR 0010) run, loads the LLMs
  again and opens the gate. It unloads and loads every model of the run
  together (`llm_models`), and journals each step as a `lease` record; a
  `load` or `unload` that works has no `detail`.
- At every `tenaz run` start, before the preflight, `engine_serving`
  loads what is not resident (`ensure_llm`) and refuses the run unless the
  engine lists every model of the run as loaded. After an engine error or
  a heavy job, `wait_for_engine` tries the same up to 30 times,
  `ENGINE_WAIT` (20 s) apart. So an engine that keeps one model at a time,
  as mlx-serve above, cannot run a map today.
- `load-api` engines (Sushi, mlx-serve) load and unload one model at a
  time. A `generic` engine has one start and one stop command for all of
  its models.
- A run is a chain of sessions: bootstrap, then per task builder sessions
  and a reviewer, with split, replan and a final review now and then. With
  a builder model and another model for the rest, each reviewed task costs
  at least two swaps: to the reviewer and back.

## Decisions

### 1. Who swaps

- **A. Tenaz, through the lease** (recommended).
- B. The engine, on request. Tenaz only stops requiring every model at
  once, and the engine swaps when fx names another model. But then a load
  happens inside a session's first request, counted in its time and
  against the harness's timeouts; it is not journaled; a model that does
  not load fails the session as an engine error, tried seven times,
  instead of blocking with its name; Tenaz cannot check at the start that
  each model loads; and it works only on engines that swap, which Sushi
  may not.

A, because what B lacks is what Tenaz is for: a run that says what
happened and stops with a reason. An engine that swaps by itself still
works under A: with one model loaded, it has nothing to swap.

### 2. How a run asks for swapping

- **A. A flag, pinned with the launch** (recommended). `tenaz init
  --swap-models` saves `launch.swapModels` in `run.json`; `tenaz run
  --swap-models` and `--no-swap-models` change it. The only other launch
  switch, `--no-proxy`, can only be turned on; this one can be turned off
  too, for a run moved to a machine that holds every model. Without the
  flag nothing changes: every model stays resident.
- B. Swap on its own when the models do not fit: load them all, and swap
  when the engine refuses one for memory. An engine runs out of memory for
  other reasons too (ADR 0012), may evict a model by itself, and the same
  run would behave differently from one day to the next.
- C. Groups of models resident together, swapped as groups. More than any
  machine needs today; A is one model per group.

A, because whether the models fit is a fact about the machine, not about
the goal, so it lives with the engine in `launch`. Tenaz does not decide
it, as it does not decide which role gets which model (ADR 0011).

`init` and `run` refuse swapping with `--engine generic` (decision 6) or
without a map; `run` checks after merging its flags into the pinned
launch. Building the lease refuses it too, rather than keep every model
loaded, when the engine cannot unload one model and keep answering.

### 3. One lease operation: serve a model alone

- **A. `serve_only(model)`** (recommended). The lease records `model` as
  the resident one, then, with the gate closed:
  1. if the model it left resident before is no longer listed as loaded,
     journals that the engine evicted it;
  2. unloads the run's other models the engine lists as loaded;
  3. loads `model` unless it serves already;
  4. opens the gate.

  Models of the engine that are not the run's are left alone. When
  `model` is already the only one loaded, it does nothing.

  With swapping, this is the one way the lease restores the engine: the
  swap before a session, `ensure_llm`, the wait after an engine error, the
  wait after a heavy job, and the reload after a media or heavy job all
  serve the resident model alone (in the code, `serve_only` is the
  session's entry and `serve_alone` the shared step). A refusal names
  that model and no longer says to load every model at once. The
  resident model stays the target after a swap fails, so each wait
  tries the whole swap again.
- B. Change only the swap. The first engine error would load every model
  again, the out-of-memory the swap is there to avoid, and the run would
  then fail the check that every model is loaded.

A, because one operation gives one answer to "what should be loaded"
everywhere.

Journal: the existing `lease` record, holder `llm`. `unload` and `load`
get the model's id as `detail`, with swapping also around a media job or
a heavy job, and the operation ends with `ready`. An eviction is a new
transition value, `evicted`, with the model's id as `detail`. No field is
renamed and `holder` gets no new value. The eviction is how a run will
show whether an engine swaps by itself.

### 4. When to swap

- **A. Before each try of a session** (recommended). In the session loop,
  after the prompt check of ADR 0012 and before the session opens, the
  lease serves the session's model alone (decision 3). That
  covers continuations, capacity reruns and engine reruns: a model the
  engine evicted is loaded back. The swap is outside the session's timing
  and its `inflight` record. A builder after a builder needs no swap.
- B. Keep the builder's model and load the others next to it for short
  sessions. Not a swap: it needs as much memory as ADR 0011's option B.

A, because a prompt too large then blocks the run without a swap, and a
session always starts with its own model loaded.

### 5. The check at the start of a run

- **A. Load each model once, in turn** (recommended). At every `tenaz
  run` start, in place of today's check that every model is loaded, Tenaz
  unloads any of the run's models that are loaded, then loads each model
  of the run one after the other, unloading the one before, and ends with
  the run's own model (`--model`) as the resident one. A model that cannot
  be loaded stops the run before any session, with its name and the
  engine's error. It costs one load per model. The preflight, when it
  runs, probes the run's model, as now. The waits after an engine error
  or a job are a separate path, `serve_only`.
- B. Load only the run's model. The others are tried when a session
  needs them: a reviewer's model with a typo would be found after the
  first builder session, perhaps an hour in.
- C. Read the engine's list of models, loaded or not. A model listed is
  not proved to fit or to load; only a load proves it.

A, because a few loads at the start are cheap next to a run that stops
at its first review, and ending with the run's model needs no knowledge
of which phase the run resumes in.

### 6. A swap that fails, and a stop

A swap that fails is handled as the reload that failed after a job
(ADR 0010): the gate stays closed, the session does not start, and the
run waits with `wait_for_engine`, each try a whole swap. When the model
still does not serve, the run blocks as "engine unavailable", naming
the model, as an engine failure in that session would be. An engine out
of memory while loading one model alone is such a failure. Loading the
model after its unload failed is not an option: on a machine that needs
swapping, that is the out-of-memory the swap avoids.

A stop during a swap, or during its wait, pauses the run before the
session, as a stop during the wait after a job does: nothing is charged.
After an earlier try of the same session (an engine rerun, a
continuation), the pause is at that try, with its continuation note, as
a stop after it would be; otherwise the continuation flag is left as it
was, so a continuation an earlier pause owed is still owed. On resume the
check at the start (decision 5) runs again.

### 7. Engines and the capacity cap

Only `load-api` engines swap. A `generic` engine could be given the
models to start in an environment variable such as `TENAZ_MODELS`, each
swap then a stop and a start of the whole server; it has no script to try
it with, and a script written without it would start every model.

`context_cap`, a share of what the engine said it can hold, stays one for
the run and is raised again as sessions go through (ADR 0012). With
swapping, a cap set in the builder's model says nothing about the
reviewer's; a cap per model would change a field of `run.json`. The
`context-cap` record names its session, and the session's record its
model, so a run can show whether that is worth doing.

## Consequences

- A machine that cannot hold the builder's and the reviewer's models
  together can run a map, at the cost of the loads: at least two per
  reviewed task, each journaled with its duration. A load is paid once per
  role change, not per request.
- Each model has the engine's memory to itself during its sessions, so
  mlx-serve's memory refusals of ADR 0012 should be rarer. Unmeasured.
- The engine's prefix cache does not survive a swap. A builder after a
  reviewer pays its whole prompt again.
- Media jobs and heavy jobs free and reload the resident model only. A
  job left by a stop runs on resume with the run's model resident, after
  the check at the start; the builder's swap follows.
- Swapping works with `--no-proxy` too. Between sessions no request is in
  flight; the gate matters only for a media job during a session.
- `tenaz status` says that the run swaps its models, and `status --json`
  gains `swap_models` (true or false). Runs without the flag are
  unchanged.
- ADR 0011's "every model in the map must be loaded before `tenaz run`"
  holds only without swapping; 0011 has a note pointing here. The README
  (the model map, "Tenaz does not swap", how it compares with model
  swappers), `skills/tenaz/SKILL.md`, the CHANGELOG and COMPATIBILITY
  change with it.
- Landed in steps: the lease's swap and the paths that use it; the swap
  before each session and the check at the start; the flag, its
  refusals and the docs. A real run with a pair that does not fit
  together, such as Flint int4 and Qwen3.8-27B on mlx-serve, goes in
  COMPATIBILITY.

## Out of scope

- `generic` engines, a capacity cap per model, and groups of models
  resident together (decisions 7 and 2C).
- Probing each role's model in the preflight (ADR 0011).
