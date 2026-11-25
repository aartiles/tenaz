# ADR 0042: A thinking level per role

Status: accepted, 2026-11-25.

## Context

`init --thinking` sets one thinking level for every session of a run.
Benchmark runs of Inkling 2 Mini showed both ends of it going wrong. At
`high` for every role, builders wrote replies of up to 63K tokens and runs
took six hours without better results. At `low`, reviewers accepted games
with obvious flaws of judgment. The maintainer wants to try roles that
judge (bootstrap, replan, reviewer, final-reviewer) at `high` and builders
at `low`, which a single level cannot express.

Tenaz already lets each role play its own model (`--model-for ROLE=ID`,
ADR 0011): the level per role can follow the same path.

## Options

### A. A per-role flag, `--thinking-for ROLE=LEVEL`

- Built like `--model-for` and `--context-window-for`: repeatable, one
  role each, the same parsing and the same errors for a wrong role.
- `--thinking` stays the level of every role not named, with its default
  `medium`, so a run without the new flag is configured as before.
- Pinned in `run.json` next to `roleModels`, as `config.roleThinking`.

### B. A run parameter, `--set thinkingFor=reviewer:high,builder:low`

- No new flag; changed for a resumed run by editing `params`.
- Run parameters are one value each, used the same way by every role. A
  map in a string needs its own syntax, which `--set` has nowhere else.
  The run's own level is not a parameter either: the two would live in
  different places of `run.json`.

### C. Reuse `--model-for` with an alias, `--model-for reviewer=m@high`

- No new flag, and a role's model and level are named together.
- `--model-for` refuses a role on the run's own model, and a role that
  only wants another level would have to name it. The `@` would have to
  be told apart from model ids that contain one. The level is not part of
  the model: the engine serves one model at any level.

## Decision

A. `init --thinking-for ROLE=LEVEL`, repeatable.

- The levels are Pi's: `off`, `minimal`, `low`, `medium`, `high`,
  `xhigh`, `max`. An unknown role or level, and a role given twice, stop
  `init` with what is wrong, as `--model-for` does. `--thinking` itself is
  still not checked, as before.
- `config.thinking` keeps its name and meaning, the level of every role
  without one of its own; `config.roleThinking` is written only when some
  role has one, so older runs read and write as before.
- The level is not part of the model the engine serves, but the
  session's model, as Tenaz hands it to a harness, carries its role's
  level (`SessionModel.thinking`): every harness already writes its
  configuration from it before each session. Each harness takes it where it took the run's level: Pi's
  `--thinking` for that session; fx's `effort` and dsh's `reasoning`
  in the configuration written before the session; the generic agent's
  `reasoningEffort` in its session file. OpenCode and Qwen Code take no
  level of their own.
- The engine proxy fills `reasoning_effort` into requests that lack it.
  Before, that was the run's level; now the orchestrator works out each
  session's effort before it starts, the preflight included, and sets it
  on the effort dial the proxy reads. fx (which does not forward its
  effort), OpenCode and Qwen Code get the role's effort this way. A role
  at `off` adds none. After a cut reply, the next session of that role
  steps down from its own level and replaces what the harness sends;
  `off`, `minimal` and `low` have no lower step, so such a session is
  neither lowered nor said to be.
- With Pi, a model is marked as reasoning in the run's `models.json` when
  a session on it asks for an effort: the preflight (the run's model at
  `--thinking`) or a role that plays it.
- `tenaz status` adds a `Role thinking:` line when some role has its own
  level, and `tenaz status --json` adds `thinking` and `role_thinking`.

## Consequences

- A run can judge at one level and build at another, and the comparison
  the maintainer wants can be made.
- `off` means Tenaz asks for no effort. Pi and dsh turn reasoning off;
  fx, OpenCode and Qwen Code send none, so the engine's own default
  applies, and an engine that thinks by default still thinks.
- fx and the engine are given `minimal`, `xhigh` and `max` as they are
  (only dsh maps them, `max` to its `xhigh`), as `--thinking` already
  did; fx documents `low`, `medium` and `high`.
- The preflight runs at `--thinking`, not at a role's level, and still
  requires the model to reason. `--thinking off` with roles that think
  is refused there unless the engine reasons by default: use a low
  `--thinking` and raise the roles that judge instead.
- With `--no-proxy`, fx, OpenCode and Qwen Code ask the engine for no
  effort at all, as before: the per-role level reaches only Pi, dsh and
  the generic agent there.
- Tested with the scripted harness, the fake agents and the proxy's unit
  tests; not yet in a run against a real model.
