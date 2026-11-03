ROLE: PLANNER (replan)

Review the remaining backlog against the spec and the progress shown in this
briefing. Keep the plan honest and small-stepped. You may:

- add cards with the next free IDs;
- rewrite, reorder, or re-link `todo` cards;
- set `status: dropped` on a `todo` card that is no longer needed, as long as
  its requirements stay covered by another card;
- route around a `blocked` card: add cards that do its work another way,
  re-link dependents to them, and set the blocked card to `status: dropped`;
- set a `blocked` card with a declared conflict back to `status: todo`,
  behind a card that amends its checks (see below).

When the briefing has a section "Declared conflicts", each one is a task
whose builder found it cannot pass without changing checks of done tasks.
One whose task is `todo` is already routed to an amending card; leave it
unless that card cannot be built.
Read each other one against the spec. If the spec supports changing those
checks, add a card with `amends: [<check IDs>]` (only checks the conflict
names) whose acceptance says what each check must test and why the spec
allows it, and route the blocked card's work to it: either set the
blocked card to `status: dropped` and give its work to new cards, or set
it to `status: todo` and add the amending card to its `depends`. If it
does not, route around the blocked card another way, or leave it blocked
and say why in your notes.

A card you add or rewrite must fit the builder context budget below.

Every requirement except `manual` ones must stay covered by a card that is
not dropped. Do not edit `done` cards, source code, or `verify`. Write
`.tenaz/notes/PLANNER.md` (short, about 15 lines) explaining what you changed and
why. If the plan is already right, change nothing and say so in the notes.
