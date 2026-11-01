# ADR 0013: A split child corrects its ancestors' notes

Status: accepted, 2026-10-31.

## Context

In the Digger below the Mine run (2026-10-30, Qwen3.8-Flash-Next 2.6bpw,
fx) T021 was split into T021a to T021d, and T021d into T021da to T021dd.
`.tenaz/notes/T021.md` was still the 94-line note of T021's third attempt.
Its key colours and percentages no longer matched the tool T021da shipped,
and T034, the task that uses the tool, was to read it as its handover.

The split gave T021dd one job: rewrite `.tenaz/notes/T021.md` under 40
lines, with the numbers the shipped tool reports. It also wrote the check
`T021dd.note-handover`, which reads that file. A builder may write only its
own task's notes, so after each of T021dd's sessions Tenaz reverted
`.tenaz/notes/T021.md`. Its attempts rewrote the note, and `verify` saw
the old 94 lines each time. At the default `max_split_depth` of 2 a task
at split depth 2 is not split again, so after its last attempt T021dd is
blocked, and T034, which depends on it, with it.

## Options

### A. The split may not write such a card

Tenaz rejects a split whose card lists, or whose check reads, a `.tenaz/`
path the child's builder may not write. The parent's stale note stays.
Later tasks that open it read numbers the work has since changed, and no
session may correct them.

### B. A split child's builder may write the notes of the tasks it was split from

`T021dd` may write `notes/T021dd.md`, `notes/T021d.md` and `notes/T021.md`.
A child finishes its parent's work, so the parent's handover is part of
that work. Siblings' notes and other tasks' notes stay out of reach, as do
the parent's card, conflict and heavy job.

### C. The split corrects the parent's note

The split session already reads the attempts' notes, and could rewrite the
parent's. But it has not done the work that changes the facts: when it
runs, T021da had not yet shipped the tool whose numbers the note must carry.

## Decision

B. The notes are the only memory between sessions, and the task that
changed the facts is the one that can correct them. A wrong edit is no
worse than a wrong note of the child's own: the reviewer's diff leaves out
`.tenaz/` for both, and only a check the card adds, like
`T021dd.note-handover`, reads them.

## Consequences

- A correction to an ancestor's note is part of the task's work. When the
  task is split or blocked, it is reverted with the rest; only the task's
  own notes and conflict outlive that.
- Nothing else of the parent opens up: its card, conflict and heavy job
  stay out of the child's reach.
- The ancestors are read from the task's id (`T021dd` from `T021d` and
  `T021`), as the split depth already is, not from the card's `parent:`.
  The split prompt asks for both to agree; nothing checks that they do.
- The briefing does not include the ancestors' notes. A child that must
  correct one opens it, as the card that asks for it names the file.
