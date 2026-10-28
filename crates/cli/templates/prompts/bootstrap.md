ROLE: PLANNER (bootstrap)

Turn `.tenaz/brief.md` into a plan that fresh, context-limited sessions of
the same model can carry out one small task at a time. The bootstrap writes
only the plan and a minimal `verify`. Do not build the project or implement
features: the first task cards do that.

The bootstrap runs in three steps, one session each: the spec and map, then
`verify`, then the task cards. This session does only the step below; the
next session does the next step from the files you leave. Write each file as soon as you know what goes in it, with a tool call. Do not
draft the files in your reasoning first, and do not plan the other steps.

## Step 1: spec and map

Write:

1. `.tenaz/spec.md`: every requirement of the brief as one numbered line,
   `- R1: ...`. Use one testable statement per line and keep the brief's
   non-negotiable rules. Mark a requirement that cannot be checked
   automatically as `- R9: ... (manual: why)`.
2. `.tenaz/map.md`: one line per source file or directory the project will
   have, giving its purpose and key exports. Include how to run the project
   and `verify`.

Write the spec first, then the map. Then end the session.

## Step 2: verify

The spec and map from step 1 are below. Write:

1. `verify`: one small file, an executable that follows the contract in the
   protocol, with one `T000.` smoke check that passes now (for example, that
   the tools the checks will need are installed). Choose the simplest
   tooling that can test this kind of project (shell, node, python, a
   headless browser for web UIs). Later tasks add their checks as files in
   `checks/` that `verify` runs; write no other checks now, and no project
   files.
2. `.tenaz/notes/BOOTSTRAP.md` (short, about 15 lines): stack, project
   layout, how `verify` finds and runs the checks in `checks/`, and
   decisions that later sessions must respect.

Write `verify` first. Before ending, run `./verify --all` twice. Both runs
must pass and print the same final JSON line.

## Step 3: task cards

The spec and map are below; `verify` exists. Write `.tenaz/tasks/T001.md`,
`T002.md`, and so on, using the card format in the protocol. Write each
card as soon as you have decided it.

Task sizing:

- Each task changes about 1–4 files and roughly 50–300 lines. If its
  acceptance does not fit in 3 bullets, split it.
- Each card fits the builder context budget below.
- T001 builds the project skeleton the map describes, with a check that
  it runs. Reach a minimal end-to-end version early, then extend it task
  by task.
- Each acceptance names the check IDs the task will add (`T007.<name>`).
- `depends` lists only real prerequisites.
- Every requirement except `manual` ones appears in some task's `requires`.
- Plan between 6 and 40 tasks. Number them in the order you expect them to
  be built.

Before ending, run `./verify --all` and check that it still passes.
