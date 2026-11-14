# ADR 0036: Keep the turn budget, and make its stop a parameter

Status: accepted, 2026-11-14.

## Context

The watchdog steers a session to wrap up at its turn budget and stops it
at 1.5 times that budget. `turnBudget` (40) has been the default since the
first commit, never measured; the 1.5 was hard-coded. Reviewers get 25
turns and the final review 60, also hard-coded. A builder or bootstrap
session stopped at its turn budget is continued by a fresh session,
without charging an attempt.

In the Hardball 3D run (2026-11-11 to 14, fx 0.0.16, Qwen3.8-Flash-Next
on Sushi 1.6.0) a third of builder sessions reached the 60-turn stop.
That asked whether 40 is right, and whether the turn budget is needed at
all when the context guard (`contextBudget`, `contextAbort`) already stops
a session at its ceiling.

We measured 1478 sessions of 23 runs (Tenaz 0.1.0 to 0.7.0; fx, Pi and
dsh; Qwen3.8 Flash and 27B, Bonsai 27B, LFM3; windows of 58k to 262k
tokens), all on one Mac:

- Builders that settled on their own: median 33 turns, p75 45, p90 52.
  36% were steered at 40 and 74% of those wrapped up; 94% of the steered
  ones that wrapped up passed `verify`, against 97% of unsteered sessions
  under 30 turns. The steer does not make wrap-ups fail.
- Of the 230 builders that wrapped up after the steer, 77 needed more
  than 10 turns past it and 29 more than 15; 81 builders settled at 50 to
  59 turns.
- The 81 builders stopped at 60 turns had a peak context of a median
  1.10 times the context budget; 52 were already over it, a few turns
  from the context stop. Their continuations took a median 42 turns:
  these were tasks of about 100 turns that no single session finishes.
- Reviewers: median 18 turns; 35% were steered at 25 and 97% of those
  wrapped up, in a median 3 turns. Replan, split and bootstrap sessions
  rarely reached their budget.
- Of 258 builder context stops, 214 came before turn 40. Where both guards
  steered one session, context came first in 188 of 233.
- The turn stop caught 29 sessions under their context budget: a dsh run
  (55 to 61% of it; dsh compacted its own context then, which ADR 0030
  has since turned off) and the Hardball run, whose model added about
  1400 tokens a turn.

## Options

### A. Drop the turn budget, keep the context guard

- One guard fewer. Most stops in these runs came from context.
- A session that adds little context each turn, or has a large window,
  is left with no bound but `sessionMinutes`: a 262k window is about 85
  turns before its context budget at 2000 tokens a turn, a 1M window
  over 300. Compaction leaves little room around the context guard:
  Tenaz stops a session at its second compaction, and turns it off or
  refuses it in dsh and Qwen Code (ADR 0022, ADR 0030).

### B. Make the turn budget relative to the context window

- Grows with the window, so it bounds least where the turn budget is the
  only bound: large windows and compacting harnesses.

### C. Raise the default, or move the stop

- Raising the steer delays wrap-ups that pass `verify` 94% of the time.
- A stop nearer the steer (1.25, at 50 turns) would end most of the 81
  builders that settled between 50 and 59 turns.
- A later stop (2.0) changes little in these runs: the sessions stopped
  at 60 turns were near their context stop already, and both stops lead
  to the same continuation.

### D. Keep 40 and 1.5, and make the stop a parameter

- The defaults stay as measured. `turnAbort` sits next to `contextAbort`
  for a harness or window these runs did not cover.

## Decision

D. `turnBudget` stays 40 and the new `turnAbort` defaults to 1.5. A
session is stopped once its turns reach `turnAbort` times its budget,
rounded up; 1 stops it at the budget without a steer, below 1 before
it, and 0 at its first turn. Like `contextAbort` and the other run
parameters, the value is not range-checked. There are no new per-role
defaults: the reviewer's 25 turns and the final review's 60 stay fixed,
and the data does not ask for others; `turnAbort` sets their stop too.
The README and the skill now document `turnBudget`, a card's
`budget: {turns: N}` and `turnAbort`.

## Consequences

- A run pinned before this loads with `turnAbort` 1.5 and stops sessions
  where it did.
- The evidence comes from one machine (M-series, memory-tight) and mostly
  one model family. Slow models there reach `sessionMinutes` (90) before
  60 turns, and 64k windows reach their context budget first; the turn
  budget bound mainly fast models on 131k windows.
- Loops that the loop, repeat and idle rules miss, such as edit-and-rerun
  cycles whose arguments change each turn, were not measured; the turn
  budget is what stops them.
