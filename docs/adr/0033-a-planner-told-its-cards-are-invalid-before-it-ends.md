# ADR 0033: A planner told its cards are invalid before it ends

Status: accepted, 2026-11-12.

## Context

When a split or replan session ends, Tenaz judges the cards it left: the
statuses it may not set are put right, then each `amends` is checked
against the conflicts that stand (ADR 0009), the requirements must stay
covered, and no card may be deleted. One error throws away all of its
changes to the cards.

In the Hardball 3D run (2026-11-11 and 12, fx 0.0.16, Qwen3.8-Flash-Next
on Sushi 1.6.0) four replans in a row wrote a card that amends
`T002.state`, which no conflict named. Each was rejected, and with it
the valid card that amends `T009.physics` for blocked T010. The last
was the one replan a run gets when no task is ready, and the run ended
BLOCKED. A replan is now told why the last one was rejected, but only in
the next replan, about 20 minutes later, and the run may have none left.

## Options

### A. Keep the valid cards, drop the bad ones

- The good card for T010 would have been kept.
- Cards depend on each other: `depends`, a blocked task reopened behind
  its amending card, the requirements a dropped card gave away. Tenaz
  would decide which part of a plan survives, which is the planner's
  call, and could keep a plan that no longer holds together.

### B. Tell the session before it ends

- When a planner session has answered every prompt, Tenaz judges its
  cards as it will at the end. If they have errors, it sends one more
  prompt with them, as it already does for a replan's refused reopens
  (ADR 0018). The session fixes them while it still holds its plan.
- Cards still wrong after that are thrown away, as now.
- One more turn per invalid planner session.

### C. Only tell the next replan

- Done already. It needs another whole session, and a run that has used
  its replan when no task is ready ends first.

## Decision

Option B. `Runner::planner_errors`
(`crates/core/src/orchestrator/phases/planner.rs`) judges the cards for
both the follow-up prompt and the end of the session, so the two cannot
disagree. The prompt is sent once per harness session, before the one
about refused reopens; a split gets it too. Its list of errors is cut
like the notes.

## Consequences

- A planner that fixes its cards when told keeps all of them the first
  time.
- A planner that does not still loses all of them; the next replan is
  told why.
- A session the watchdog aborted, for its turns or anything else, is not
  asked; one only told to wrap up still is.
- A split whose cards are valid but leave fewer than two tasks is still
  thrown away without being told: that rule is checked after the session,
  not by the cards' judgement.
