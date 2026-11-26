# ADR 0044: Let a session that is still writing run past the turn stop

Status: accepted, 2026-11-26. Amends ADR 0036.

## Context

ADR 0036 kept `turnBudget` at 40 and its stop at 1.5 times that (60
turns), measured on runs up to 2026-11-14. Since then four long fx runs
on Sushi 1.6.0 (Qwen3.8 Flash Next) made the turn stop the most common
stop of all: 216 sessions across runs, against 36 context stops. The
four runs are Hardball 3D (HB), Hardball improvements (HBi), Pocket Roads
(PR) and Last Delivery r2 (LD, still running when measured).

We measured the 125 builder sessions of those runs stopped at 60 turns
(HB 61, HBi 32, PR 23, LD 9), and the 265 builders that ended on their
own:

- Builders that ended on their own: median 43 turns, p75 50, p90 55;
  75 of them (28%) used 50 to 59 turns. The median was 45 to 47 in HB,
  HBi and PR, and 35 in LD.
- Stopped sessions were still working. 102 of the 125 (82%) wrote a file
  in their last 5 turns; the median one wrote 1 turn before the stop.
  Only 23 had gone 5 or more turns without a write.
- The idle rule's own idea of progress does not tell them apart: a read
  with new arguments counts, so 124 of the 125 made "progress" in their
  last 5 turns. Re-reading a file at a new line range is progress to it.
- The next session on the task ended on its own in 82 of 125 and was
  itself stopped at 60 in 36. The 82 took a median 39.5 turns: it reads
  again what the stopped session had read. The task was done next in 97
  of 125.
- At the stop, the context was at or over 0.9 times the budget in 51
  sessions. fx compacts its own context at about 92k tokens without
  telling Tenaz, so most of these sessions are bounded by that, not by
  the context stop (1.3 times an 85k budget is 111k).

The writes were read from each session's `rpc.jsonl`, counting a write
tool call in the turn the watchdog sees it; working-tree changes by shell
commands could not be replayed. The planners sized cards by context
alone; they are now told the turn budget too (a separate change). That makes fewer cards too big, but the
evidence above is about sessions already working on a card that fits.

## Options

### A. Raise `turnBudget` (to 55, about the p75)

- Simple, and a run can do it today with `--set turnBudget=55`.
- Moves the steer too: the 36% of builders steered at 40, who wrap up
  and pass (ADR 0036), are steered later. A stuck session also runs to
  the new stop, 83 turns, before it ends.
- The planners are told a bigger budget and size bigger cards.

### B. Raise `turnAbort` (to 2.0)

- Keeps the steer at 40 and gives every session 80 turns. A session that
  has stopped writing at turn 60 runs 20 more turns for nothing; the
  idle rule only ends it after 20 turns without a write or a new read.

### C. Past the stop, keep a session that is still writing, up to a ceiling

- The steer and the stop stay where they are. At the stop, a builder or
  bootstrap session that wrote a file in the last `turnWriteWindow` turns
  (5) goes on; it is stopped at the first turn after the stop with no
  write in that window, and at `turnCeiling` times its budget (2.0, 80
  turns) whatever it does.
- In the measured runs, 102 of the 125 stopped sessions would have gone
  on and 23 would have stopped as before.
- One rule and two parameters more. Only builder and bootstrap sessions
  write the project's code, so reviewers and planners keep the plain
  stop.

### D. Use the idle rule's progress (new reads too) for C

- Reuses the idle count as it is, but extends 124 of 125 sessions: no
  different from B in these runs.

## Decision

C. `turnBudget` (40) and `turnAbort` (1.5) keep their meaning; the new
`turnCeiling` (2.0) and `turnWriteWindow` (5) let a session that still
writes run on to 80 turns. A session let past the stop is steered once:
it is told it goes on only while it writes, and when it stops at the
latest. Its stop reason stays `turn budget exceeded (n/budget)`, so a
stopped session is continued as before. `turnCeiling` at or below
`turnAbort`, or `turnWriteWindow` 0, turns the extension off.

Writes, not reads: a session past 60 turns should be finishing, and the
runs show that reads alone do not separate the stuck sessions. A check
run after an edit is covered by the edit.

A write is a write tool call that ends without an error on a project
file outside `.tenaz/`; a write call that names no path, or a file
outside the project, does not count. A sandboxed run gives the watchdog
no write scope, so there only a relative path that stays in the project
counts, and a builder that writes by absolute path is stopped at the
stop. A failed edit (old text not found, a common
small-model failure) is not progress. The handoff notes the steer asks
for are under `.tenaz/notes/`, so writing them does not keep a session
going. A file a shell command changed does not count either: the idle
rule's working-tree fingerprint also changes with the notes, and the
runs measured write calls only. A builder that writes only through its
shell is stopped at the stop, as before.

## Consequences

- Fewer continuations that re-read the work, and fewer stop-and-continue
  cycles for the cards that need 60 to 80 turns. Not yet measured on a
  run: the next Sushi + fx goal should compare turn stops, sessions per
  done task and wall time per done task with the numbers above.
- A session that keeps rewriting the same file without getting closer
  (the edit-and-rerun cycles ADR 0036 left to the turn budget) runs to
  80 turns, 20 more than before. The loop and repeat rules, the context
  stop and `sessionMinutes` still bound it.
- The planners are told the stop and the ceiling, and still size cards
  to the budget.
- A run pinned before this change loads with the new defaults, so a
  resumed run gets the extension.
- If the extension does not pay off, the fallback is A on the measured
  p75: `--set turnBudget=55`.
