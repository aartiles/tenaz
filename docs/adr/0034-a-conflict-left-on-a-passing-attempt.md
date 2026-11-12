# ADR 0034: A conflict left on a passing attempt

Status: accepted, 2026-11-12.

## Context

A builder that finds its task cannot pass without changing a check of a
done task writes `.tenaz/conflicts/<TASK>.md` and stops (ADR 0009).
Tenaz reads the file only when the attempt fails `verify`; when the task
is done, the file is removed.

In the Hardball 3D run (2026-11-11, fx 0.0.16, Qwen3.8-Flash-Next on
Sushi 1.6.0) T003's builder found that R23 (who bats in the top half)
needs `T002.state` changed. It wrote the conflict, but also changed its
own check so it no longer pinned the teams, and the attempt passed. The
reviewer accepted T003, calling the inverted R23 minor because "the
conflict file stands". Tenaz removed the file when T003 was done and
said nothing. No replan could then amend `T002.state` (ADR 0009 allows
only checks a standing conflict names), four replans that tried were
rejected, and R23 stayed inverted.

## Options

### A. Tell the builder and the reviewer

- When a builder session has answered every prompt with a conflict file
  of its task, and its attempt would pass as Tenaz judges it
  (`judge_attempt`), it gets one more prompt: the file does not stand;
  either make the task's checks test what the card asks, so they fail
  and the conflict stands, or delete the file and say what is left.
- The reviewer is told the file does not stand and to judge the work as
  if it were not there.
- The rule stays as it is: a conflict needs a failing attempt.

### B. A conflict file blocks the task whatever `verify` says

- Trusts the builder's word.
- The work of a passing attempt is reverted, as for any conflict.

### C. A conflict stands after the task is done

- Keeps the work, and a replan may amend the checks later.
- Changes when a conflict stands and when it is removed (ADR 0009 and
  0018), with a done task as its owner.

## Decision

Option A, the same pattern as ADR 0018 and 0033: the session is told
while it still holds its work. `Runner::verify_attempt`
(`crates/core/src/orchestrator/phases.rs`) judges the attempt both for
the builder's follow-up (`session/harness.rs`) and at the end of the
attempt, so they cannot disagree. The reviewer's section
(`phases/review.rs`) says that what the card asks and the work does not
do is a MAJOR finding: in the Hardball 3D run the reviewer called it
minor, and only minor findings accept the task. The removal is logged in
`phases/task.rs`.

## Consequences

- A `verify --all` and `--task` before the session ends for a builder
  that leaves a conflict file, and one more turn when its attempt would
  pass; neither for any other builder.
- A builder that keeps the file anyway still passes; the reviewer judges
  the work against the spec, and the file is removed when the task is
  done.
