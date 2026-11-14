---
name: tenaz
description: Delegate a long, verifiable coding goal to Tenaz, which validates your bootstrap and drives a local model session by session until the goal is met, on its own branch. Use when the user asks to hand work to Tenaz or to a local model, or to run a goal unattended for hours, and when they want to install Tenaz, set it up or check why it is not ready (tenaz doctor, tenaz demo); not for small edits or work that needs judgment at every step.
---

# Delegating a goal to Tenaz

Prepare the brief and bootstrap yourself, then let Tenaz run a coding
agent on a local model for hours to build the tasks, verify them and commit.
Tenaz can also plan with the local model when you leave the bootstrap to it.
You stay the one who decides what to build and whether the result is good
enough. `tenaz --help` has the commands, flags and exit codes; this
file is about the judgment around them. If a command named here is not in
`tenaz --help`, the installed Tenaz is older than this skill: tell the user
to update it the way they installed it.

## When to delegate

Delegate when the goal is long, can be checked by running the program, and
does not need a decision at every step: a feature with clear behaviour, a
port, a batch of similar changes, a test suite. Do the work yourself when it
is small, when it hinges on taste or on knowledge the brief cannot carry, or
when the user wants it now: a run takes hours and holds the machine's memory.

## Set it up

Do this once, and again whenever a run fails before its first session.

1. `tenaz --version`. If it is not found, give the user one of these and
   let them run it (it changes their machine):
   - `brew install aartiles/tap/tenaz` on macOS with Homebrew.
   - On Apple silicon macOS or Linux x86_64, the archive for their
     platform from the
     [releases page](https://github.com/aartiles/tenaz/releases), with
     its SHA-256 checksum.
     `gh attestation verify <archive> --repo aartiles/tenaz` checks it
     was built by the release workflow. They unpack it and put `tenaz`
     on their `PATH`.
   - `cargo install --locked --git https://github.com/aartiles/tenaz tenaz`
     with Rust 1.91 or later, on any platform.

   The README's Install section has every way, with what each needs. Do
   not download and run an install script yourself.
2. `tenaz doctor`. It checks git, a coding agent (Pi, fx, OpenCode or Qwen
   Code), an engine on the usual ports (`--base-url` adds another address)
   and a model with at least 32k tokens of context, prints `ok`, `check`
   or `missing` for each with the fix under it, and exits with 1 while
   something is missing.
   Its `images` line is optional: it names an image engine it found and
   the `--images` or `--comfyui` flags that give it to a run. Its `3d`
   line is optional too: a 3D model (Idol, TRELLIS.2) an engine lists,
   and the `--meshes` and `--mesh-model` flags that give the agent
   `generate_3d`, which turns a picture inside the project into a GLB.
   Work through the fixes in order:
   - git or the agent missing: the fix is an install command. Show it and
     ask before running it; installing software is the user's call.
   - no engine, no model, or too small a context: the user starts the
     engine, loads the model and sets its context (doctor names the
     setting for LM Studio, Ollama, llama.cpp and vLLM). Do not start,
     stop or reconfigure engines yourself.
   - `check` on the model means the engine does not report its context
     (Ollama): ask the user what it runs with, and pass it to `init` as
     `--context-window`.
   Run `tenaz doctor` again until it says Ready. Whether the model calls
   tools is checked by `tenaz run` before its first session, not here.
3. To show the user what a run looks like before the setup is done,
   `tenaz demo --project <empty scratch dir>` runs a scripted goal with no
   model and no agent in well under a minute. Put it outside the user's
   repository. It proves nothing about their engine or model.

For the `init` line, take the engine address and the model id from what
doctor found, or ask the user when it lists several. A person at a
terminal can instead run `tenaz init` with no `--model` and answer the
setup wizard's questions; you cannot, because it needs a terminal.

When the user wants another model for some roles, for example a reviewer
of another family, add `--model-for reviewer=<id>` (repeatable, one role
each) and `--context-window-for reviewer=<tokens>` when its window
differs; roles on the same model need the same window. The engine must
have every model named loaded at once, unless they do not fit together:
then add `--swap-models` (only with `--engine load-api`, Sushi or
mlx-serve), and Tenaz loads one at a time, swapping before each session.
With `--harness pi`, a role's window and inputs are read from the user's
`~/.pi/agent/models.json` when it lists the model.

Doctor finds Pi, fx, OpenCode and Qwen Code on PATH. For DeepSeek
Harness pass `--harness dsh --dsh-bin <path>`. Any other ACP agent runs
with `--harness acp --acp-bin <command>` and `tenaz run --acp-args
"<args>"`; the command reads the session file `TENAZ_AGENT_SESSION`
names (README, "Any other ACP agent").

## Write the brief

Inspect the project instructions, source, interfaces, dependencies and tests
first. Write a brief that says:

- the goal as observable behaviour: what a person can run and see when it is
  done;
- what must not change: files, interfaces, behaviour, dependencies;
- how the project is built and tested today, with the exact commands;
- constraints that are not obvious from the code: versions, platforms,
  performance limits, style rules that matter.

Keep it to one page. Put the concrete plan in the bootstrap after `init`.
Write the brief outside the repository so it does not dirty the checkout.

## Initialise the run

1. `tenaz init --project <repo> --brief <brief-path> --model <id> ...` from the
   user's repository. In a repository with commits it works on a new branch in
   a worktree and prints that directory: pass it as `--project` from then on.
   `init` refuses uncommitted changes; ask the user before committing theirs.
   `--mcp name=url` offers the agent an MCP server of the user's own that
   speaks HTTP (repeat it for more; fx, OpenCode, Qwen Code or `--harness
   acp`, not Pi or dsh). The user starts that server; `tenaz run --no-mcp`
   drops the pinned ones. `--set turnBudget=50` changes the turn budget
   (40) of builder, bootstrap, replan and split sessions; reviewers keep
   25 turns and the final review 60. `--set turnAbort=2` stops every
   session at twice its budget (1.5 by default).
2. Before starting `run`, prepare the bootstrap below in the directory
   `init` printed. For a repository with commits, confirm it with
   `git worktree list`; do not guess the directory from the goal name. For a
   new project, use the project directory `init` initialised.

## Prepare the bootstrap

Do this before the first `run`, while no runner is active. You write only
the bootstrap, leaving implementation to the local model.

1. Run `tenaz status --project <dir>` (text output). Read `Model`, any
   `Role models` entry for `builder`, and `Context budget`. Confirm the
   builder's model and configured context window from setup; with Pi,
   account for the window taken from `models.json`. Do not size tasks until
   these are known. Use the reported builder budget, not the full window.
2. Read the run's `.tenaz/protocol.md` and `.tenaz/prompts/bootstrap.md`.
   These pinned files define the current formats. In an existing project,
   follow the existing-project rules: map only the affected code, keep the
   current stack, and plan only as many tasks as the change needs.
3. Write `.tenaz/spec.md` with one observable requirement per `- R1: ...`
   line. Preserve the brief's constraints. Use `(manual: why)` for a
   requirement that needs human judgment. Write `.tenaz/map.md` with the
   affected files, their purpose and key exports, and exact run and test
   commands. Distinguish existing code from planned changes.
4. Write the executable `verify` and short `.tenaz/notes/BOOTSTRAP.md`.
   In an existing project use `.tenaz/verify`, wrapping its own test suite;
   put new behaviour checks in that suite. In a new project use root
   `verify`, with checks in `checks/`. Follow the protocol's `--all`,
   `--task <ID>`, exit code and final JSON contract. Bootstrap `T000.`
   checks must test something true now: existing tests or an actual tooling
   smoke check. Run `--all` twice from the run directory with
   `CI=1 NO_COLOR=1`; both runs must pass with the same final JSON and
   leave the files `git status` lists unchanged. List in `.gitignore` the
   folder where checks write what they produce (screenshots, reports).
   This proves only the initial baseline. Future requirements stay in
   task acceptance criteria; builders add their real `<TASK>.` checks
   while implementing them. Do not add constant-success checks, count
   future checks as passed, or weaken existing tests to get a green baseline.
5. Write `.tenaz/tasks/T001.md`, `T002.md`, and so on in the protocol's
   frontmatter and `Goal`, `Acceptance`, `Context` format. Start at
   `status: todo`; list requirement IDs in `requires`, real prerequisites
   in `depends`, and concrete source and test paths in `files`. Cover every
   non-manual requirement. In `Context`, resolve design decisions now:
   name functions, interfaces and data shapes, error behaviour, dependencies
   and versions, and which earlier task supplies what. Give exact acceptance
   behaviour and the check IDs the builder will add. Do not leave the local
   model to choose the stack, architecture or interfaces you can settle.
   Keep shared decisions in `BOOTSTRAP.md` (about 15 lines).
6. Estimate each card's required reading and writing, including called code
   and tests. Keep it within about two thirds of the builder context budget
   (roughly four characters per token), leaving room for prompts, tool
   output and turns. Aim for 1–4 files, about 50–300 changed lines and at
   most three acceptance bullets; split larger work before starting. For a
   new project, reach a minimal end-to-end version early.
   A builder is also stopped at 60 turns by default (see `turnBudget`
   above), and a fresh session carries on from its changes, up to
   `contextContinuations` (3) times. A card can set its own budget with
   `budget: {turns: 30}`.
7. Check requirement coverage, dependency order and the baseline once more.
   Leave `run.json`, `journal.jsonl`, pinned prompts and protocol untouched.
   Do not set phases, checkpoints or task statuses to skip validation.

## Run it

1. `tenaz run --project <dir>` in the background. Do not wait on it in the
   foreground and do not start a second one on the same project.
2. Check `tenaz status --project <dir> --json` now and then, not in a tight
   loop: sessions take minutes. `state` says `running`, `paused`, `blocked`,
   `interrupted`, `complete` or `complete_with_gaps`.
3. When `run` exits, its code says how it ended (`tenaz run --help`).

`run` still performs its normal preflight and validates each bootstrap
step. A valid prepared step opens no bootstrap session; with all three
valid, the local model starts building after preflight. Missing or invalid
files follow the existing local bootstrap repair and retry path, which can
block. To let the local model prepare the whole bootstrap instead, skip
"Prepare the bootstrap" and run immediately after `init`.

When it is blocked, read `blocked_reason` and the newest notes in
`.tenaz/notes/`. A reason that starts `prompt too large:` names the
role, its model and that model's window, and says what to change: for a
review, `params.diffShare` or `params.diffLimitBytes` in
`.tenaz/run.json`; otherwise tell the user the role needs a model with
a larger window, which takes a new run. A task whose builder found it
cannot pass without changing a check of a done task leaves
`.tenaz/conflicts/<TASK>.md`; a replan may then add a card whose
`amends` lets its builder change those checks, and set the blocked task
back to `todo` to be built after that card.
A builder may declare a heavy job (a render, a long build) in
`.tenaz/jobs/<TASK>.json`; unless `jobMinutes=0`, Tenaz runs it between
sessions with the engine's memory freed where the engine can, logs it to
`.tenaz/private/jobs/`, and journals `job-started` and `job-finished`.
While a job runs no session is in flight (`inflight` is null in
`tenaz status --json`): `.tenaz/private/orchestrator.log` names the job,
and its log grows in `.tenaz/private/jobs/`.
To see why a session went wrong, read its briefing and, with fx, OpenCode
or Qwen Code, the model's reasoning: `.tenaz/private/<label>.prompt.md`
and `<label>.thinking.md` (the label is in `tenaz status`). Fix what you
can (an engine down, an unclear brief: tell the user, since the brief
cannot change after `init`), then `tenaz run --project <dir> --resume`;
after updating Tenaz, add `--refresh-templates` so the run gets the new
prompts. A run blocked with no task ready gets a replan again on
`--resume`. Do not edit `.tenaz/run.json` or the task cards to force
progress; the one edit a block reason asks for is the exception.

## Check the result

A complete run means the checks Tenaz wrote pass, not that the user's goal is
met. Before reporting success:

1. `tenaz finish --project <dir>` puts the result in one commit on a new
   branch of the original repository; nothing is pushed.
2. Read that commit (`git show <branch>`) as you would a colleague's pull
   request, run the project's own tests, and try the behaviour the brief asked
   for.
3. Tell the user what was delivered, what you checked, and what is missing or
   doubtful. With `complete_with_gaps`, say which requirements are open.

Merging, pushing and removing the worktree are the user's call.
