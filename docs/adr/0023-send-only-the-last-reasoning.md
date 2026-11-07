# ADR 0023: Send the engine only the last reasoning

Status: accepted, 2026-11-05.

## Context

A coding agent sends the whole conversation with every request. Qwen
Code 0.26.6 puts each earlier assistant message's reasoning back in it
(`reasoning_content`) for a generic OpenAI-compatible endpoint; it drops
it only for a few providers it knows by host. A Qwen chat template keeps
the reasoning of every message after the last user message, and in an
agent session that is nearly all of them. Sushi did: the same request
took 287 prompt tokens without 3000 tokens of reasoning in an earlier
assistant message and 3288 with it.

In the articulated-cabinet run (2026-11-05, Qwen3.8 Flash Next on Sushi,
an 85196-token budget) two builder sessions of T026 grew by 70k and 78k
tokens before they ran out of context. About 27k to 36k of each was the
model's own reasoning from earlier turns, and 25k to 30k was what its
tools returned. Most builders ran out after 25 to 40 turns.

## Options

### A. Drop it in the proxy, by default

- Every request goes through Tenaz's proxy. It removes `reasoning_content`
  and `reasoning` from every assistant message but the last, for any
  harness and model.
- Only the previous assistant message changes from one request to the
  next, so the engine's cached prefix stays valid up to it.
- The model no longer sees why it took its earlier steps, only what it
  did and what came back. Qwen3's own template and most hosted APIs with
  reasoning already work this way.

### B. Drop it only for Qwen Code

- Narrower, but fx and Pi run the same models through the same engines,
  and nothing about the problem is Qwen Code's alone.

### C. Keep the last N reasonings

- Closer to the agent's own history, but a number to choose with nothing
  to choose it by, and N reasonings still crowd a small window.

### D. Leave it to the engine's template

- Not Tenaz's to change, and a run should not depend on how each
  engine's template treats reasoning.

## Decision

A, with `--set keepPastReasoning=true` to send it all again.

## Consequences

- Each request cuts the engine's cached prefix at the previous assistant
  message, so that message, its tool result and the new turn are read
  again; the reasoning removed is usually larger.
- A session should last more turns before it runs out of context; how
  many more is not measured yet.
- A run that compares harnesses or models sends the same kind of history
  to each.
- A model that depends on its earlier reasoning may repeat work. If so,
  `keepPastReasoning` turns this off for the run.
- Not yet seen in a real run.
