# Tenaz

Give it a goal; it plans the work, splits it into tasks and drives a coding
agent session by session until the goal is met, on a local model.

Tenaz is built for machines that fit **one model at a time**: a laptop or a
single GPU running a local LLM. Long agent runs on that kind of hardware fail
in predictable ways: the context fills up, the engine runs out of memory or
restarts, the model loops on the same tool call, and an image or speech model
cannot be loaded next to the LLM. Tenaz handles those failures instead of
counting them against the work.

## How it works

```text
 goal ─▶ bootstrap ─▶ ┌─ build task ─▶ verify ─▶ review ─▶ commit ─┐ ─▶ final review ─▶ done
                      └──────────── retry, split, block or replan ◀──┘
```

- **Fresh sessions, state on disk.** Every unit of work is a new agent
  session with a short context. The spec, task cards, notes and progress live
  in the project under `.tenaz/`, and every step is a git commit.
- **The runner decides, not the model.** Tenaz runs the project's `verify`
  script itself, commits or reverts, and decides when to retry, split a task,
  block or replan.
- **Engine failures are not task failures.** A crash, a refused connection or
  a smaller context than expected is retried with backoff without charging
  the task.
- **Watchdogs.** Loops, repeated calls, idle turns and huge outputs get a
  steering message first, then the session is stopped.
- **One resident model.** When the agent asks for an image, Tenaz unloads the
  LLM, runs the image model, frees it and reloads the LLM. The agent's
  requests wait meanwhile instead of failing.
- **Pause and resume.** Ctrl-C pauses after the current session; run again
  to continue.

## What you need

- macOS on Apple silicon or Linux x86_64, and `git`.
- **A coding agent** Tenaz can drive: [fx](https://github.com/vercel-labs/fx)
  (over ACP) or [Pi](https://github.com/earendil-works/pi) (over its RPC mode).
- **An OpenAI-compatible engine** serving a model with tool calling:
  llama.cpp's `llama-server`, LM Studio, Ollama, vLLM, MLX's
  `mlx_lm.server`, TensorFold, Sushi or similar.
  Reasoning and prefix caching are checked before the first session.
- Optional: [ComfyUI](https://github.com/comfyanonymous/ComfyUI) for images.

How much memory depends on the model you choose, not on Tenaz.
[`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) lists what has been
tested and what has not.

## Install

Download the archive for your platform from the
[releases page](../../releases), unpack it and put `tenaz` on your `PATH`:

```bash
tar -xzf tenaz-*.tar.gz && install -m 755 tenaz-*/tenaz ~/.local/bin/tenaz
```

Or build from source with Rust 1.91 or later:

```bash
cargo install --locked --git https://github.com/aartiles/tenaz tenaz
```

## Try it without a model

The scripted harness plays a fixed session script, with engine failures,
watchdog stops and a review rejection, against a real git repository. It
needs no agent and no engine:

```bash
echo "Build four trivial files." > brief.md
```

```bash
tenaz init --project demo --brief brief.md --harness fake --model fake --provider fake --set engineRetryBaseSec=0 --set notify=false
```

```bash
tenaz run --project demo
```

Then look at `demo/.tenaz/` and `git -C demo log --oneline`.

## Run it on a real goal

Start your engine with a model loaded, then:

```bash
tenaz init
```

It asks what to build, finds the coding agents on your `PATH`, the engines
on their usual ports (Sushi 12345, llama.cpp 8080, LM Studio 1234, Ollama
11434, vLLM 8000) with the models they serve and their context windows,
and ComfyUI on 8188 when it runs. It shows what it found, prints the same
setup as a `tenaz init` command line for next time, and creates the project.
Then, from the project directory:

```bash
tenaz run
```

```bash
tenaz status
```

- `init` creates the project (or uses the directory you point it at),
  writes the goal to `.tenaz/brief.md`, adds the working protocol
  (`.tenaz/protocol.md`) and role prompts (`.tenaz/prompts/`), pins the
  model, the agent, how to start the engine and the run parameters in
  `.tenaz/run.json`, and makes the first commit. Everything it writes is
  under `.tenaz/`, with its own `.tenaz/.gitignore`; the project's
  `AGENTS.md`, `CLAUDE.md` and `.gitignore` are left as they are.
- `run` reads all of that from `run.json`, probes the engine, then works
  until the goal is met or a task blocks. A launch flag given to `run`
  (`--comfyui`, `--engine-start`, ...) replaces the pinned value and is
  saved. `--resume` continues a blocked run after you have fixed what
  blocked it.
- `status` reads files only and is safe while a run works.
- `tenaz watch` follows the run as it works, session by session: with
  `--level summary` the run log only, with `actions` (the default) also each
  tool call and model reply on one line with the context it used and the
  decode speed, with `full` whole replies and the start of tool results, and
  with `debug` the harness's own log too. It reads files only; Ctrl-C stops
  watching, not the run.

> **Tenaz commits to the project's repository.** Every step of a run is a
> commit, and a failed attempt is undone with `git reset --hard` and
> `git clean -fd`, which deletes untracked files that are not ignored. So
> on a repository that already has commits, `init` leaves your checkout
> alone: it creates a branch `tenaz/<goal>` from your HEAD, checks it out in
> a worktree next to the repository (`../<repo>-tenaz-<goal>`; `--worktree`
> and `--name` change them), records the starting branch and commit in
> `.tenaz/run.json`, and the run works there. `run` and `status` say where,
> and point to the worktree when started from your checkout. `init` refuses
> a tree with uncommitted changes (an untracked `.tenaz/brief.md` is taken
> as the brief) rather than leave them behind. `--in-place` keeps the old
> behaviour: the run commits on your current branch, and its first commit
> takes everything uncommitted (`git add -A`). A new or empty directory
> becomes a new repository, as before.

> **By default Tenaz does not sandbox the agent.** The agent runs the shell
> commands and writes the files the model asks for, as your user, and Tenaz
> approves every permission request it makes; the project's `verify` script
> runs the same way. A model that misreads the goal can touch anything your
> user can. `init --set sandbox=true` (or `"sandbox": true` in the params
> of `.tenaz/run.json`) limits that: see [Sandbox](#sandbox). Either way,
> run Tenaz in a container, a VM or a user account that holds nothing you
> would miss, and do not point it at a brief or a repository you do not
> trust. While a run works, the engine proxy and the media MCP server listen
> on `127.0.0.1` without authentication, so any local process can reach
> them. See [`SECURITY.md`](SECURITY.md).

When the run is done (or stopped), turn it into something to review:

```bash
tenaz finish
```

`finish` creates a branch in your repository (named after the goal, or
`--branch`) with one commit on top of the commit the run started from: the
project's changes, without `.tenaz/` (and, for runs from older layouts,
without the root files and `.gitignore` lines Tenaz's first commit
added). One commit, because the run's own history interleaves bookkeeping
with steps that later steps rework; that history stays on `tenaz/<goal>`. `--remove-worktree` then
removes the worktree (its uncommitted `run.json` and journal go with it).
Nothing is pushed.

### Without questions

Every answer has a flag, and `init` with `--model` asks nothing, for
scripts and CI. With llama.cpp's server on its default port:

```bash
tenaz init --project my-goal --brief brief.md --model my-model --base-url http://127.0.0.1:8080/v1 --context-window 65536
```

`--harness pi` uses Pi instead of fx. Pi needs no setup of its own: like
fx, it runs with a configuration Tenaz writes for the run, so the engine
and model are the same answers or flags. When your `~/.pi/agent/models.json`
lists the model, the wizard offers it and the run keeps its settings there
(API key, thinking map); the file is only read. `--vision` says the model reads
images. `--set key=value` overrides a parameter; `tenaz params` lists them.
To give the agent an image tool, point it at ComfyUI and an API-format
workflow (`--comfyui http://127.0.0.1:8188 --workflow default=txt2img.json`);
with an engine other than Sushi, `--engine-stop` and `--engine-start` let
Tenaz free the memory while images render.

The protocol in `.tenaz/protocol.md` and the prompts in `.tenaz/prompts/`
are plain Markdown in your project; edit them to change how the agent works.
Every session's prompt starts with the protocol, so it does not depend on
the agent finding a context file. The project's own `AGENTS.md` or
`CLAUDE.md`, which agents such as Pi load by themselves, still apply to how
the code is written; where they disagree with the protocol about how the
work is done (commits, branches, scope), the protocol wins. Sessions may not
edit either file. Skill
directories passed with `--skill` at `init` are pinned read-only into
`.tenaz/skills/`, and are the only skills the agent sees; `--set
agentSkills=true` also lets it load the ones it finds by itself.

### Sandbox

With `--set sandbox=true` at `init`, the agent and `verify` run in an OS
sandbox: Seatbelt on macOS, Landlock on Linux (5.13 or later). Nothing to
install. Everything they start inherits it.

- They can write only to the project, its git directory (for a worktree,
  the repository's shared `.git` too), the temporary directories, `/dev`
  and the existing directories in `sandboxAllowWrite` (by default the
  package caches `~/.cargo/registry`, `~/.cargo/git`, `~/.npm`, `~/.cache`
  and `~/Library/Caches`). Writing elsewhere fails with `Operation not
  permitted`.
- They cannot change what Tenaz runs or trusts outside the sandbox: the
  git hooks and config, a worktree's `.git` file and `.tenaz/run.json`. On
  macOS the sandbox refuses those writes. Landlock cannot refuse a path
  inside a directory it lets a process write to, so on Linux Tenaz puts
  them back after every session and every `verify` run, before it runs git
  again, and says so in its log. It does that on macOS too.
- They cannot read `sandboxDenyRead` (by default `~/.ssh`, `~/.aws`,
  `~/.gnupg`). On Linux the names of the files there stay visible.
- The network is not limited yet.

Both lists are comma-separated, with `~` for your home directory:
`--set sandboxAllowWrite=~/.cargo/registry,~/.m2`. `run` checks the sandbox
by starting a program in it before the first session and stops with the
reason when it cannot, for example when Tenaz itself already runs in a
sandbox on macOS. The sandbox reduces what a mistaken model can damage; it
is not a boundary against a hostile brief or repository.

### In an existing project

This mode is experimental: it has been exercised with the scripted harness
only, not yet with a real model.

Pointed at a repository that already has commits, `init` sets
`existingProject=true` (override it with `--set existingProject=false`).
The run then treats the brief as a change to that project:

- `verify` is `.tenaz/verify`, a thin wrapper that runs the project's own
  tests and reports them under the contract, so nothing is added at the
  root and the wrapper is not part of the change.
- Checks for new behaviour go into the project's own test suite, written
  like its existing tests, so they ship with the change; its existing tests
  are the `T000.` checks and must keep passing.
- Bootstrap maps only what the change touches and plans as few tasks as the
  change needs, instead of a skeleton and 6 to 40 tasks.
- The run works on its own branch in a worktree (above), and
  `tenaz finish` puts the change on a branch of your repository as one
  commit, without `.tenaz/`.

## From a script or another agent

A frontier agent (Claude Code, Codex and the like) can hand a long goal to
Tenaz and check the result afterwards. Everything it needs is in the binary:

- `tenaz --help` gives the workflow and a non-interactive example.
- `tenaz run` exits with 0 when the goal is met, 3 when it completes with
  gaps, 4 when it is blocked and 5 when it pauses; 1 is an error.
- `tenaz status --json` prints the phase, the state (`running`, `paused`,
  `blocked`, `interrupted`, `complete`, `complete_with_gaps`), the block
  reason and the tasks as one JSON object.
- `tenaz guide` prints an [Agent Skill](crates/cli/skill/SKILL.md): when to
  delegate, how to write the brief, how to check what comes back. To install
  it for Claude Code:

```bash
mkdir -p ~/.claude/skills/tenaz && tenaz guide > ~/.claude/skills/tenaz/SKILL.md
```

A second `tenaz run` on a project that is already running is refused.

## How it compares

Model swappers such as llama-swap, Ollama's `keep_alive` or LM Studio's JIT
loading load models on demand, but know nothing of a goal or a task. Loop
patterns such as Ralph keep an agent going, but assume the engine is always
there and count an engine failure as a task failure. Tenaz combines a
deterministic goal loop with one lease over every model the run uses.

## Documentation

- [`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md): what is tested, known gaps.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): the crates and how they fit.
- [`CONTRIBUTING.md`](CONTRIBUTING.md): building, testing, conventions.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
