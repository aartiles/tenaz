# ADR 0017: The preflight's checks, listed once

Status: accepted, 2026-11-02.

## Context

The preflight judges the model on five checks: the session finished,
reasoning, the prefix cache, a tool call, and the image for a vision
model. Three places listed them apart: whether the preflight passed
(`ok`), its line in the run's log and in `tenaz status`, and the error
that stops the run. A check added to `ok` but not to the line made a
failed preflight's line read like a pass: that happened to the image
check and to the session finishing (fixed in #33 and #34).

## Options

### A. Keep three lists, and a test per check

- The table test already pins the line. It catches a missing check only
  when someone adds a row for it, which is the step that was missed.

### B. An enum of the checks, with a `match` for each use

- The compiler makes each `match` handle a new check. But a new check
  still means editing three places, and a list of all the variants must
  be kept by hand for the order of the line, which the compiler does
  not check.

### C. One function that lists every check with all it needs

- Each entry holds the check's key on the line, what the line shows of it
  (nothing for a session that finished, or a check the preflight did not
  make), whether it passed, and its failure text. `ok`, the line and the
  stop's error read the list.
- Once the preflight measures it, a new check is one entry (and the
  list's length). Leaving out a field of an entry does not compile; a
  check left out of the list does, and only the tests can catch it.
- The failure text of the tool check names the harness, which `ok` does
  not know, so it is a function of the preflight and the harness, not a
  string.

## Decision

**C**, in `checks` in `crates/core/src/orchestrator/preflight.rs`. The
"not judged" rule (an engine that could not take requests) and the warning
for a cache the engine does not report stay apart: they are about the
engine, not checks of the model.

## Consequences

- The line and the stop's error read as before, byte for byte. The table
  test pins the line; a new test pins the stop's error with every check
  failed. Without its line that reads the new list, it passes on the
  code from before the change too.
- The order of the list is the order of the line and of the stop's error.
