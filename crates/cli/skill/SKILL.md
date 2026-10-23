---
name: tenaz
description: Delegate a long, verifiable coding goal to Tenaz, which plans it and drives a local model session by session until the goal is met, on its own branch. Use when the user asks to hand work to Tenaz or to a local model, or to run a goal unattended for hours; not for small edits or work that needs judgment at every step.
---

# Delegating a goal to Tenaz

Tenaz runs a coding agent on a local model for hours: it plans the goal,
splits it into tasks, verifies each one with a `verify` script it writes, and
commits. You stay the one who decides what to build and whether the result
is good enough. `tenaz --help` has the commands, flags and exit codes; this
file is about the judgment around them.

## When to delegate

Delegate when the goal is long, can be checked by running the program, and
does not need a decision at every step: a feature with clear behaviour, a
port, a batch of similar changes, a test suite. Do the work yourself when it
is small, when it hinges on taste or on knowledge the brief cannot carry, or
when the user wants it now: a run takes hours and holds the machine's memory.

Tenaz needs an engine serving a model with tool calling (`tenaz init` without
`--model`, in a terminal, finds one). `tenaz doctor` says whether git, a
coding agent, an engine and a model with enough context are there, and exits
with 1 when something is missing. If no engine is running, ask the user to
start one; do not start or stop engines yourself. `tenaz demo` runs a
scripted goal with no model, in about a minute, to show the user what a
run looks like; it does not check the setup.

## Write the brief

The brief is all the local model gets besides the code, and it is smaller
than you. Read the code first, then write a brief that says:

- the goal as observable behaviour: what a person can run and see when it is
  done;
- what must not change: files, interfaces, behaviour, dependencies;
- how the project is built and tested today, with the exact commands;
- constraints that are not obvious from the code: versions, platforms,
  performance limits, style rules that matter.

Leave out the plan: Tenaz makes its own. Keep it to one page.

## Run it

1. `tenaz init --project <repo> --brief brief.md --model <id> ...` from the
   user's repository. In a repository with commits it works on a new branch in
   a worktree and prints that directory: pass it as `--project` from then on.
   `init` refuses uncommitted changes; ask the user before committing theirs.
2. `tenaz run --project <dir>` in the background. Do not wait on it in the
   foreground and do not start a second one on the same project.
3. Check `tenaz status --project <dir> --json` now and then, not in a tight
   loop: sessions take minutes. `state` says `running`, `paused`, `blocked`,
   `interrupted`, `complete` or `complete_with_gaps`.
4. When `run` exits, its code says how it ended (`tenaz run --help`).

When it is blocked, read `blocked_reason` and the newest notes in
`.tenaz/notes/`. Fix what you can (an engine down, an unclear brief: tell the
user, since the brief cannot change after `init`), then
`tenaz run --project <dir> --resume`. Do not edit `.tenaz/run.json` or the
task cards to force progress.

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
