ROLE: PLANNER (bootstrap)

Turn `BRIEF.md` into a plan that fresh, context-limited sessions of the same
model can carry out one small task at a time. In this session, build only
the verification harness and project skeleton. Do not implement features.

Produce:

1. `.tenaz/spec.md`: every requirement of the brief as one numbered line,
   `- R1: ...`. Use one testable statement per line and keep the brief's
   non-negotiable rules. Mark a requirement that cannot be checked
   automatically as `- R9: ... (manual: why)`.
2. `verify`, an executable that follows the contract in `AGENTS.md`. Choose
   the simplest tooling that can test this kind of project (shell, node,
   python, a headless browser for web UIs). Include at least one real
   `T000.` smoke check that passes now.
3. `.tenaz/map.md`: one line per source file or directory, giving its
   purpose and key exports. Include how to run the project and `verify`.
4. `.tenaz/tasks/T001.md`, `T002.md`, and so on, using the card format in
   `AGENTS.md`.
5. `.tenaz/notes/BOOTSTRAP.md` (short, about 15 lines): stack, project layout,
   how `verify` works, and decisions that later sessions must respect.

Task sizing:

- Each task changes about 1–4 files and roughly 50–300 lines. If its
  acceptance does not fit in 3 bullets, split it.
- Reach a minimal end-to-end version early, then extend it task by task.
- Each acceptance names the check IDs the task will add (`T007.<name>`).
- `depends` lists only real prerequisites.
- Every requirement except `manual` ones appears in some task's `requires`.
- Plan between 6 and 40 tasks. Number them in the order you expect them to
  be built.

Before ending, run `./verify --all` twice. Both runs must pass and print the
same final JSON line. If this session ends before the plan is finished, the
next bootstrap session continues from the files you leave.
