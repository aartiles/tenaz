# ADR 0003: Bootstrap in steps

Status: accepted, 2026-10-25.

## Context

The bootstrap is one session that writes the spec, the map, `verify`, the
project skeleton and every task card. In the image-landing run of
2026-10-25, with a local model behind fx and a 16384-token output limit,
four bootstrap sessions in a row ended the same way: a few short turns,
then one reply that spent the whole output limit reasoning. The saved
reasoning (`<label>.thinking.md`) shows no loop. The model drafts the
whole bootstrap in its reasoning, with 28 to 31 code blocks starting with
the spec, before it writes any file. Nothing reaches the disk, so the next
session starts from nothing.

Lowering the reasoning effort to low and telling the session to reason
briefly and write files early did not change this. Raising the output
limit does not fit: the context budget is about 42,600 tokens, and a reply
of 32,000 tokens would nearly fill it.

## Options

### A. Tell the one session to write as it goes

- A prompt change only: number the files and ask for each to be written
  before the next is planned.
- The run above already had such a note, at low effort, and the model
  still planned everything first.

### B. Cap the reasoning tokens per request in the proxy

- Would stop the reply before the limit, but not make the model write
  anything; the next reply would plan again.
- Depends on each engine supporting a reasoning budget. TensorFold's has
  not been checked.

### C. Run the bootstrap as three sessions, one step each

- Step 1 writes the spec and the map; step 2 `verify`, the skeleton and
  the bootstrap notes; step 3 the task cards. Each session is told only
  its step, so no reply has to hold the whole plan.
- Each step ends with files on disk that the orchestrator checks; the
  next step starts from them.
- Cost: at least three sessions instead of one, so more prompt processing
  for a model that could do it in one. The steps are sequential.

## Decision

**C.** The bootstrap phase has a step (`bootstrap_step` in `run.json`:
`spec`, `verify` or `tasks`).

- The role prompt `bootstrap.md` has a common part and a `## Step N`
  section per step. A session gets the common part and its own step's
  section; its header says the step. A project whose copy of the prompt
  has no step sections gets it whole, with the step in the header.
- After each session, the orchestrator checks that step's files: step 1
  needs requirement lines in the spec and a map; step 2 adds a `verify`
  that passes twice with the same result; step 3 is the full check the
  single bootstrap had (cards, coverage, `verify`).
- A step that passes moves the run to the next one, and the attempt
  counts start again: `maxBootstrapAttempts` applies to each step.
- Before a step's first session, its check runs: a step whose files are
  already right (an earlier session wrote them, or a run resumed in an
  older Tenaz) is passed without a session.
- Steps 2 and 3 get the spec and the map in their prompt.

## Consequences

- A bootstrap takes at least three sessions where the model writes
  nothing beyond its step; a model that writes more in step 1 has the
  later steps passed by their checks.
- `BootstrapValidation` records in `journal.jsonl` gain `step`.
- `tenaz status` shows the step while the run is in bootstrap.
- Tested with the fake harness only. Not yet run against fx and a real
  model.

## Amendment, 2026-10-25: step 2 writes only `verify`

Step 2 first asked for `verify`, the project skeleton and the notes. In
the next image-landing run, step 1 passed, but the step-2 session drafted
a six-module check harness and the skeleton in its reasoning (62 code
blocks) and was cut at the output limit inside eight parallel file
writes. Step 2 now writes one small `verify` file with a single `T000.`
smoke check, and the notes; the checks of later tasks go in `checks/`
(never a `verify/` folder beside the executable, which the draft had),
and the skeleton becomes the first task card.
