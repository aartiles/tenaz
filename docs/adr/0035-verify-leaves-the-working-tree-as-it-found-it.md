# ADR 0035: `verify` leaves the working tree as it found it

Status: accepted, 2026-11-12.

## Context

The protocol asks checks to be deterministic and to clean up after
themselves, but nothing checked that `verify` leaves the files git sees
alone. In the Hardball 3D run (2026-11-11 and 12, fx 0.0.16,
Qwen3.8-Flash-Next on Sushi 1.6.0) the checks wrote screenshots into
`artifacts/`, a folder git tracked, and their bytes changed on each run:

- every `verify` dirtied the tree, so "bookkeeping before" commits kept
  committing them (`artifacts/T001.smoke.png` in 18 commits);
- reviewers and replans that ran `verify` had them reverted as
  out-of-role changes;
- each reviewer was told to look at every image the task's diff
  changed: 5 to 11 screenshots, most of other tasks.

## Options

### A. The rule, checked

- The protocol says what checks produce goes under a folder `.gitignore`
  lists. The bootstrap is not accepted, and an attempt fails, when
  Tenaz's own `verify` run changed files git tracks or does not ignore;
  the error names them and says how to fix it.
- A run already started with such checks fails its next attempt until a
  builder fixes them.

### B. The rule, not checked

- One line in the protocol and the bootstrap prompt. A bootstrap that
  misses it, as an external one did here, goes on as before.

### C. Tenaz cleans up after `verify`

- Tenaz restores what its `verify` runs changed and leaves those files
  out of the reviewer's images. The contract stays as it is; the agent's
  own `verify` runs still dirty the tree, and a real asset a check also
  lists as an artifact would be hidden from the reviewer.

## Decision

A. Tenaz takes the state of the files git reports as changed, each with
a hash of its bytes on disk (none for a file deleted, or one git
ignores, as after `git rm --cached`), before and after its own `verify`
runs (`Runner::tree_state`). A file outside `.tenaz/` that
differs is an error (`verify::writes_error`, through
`Runner::tree_writes`): at bootstrap validation around the two `--all`
runs, and in `Runner::verify_attempt` around `--all` and `--task`, only
when the attempt passes otherwise. `Vcs::ignored` says, in one call, which of the changed files git
ignores; a file HEAD tracks never is, even when a `.gitignore` matches
it.

## Consequences

- A check whose output has the same bytes on each run is not caught once
  that output exists: it is committed once and never changes again.
- The agent's own `verify` runs are not watched; once the output is
  ignored, they leave nothing for Tenaz to commit.
- A file the attempt's builder left and a check then rewrites is caught
  too, as it should be.
- When git cannot say what changed, before or after, nothing is checked,
  and the log says so.
- A failed attempt's files are committed before the next session
  ("bookkeeping before builder"), so the fix for a file `verify` keeps
  rewriting is to ignore its folder and untrack the file
  (`git rm --cached`), as the error says.
