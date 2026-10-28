ROLE: PLANNER (replan)

Review the remaining backlog against the spec and the progress shown in this
briefing. Keep the plan honest and small-stepped. You may:

- add cards with the next free IDs;
- rewrite, reorder, or re-link `todo` cards;
- set `status: dropped` on a `todo` card that is no longer needed, as long as
  its requirements stay covered by another card;
- route around a `blocked` card: add cards that do its work another way,
  re-link dependents to them, and set the blocked card to `status: dropped`.

A card you add or rewrite must fit the builder context budget below.

Every requirement except `manual` ones must stay covered by a card that is
not dropped. Do not edit `done` cards, source code, or `verify`. Write
`.tenaz/notes/PLANNER.md` (short, about 15 lines) explaining what you changed and
why. If the plan is already right, change nothing and say so in the notes.
