# ADR 0009: A card that conflicts with a done check

Status: accepted, 2026-10-29.

## Context

In the Lotion Pump run (2026-10-28, Qwen3.8-Flash-Next 2.6bpw, fx) the
final review found the backdrop too dark for the brief and added T016.
T016 was split. T016a reworked the look checks, with an edge-gradient
floor (`look_edge_grad_min` 0.015). T016b had to make the wall light gray to white, with thresholds its
card set: at most 10 % of the wall below 0.55, at least 2 % above 0.90,
and a gradient floor.

The three builders of T016b measured that these cannot hold
together. The 0.55 low stop keeps the 5th percentile at or
above 0.576. The gradient floor then needs a white region about twice
the 2 % asked for, and that much white drops the edge gradient under
T016a's floor. Each builder wrote this in its notes, put the scene back
where it started so the other 25 checks kept passing, and stopped. The
last one ended: "the four thresholds are jointly binding and the task as
written cannot pass without relaxing a threshold".

None of those thresholds is in the spec. R15 says only "luminance
spanning roughly 0.55 to 1.00 across the visible backdrop". The numbers
were chosen by the final reviewer, the split and T016a's builder.

The builder prompt says: "If the task cannot be done as written, explain
why in your notes and stop. The orchestrator will replan it." Tenaz does
not do that. The attempt fails like any other. After three of them the
task is split, and the split is told "do not edit `done` cards, source
code, or `verify`". `protocol.md` tells every role "never delete or
weaken an existing check". So no session may change the check that is
in the way. The split wrote T016d, which only measures, and T016e, which
must meet the same thresholds and says "nothing may be weakened". T016d
passed; its builder swept the levers again and wrote that no lever
reaches the targets and that "those targets may be unreachable". Since
91f672a the split at least sees the builders' notes. It still may not
act on them.

## Options

### A. Leave it

- The split sees the notes and may choose other thresholds for the new
  cards. It may not change T016a's floor.
- A real conflict ends with the task blocked after up to three more
  attempts per split level. T016b alone took three hours. A blocked
  task fails the goal.
- Fix the builder prompt so it no longer promises a replan.

### B. The builder declares a conflict, a planner may amend named checks

- The builder writes `.tenaz/conflicts/<TASK>.md`: the check IDs in the
  way, the requirement each one serves, and what it measured. It writes
  nothing else and stops.
- Tenaz checks that the file names checks that exist, then takes the task
  out of its attempt loop at once and starts a replan with the file.
  Without a valid file the attempt fails as now.
- The replan may write one card with `amends: [<check IDs>]`. Its builder
  may change those checks, and only those, as long as each still tests its
  requirement. The reviewer is told which checks were amended and judges
  every change against the spec, not against the old number. The count of
  passed checks may not drop.
- This keeps "never weaken a check" for everyone else. The spec decides
  between two invented thresholds, and a session that cites the spec
  decides, not the builder that is stuck.
- Risk: a model that declares conflicts to escape work. A conflict costs a
  replan session, names its checks, and its amendment is reviewed. The
  journal records it, so a run with many conflicts shows.

### C. The builder declares a conflict, and a person decides

- Same file as B. Tenaz blocks the task with the conflict as its reason
  and runs the rest of the backlog. `tenaz status` and the end-of-run
  message show the conflict.
- Nothing weakens a check without a person, but an unattended run stops
  moving on that goal for hours. In this run that is the whole look.

### D. Let the final reviewer amend checks

- It already judges the whole goal against the brief and adds cards.
- A conflict met by a builder would wait until every other task is done.
  The final review gets two cycles (`maxFinalCycles`). In this run the
  conflict came from a final-review card.

## Decision

**B**, with C as its fallback: when the amending card is blocked too, it
stays blocked, and so does the task, with the conflict in its history. The
builder prompt no longer promises a replan for a task that cannot be done
for any other reason.

The conflict file has front matter `checks: [<check IDs>]` and a free
body. It stands when every check belongs to a done task other than the
builder's own; a task's own checks are its card to fix. It is read after
an attempt fails `verify`. One that does not stand is logged, the next
builder is told why, the file is removed, and the attempt counts as usual;
a task that passes has its conflict file removed too. One that stands: the
task's work is reverted with its notes and conflict kept, the task is
blocked, the conflict is journaled (`conflict`: task, attempt, checks), and
a replan runs. Every replan is shown the conflicts that stood, those of
blocked or dropped tasks, under "Declared conflicts", so one after a pause
or a rejected replan still sees them; only those conflicts license an
`amends`. `amends` is checked whenever the
backlog is loaded, for cards that are not done: a card naming a check no
conflict names is an invalid card, which a replan must repair. The builder
and reviewer of an amending card get a section "Checks this card may
amend" with the checks and the conflicts that named them.

Nothing stops the amending builder from changing other checks except the
prompts, its review and the floor on passed checks, as for any builder.

## Consequences

- New file, role scope and validation: `.tenaz/conflicts/`, writable by
  the builder of its task. A card field `amends`, and validation that it
  names only checks a conflict names. A new journal record `conflict` that
  scripts can read, kept stable like the others.
- Tenaz does not list a project's checks, so it cannot tell that a named
  check exists: only that it is written like one and belongs to a done
  task. A conflict with a check that does not exist leads to a replan
  that finds nothing to amend.
- `protocol.md`, the builder, replan and reviewer prompts, `SKILL.md` and
  the README change with it.
- Tested with the scripted harness, a builder that declares a conflict and
  a replan that amends, and one whose conflict does not stand; not yet seen
  against a real model. Whether a small
  model declares conflicts at the right times is unknown until a run.
