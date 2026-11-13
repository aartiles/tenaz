# ADR 0018: A blocked task reopened behind an amend

Status: accepted, 2026-11-03.

## Context

In a run with Qwen Code 0.26.6 and Qwen3.8-27B on 2026-11-03, T002's
builder declared a conflict with `T001.runs` (ADR 0009). T002 was blocked
and a replan ran. It added T008 with `amends: [T001.runs]` and set T002
from `status: blocked` back to `status: todo` with
`depends: [T001, T008]`: amend the check first, then do T002's work.

Tenaz rejected that backlog. A conflict counted only while its task was
blocked or dropped, so once T002 read `todo` its conflict no longer
licensed T008's `amends`: "T008: amends `T001.runs`, which no file in
.tenaz/conflicts/ names". The next replan wrote the same plan, was
rejected the same way, and the run ended BLOCKED.

The replan prompt says to "route the blocked card's work" to the amending
card. Reopening the blocked card behind it is a fair reading, and often
the better plan: the amending card stays small, and the blocked card keeps
its own acceptance. Even had the conflict counted, the backlog would not
have worked: a planner may only drop a blocked task, so Tenaz would have
put T002 back to `blocked`, where nothing builds it.

## Options

### A. Accept the reopened card

- A conflict also stands while its task is `todo` and another card still
  to build (`todo`, `active` or `blocked`) amends one of the checks it
  names. A conflict file of a task in any other state is left from an
  earlier attempt (as when a person reopens a blocked task), or its amend
  has been made, and licenses nothing.
- A planner may change a card from `blocked` to `todo` while its
  conflict stands that way. The reopened task starts with no attempts, as
  a new card does.
- A task whose conflict stands is not built until no card still to build
  amends its checks, whatever its `depends` say. Its next attempt removes
  the conflict, so no `amends` is left that it licensed.
- Both shapes a model writes work. The rule stays narrow: a reopen needs
  an amending card of its own conflict, and that card's `amends` is still
  checked against the conflict.

### B. Tell the planner to drop the blocked card

- The replan prompt says plainly to set the blocked card to `dropped`, and
  the next replan is told why the last backlog was rejected.
- A prompt is read, not enforced. The model wrote the same plan twice with
  the rejection in its journal, so a clearer sentence may not be enough. A
  dropped card also moves all its work onto the amending card, which makes
  that card bigger.

## Decision

**A**, in `Conflict::stands_in` (`crates/core/src/conflict.rs`),
`LoadedBacklog::ready` (`crates/core/src/backlog.rs`) and
`judge` with `reopen_fix`
(`crates/core/src/orchestrator/phases/planner/judge.rs`), written by
`Runner::put_statuses_right` (`planner.rs`). Only a replan may
reopen; a split is not shown the conflicts. The replan prompt and
`protocol.md` name both shapes: drop the blocked card, or set it back to
`todo` depending on the amending card.

Tenaz now puts right the statuses a planner may not set before it judges
its cards, and writes them only when it keeps the cards. A blocked task
the planner set to `done` or `active` is blocked again, its conflict
still stands, and the amending card is accepted instead of rejected with
"no file in .tenaz/conflicts/ names" its check. A reopen is judged last,
against the statuses the others keep: an amending card the planner wrote
as `done` is `todo` by then.

Later (2026-11-12), part of B was added too. In the Hardball 3D run four
replans in a row were rejected for the same card, and none was told why:
the journal summary named no errors, and later entries pushed it out of
the recent journal. A replan now gets the last rejected replan's errors
in "Why the last replan was rejected", until a replan is accepted.

## Consequences

- The reopened task waits for every card still to build that amends its
  checks, not only the ones in its `depends`: a later replan may re-link
  it, add a second amending card, or split the amending card (its
  children keep `amends`, `split.md`), and it still waits. A `depends`
  that would wait for the amending card is still the plan to write, as
  the prompt says.
- `amends` is checked only on cards that may still be built: a done,
  dropped or split card's `amends` licenses nothing more, and does not
  make the backlog invalid once the conflict is gone.
- A conflict of an `active` task does not stand: the attempt removed the
  file it had, and one there now is that attempt's, read when it ends. A
  task does not amend its own conflict, for the same reason.
- A conflict file whose checks do not belong to done tasks does not
  stand either (`Conflict::validate`), so a file never read because the
  last attempt passed `verify` licenses nothing.
- A task reopened when no card still to build amends its checks is set
  back to `blocked`, as any status a planner may not set is. So a task
  whose new attempt declares the same conflict again cannot be reopened
  over and over without a new amending card. The replan is told before
  it ends: a replan session that answered every prompt having reopened
  such a task gets one more prompt, once per session, which asks it to
  add the `amends` or route around the task. It is judged as the end of
  the replan judges it, with the other statuses put right. A session the
  watchdog stops is not asked; a continuation or retry of the replan is
  asked again.
- Why the prompt: a replan of the articulated-cabinet run (2026-11-05)
  reopened a task behind a card that fixed the geometry but named no
  `amends`. Tenaz blocked the task again in silence, and the run found
  no task ready and blocked.
- A conflict that names two checks stands behind a card that amends only
  one of them. The reopened task may then meet the other and declare the
  conflict again; the next replan sees it.
- "Declared conflicts" in a replan's briefing also lists the conflict of
  a reopened task until its amend is built. The prompt says such a task
  is already routed.
- Tested with the scripted harness: a replan that reopens the blocked
  task with and without the amending card in its `depends` or with the
  amending card written as `done`, and one that marks the task done. Not
  yet seen against a real model.

Known gaps, older than this change or wider than it:

- The wait for an amend is not in `depends`, so the cycle check does not
  see it. An amending card that depends on the task it unblocks, or one
  that is itself blocked, leaves nothing ready; the replan that follows
  sees the task as `todo` but not why it waits.
- A status Tenaz puts right is not reported to the planner. A reopen it
  undoes leaves the task blocked with nothing in the journal to say why.
- A planner may write files in `.tenaz/conflicts/`, so it can declare a
  conflict no builder found and license its own `amends`. This was
  possible through a dropped task before; nothing checks a conflict
  against the journal's `conflict` records. Likewise, once the amend is
  built, a reopened task's conflict file stays until its next attempt; a
  replan that adds another card amending the same check in between is
  licensed by it again.
  (2026-11-13: the code does not allow it. A planner's writes to
  `.tenaz/conflicts/` are reverted as out of its role; only a builder
  writes its own task's conflict. In the resumed Hardball 3D run a
  replan tried it, and the revert left its `amends` unlicensed.)
