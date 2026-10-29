---
name: tenaz
description: Delegate a long, verifiable coding goal to Tenaz, which plans it and drives a local model session by session until the goal is met, on its own branch. Use when the user asks to hand work to Tenaz or to a local model, or to run a goal unattended for hours, and when they want to install Tenaz, set it up or check why it is not ready (tenaz doctor, tenaz demo); not for small edits or work that needs judgment at every step.
---

# Delegating a goal to Tenaz

Tenaz runs a coding agent on a local model for hours: it plans the goal,
splits it into tasks, verifies each one with a `verify` script it writes, and
commits. You stay the one who decides what to build and whether the result
is good enough. `tenaz --help` has the commands, flags and exit codes; this
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
   - `curl -LsSf https://github.com/aartiles/tenaz/releases/latest/download/tenaz-installer.sh | sh`
     installs to `~/.local/bin` and adds it to `PATH`; a new terminal
     picks it up.
   - `brew install aartiles/tap/tenaz` on macOS with Homebrew.
2. `tenaz doctor`. It checks git, a coding agent (Pi or fx), an engine on
   the usual ports (`--base-url` adds another address) and a model with at
   least 32k tokens of context, prints `ok`, `check` or `missing` for each
   with the fix under it, and exits with 1 while something is missing.
   Its `images` line is optional: it names an image engine it found and
   the `--images` or `--comfyui` flags that give it to a run.
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
`.tenaz/notes/`. A task whose builder found it cannot pass without changing
a check of a done task leaves `.tenaz/conflicts/<TASK>.md`; a replan may
then add a card whose `amends` lets its builder change those checks.
A builder may declare a heavy job (a render, a long build) in
`.tenaz/jobs/<TASK>.json`; Tenaz runs it between sessions with the engine's
memory freed, logs it to `.tenaz/private/jobs/`, and journals
`job-started` and `job-finished`.
To see why a session went wrong, read its briefing and,
with fx, the model's reasoning: `.tenaz/private/<label>.prompt.md` and
`<label>.thinking.md` (the label is in `tenaz status`). Fix what you can (an
engine down, an unclear brief: tell the user, since the brief cannot change
after `init`), then `tenaz run --project <dir> --resume`; after updating
Tenaz, add `--refresh-templates` so the run gets the new prompts. Do not
edit `.tenaz/run.json` or the task cards to force progress.

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
