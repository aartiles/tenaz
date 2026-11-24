# ADR 0041: A run parameter for the output limit of a reply

Status: accepted, 2026-11-24.

## Context

Every harness Tenaz configures gives the model a fixed output limit of
16,384 tokens per reply, reasoning included: fx's `max_output_tokens`,
OpenCode's `limit.output`, dsh's and Pi's `maxTokens` (Pi keeps a value
of the user's own `models.json`), the `maxOutputTokens` a generic ACP
agent is told, and the limit the engine proxy assumes for a request that
names none. A reply that reaches it is cut (`length`), its work is lost,
and the next session of that role reasons at a lower effort.

In benchmark runs of Inkling 2 Mini (a reasoning model, thinking `high`)
on the Raycaster Shooter brief, single replies spent 30k to 40k tokens on
design reasoning before writing any file. The reasoning was not
repetitive; it was cut at 16,384 tokens after 8 to 11 minutes and thrown
away, and the run fell back to a lower effort. Others run Inkling with up
to 65,536 output tokens per request. The maintainer wants to let the
reasoning finish in a follow-up run and compare that with lowering the
effort.

## Options

### A. Keep 16,384 for every run

- Nothing to configure, and every run is comparable with the others.
- A model that needs a longer reply loses it every time; the only lever
  is the effort, which is the other half of the comparison wanted.

### B. One run parameter, `maxOutputTokens`

- Pinned in `run.json` like the other parameters, set with
  `init --set maxOutputTokens=65536`, changed for a resumed run by editing
  `params`. One value for every role and every harness.
- Unset (`null`), a run is configured exactly as before: 16,384, and Pi
  keeps a `maxTokens` of the user's own `models.json`.
- Set, it wins over the user's Pi `maxTokens` too: the run says what it
  measured with.

### C. A value per model (`--max-output-for role=N`, like `--context-window-for`)

- Fits a run whose roles play models with very different windows.
- More flags and more `run.json` fields for a need no run has shown yet:
  the reviewer and builder of the Inkling runs play the same model. It can
  be added later on top of B, with B's value as the default.

### D. Read it from the model card or the engine

- No engine Tenaz drives reports a per-request output maximum: the
  OpenAI-compatible `/v1/models` lists ids, mlx-serve and Sushi add the
  context length at most. A model card's "max output" is the model's
  trained ceiling, not what fits next to a growing session in the window
  the run gave it; taking it would make a run's behaviour depend on a file
  Tenaz does not pin.

## Decision

B, an optional run parameter `maxOutputTokens`, for every role.

- Each harness is configured with it: fx's `max_output_tokens`,
  OpenCode's `limit.output`, dsh's `maxTokens`, Pi's for every model of
  the run, the generic agent's `maxOutputTokens`. Qwen Code's settings have
  no such field that Tenaz writes; the engine proxy puts the limit into
  every request that names none (`max_tokens`, or `max_output_tokens` on
  the responses API), so Qwen Code and any agent that sends none get it
  too. Unset, the proxy adds nothing, as before.
- The proxy reports a reply that reaches the request's limit, or this one
  for a request that names none, as cut, so the effort dial and the
  "reply reached the output limit" stop work at any limit.
- 0 is refused, by `--set` and when `run.json` is loaded.
- A pinned limit must leave room for a prompt in each role's window: a
  reply is written into the same window as the prompt and the session so
  far. `init` and `run` refuse a limit that, with `harnessReserveTokens`,
  fills the window of a role's model, and name the model, its roles and
  the value to stay below. They warn when a session at its context budget
  would leave less than the limit for a reply: the engine is then likely
  to cut the reply at the end of the window (reported as cut) or refuse
  the request; the warning gives the `contextBudget`, and the limit, that
  keep the room (only the `contextBudget` when it fills the window by
  itself).
  Before each session the prompt must fit both the role's budget and what
  the limit leaves of the window, or the run blocks with
  `prompt too large:` and says to lower the limit when that is enough. A
  prompt the harness refuses in such a run is told the same, among the
  other things to change. With the parameter
  unset none of this applies, so existing runs block exactly where they
  did.

Not changed: `bigOutputTokens` is the size of one tool result, not of a
reply; `harnessReserveTokens` is what the harness adds to the first
request's input. Neither depends on the output limit.

## Consequences

- A run can let a reasoning model finish a long reply, at the cost of
  wall-clock time per reply and of context room: a reply of 65,536 tokens
  in a 131,072-token window leaves half of it for the session, less than
  the default `contextBudget` of 0.65, which the run then warns about.
- A run that sets the limit is not comparable, reply for reply, with one
  that did not; `run.json` records which.
- Only Pi was configured with the value of the user's own file before. A
  run that sets the parameter no longer uses it.
- Untested against a real harness or engine at a limit above 16,384: the
  configuration each harness is given is tested with fakes, not that each
  one sends it, nor that mlx-serve honours 65,536 (see "Known gaps" in
  docs/COMPATIBILITY.md).
