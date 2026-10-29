# ADR 0010: A model per role

Status: accepted, 2026-10-29.

## Context

A run has six session roles: bootstrap, builder, reviewer, split, replan
and final reviewer. Until now all of them were played by the one model
pinned in `run.json`. The maintainer wants a fast mixture-of-experts model
as the builder, and a slower, denser model of another family as the
reviewer and the planners. The reasons, in order:

1. A reviewer of another family does not share the builder's blind spots.
2. The bootstrap and the reviews are short sessions, where a slow model is
   affordable. The builder's sessions are the long ones and need speed.
3. On the maintainer's machine (M5 Pro, 64 GB) a 24 GB builder and a 20 GB
   reviewer fit in memory together, so no model has to be swapped out.

What was found on 2026-10-29: no local model has a measured 100 tokens per
second at 32k context on an M5 Pro. The closest is Flint 40B-A2.5B on
Kapitsa, 98 tokens per second at 32k, 24 GB. Qwen3.8-27B at 4 bits is
about 20 GB. Sushi serves neither. Kapitsa and mlx-serve serve both, in
one process.

## Options

### A. One model for every role

What Tenaz did. Simple, and the only choice on a machine that fits one
model. It cannot give the review to another family.

### B. A map from role to model, every model resident at once

- `tenaz init --model-for ROLE=ID`, repeatable, with
  `--context-window-for ROLE=N`. A role without an entry plays `--model`.
  Tenaz does not decide which role gets which model.
- The map is pinned in `run.json` (`config.roleModels`). Each session is
  opened with its role's model, and its context budget comes from that
  model's window.
- A role's model reads images when the run's does (`--vision`): there is
  no flag per role for it.
- One engine serves every model in the map. The preflight and `doctor`
  check that the engine lists each one as loaded, and name the one that
  is not. Tenaz does not swap between them: the lease still unloads the
  LLMs only for a media job, and then all of them.
- fx is the harness covered. `init` refuses a map with `--harness pi`.

### C. Swap the models through the lease

The lease would unload the builder's model and load the reviewer's
between sessions. It works on a machine that cannot hold both, but every
review then costs two model loads, and no engine on the maintainer's
machine needs it today.

### D. Two engines

One engine per model, each in its own process. The two processes share
the memory without knowing of each other, the proxy would need an
upstream per session, and the compatibility surface doubles.

## Decision

B. C is deferred until a machine needs it; the map does not get in its
way. D is rejected.

B lands in steps, each with its CHANGELOG line: `init` pins the map and
`status` shows it; then the sessions use it; then the preflight and
`doctor` check each model; then fx is launched with the session's model.

One engine per run, as before. The capacity cap the engine sets after it
runs out of memory (`context_cap`) stays one for the run, whichever
model's session hit it.

## Out of scope

- Swapping LLMs through the lease when they do not fit together (C).
- Pi with a map. It is small: pass the session's model as `--model` and
  list every model of the map in the run's `models.json`.
- Reasoning effort per role: a separate change.
- Reading a role model's context window from the engine instead of from a
  flag.
- Running the preflight's probes (reasoning, cache, a tool call, an image)
  on each role's model. They run on the run's model.

## Consequences

- An unattended run can review with another model family.
- Every model in the map must be loaded before `tenaz run`; a missing one
  stops the run before its first session, as a missing `--model` does.
- `run.json` files without `roleModels` load as before: every role plays
  the run's model.
