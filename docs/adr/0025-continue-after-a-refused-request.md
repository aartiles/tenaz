# ADR 0025: Continue after a refused request

Status: accepted, 2026-11-06.

## Context

An engine answers some requests with 400, 413, 415 or 422 because of
what they hold: a file it cannot decode, a part type it does not know.
The agent keeps that input in its conversation and sends it with every
later turn, so every request of the session is refused. Tenaz treated
the session as an engine failure (ADR 0012): it reran the same session
with growing waits, six times, then blocked the run as "engine
unavailable". In the articulated-cabinet run (2026-11-05) that took an
hour, for a video Sushi could not decode (ADR 0024).

## Options

### A. Retry with backoff, as before

- Right for an engine that is busy or out of memory, which those
  errors are not. The rerun meets the same input.

### B. Fail the attempt

- The refused input may be the reviewer's or builder's way of looking,
  not a fault in the work. The attempt would be charged for it.

### C. Continue in a fresh session, with a note (chosen)

- As for a refused image: the fresh session starts without the refused
  input, keeps the file changes, and is told what was refused and to
  read it another way.
- Uses the session's continuation budget, shared with context, image
  and turn stops.

### D. Strip the input in the proxy

- Right when the input is known, as for video files (ADR 0024). Not
  possible for a refusal Tenaz has not seen before.

## Decision

C. An image refusal, a prompt too long and an engine out of memory keep
their own handling. A reply after the refusal clears it, and so does
another error: the last error says how the session ends.

## Consequences

- A refusal the fresh session meets again, such as a chat-template or
  tool-schema error, spends the continuations, and the session is then
  retried as an engine failure, as before: at most `contextContinuations`
  (3) extra sessions before the old retries.
- The journal records `refused-continuation`; the session record has
  `request_refusal` when one ended it.
- Tested with unit tests. Not yet seen in a run.
