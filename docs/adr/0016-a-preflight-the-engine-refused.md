# ADR 0016: A preflight the engine refused

Status: accepted, 2026-11-02.

## Context

The preflight checks reasoning, the prefix cache and a tool call before
the first session. On 2026-11-02 mlx-serve 26.10.3 refused all three of
its prompts with HTTP 503 under Pi: it had no memory left for them. The
run stopped saying there was no reasoning at the configured level, the
engine did not reuse the cached prompt prefix, and the model could not
run `cat probe.txt` with Pi's shell tool; it advised another model and
pointed to `.tenaz/private/preflight.*`, which Pi never writes. The model
had answered nothing; the engine's error was only in the engine's log.

A failed preflight stops the run unless `--allow-weak-engine` accepts it.
An accepted preflight is saved as passed and is not run again. A session
the engine fails is rerun with a backoff, up to `engineRetries` times (6
by default; see ADR 0012).

## Options

### A. Treat it like any failed check, as before

- Without the flag the stop blames the model and advises another one, as
  on 2026-11-02.

### B. Name the engine's error, and let the flag accept it

- The operator sees the cause. But with the flag the run starts on an
  engine that cannot take requests, and the preflight is saved as passed,
  so the model is never measured.

### C. Retry, as a session does

- A busy engine may answer a few seconds later. An engine out of memory
  does not get its memory back by waiting: in the Digger 3D run only a
  reload helped (ADR 0012). With the sessions' backoff the run would wait
  up to about 45 minutes before it says so.
- A session keeps its retries (ADR 0012) because stopping an unattended
  run part way costs more than waiting. Before the first session nothing
  is lost by stopping, and the operator is likely there.

### D. Name the engine's error, and stop whatever the flag says

- The operator sees the engine's own error and the advice the run gives
  for memory. The preflight is saved as failed, so the next run measures
  the model again.
- A single busy answer stops the run too, even when a check failed on
  its own beside it.

## Decision

**D**, for the errors that say the engine could not take requests: out
of memory, 502, 503 or 504, or an engine not reached
(`is_engine_unavailable`). Any other engine error (a refused image, a
400 or 500 not about memory, a 429) is left to the checks and the flag,
since it may be about the request or the model.

- The stop names each such error once, with the run's out-of-memory
  advice when one is about memory. It points to `run.json`, and to
  `.tenaz/private/preflight.fx-acp.log` only with fx: Pi keeps no log of
  it.
- `run.json`'s `preflight` keeps every engine error of the preflight,
  each cut to 500 characters, in `engine_errors` (absent when there were
  none). The log and `tenaz status` show the preflight as not judged.

## Consequences

- An engine that is briefly busy at the start stops the run; running it
  again is the retry.
- The check reads the error's text. The engine proxy leads an error with
  its status (`503: …`), and Pi is assumed to do the same; a Pi message
  without the status is still told by its memory wording, a busy engine
  is not. With `--no-proxy`, an engine Pi cannot reach is not told
  either.
- Tested with fakes and the scripted harness only, not against a
  refusing engine (see Known gaps in COMPATIBILITY.md).
