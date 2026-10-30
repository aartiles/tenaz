# Tenaz

Tenaz drives a local model through a coding agent until the goal passes its
checks. It does not trust the model: it writes a `verify` script first, every
small task gets a fresh session, and only work that passes `verify` and a
review is committed. A failed task is reverted and retried or split.

![A scripted tenaz demo run, then tenaz status](docs/demo/demo.gif)

A scripted run (`tenaz demo`, sped up 2x, output trimmed): no model and no
agent, but the real runner, `verify` and git commits.
[`docs/demo/demo.tape`](docs/demo/demo.tape) and `docs/demo/render.sh`
regenerate it.

Tenaz is built for machines that fit **one model at a time**: a laptop or a
single GPU running a local LLM. Long agent runs on that kind of hardware fail
in predictable ways: the context fills up, the engine runs out of memory or
restarts, the model loops on the same tool call, and an image or speech model
cannot be loaded next to the LLM. Tenaz handles those failures instead of
counting them against the work.

You can start a run yourself, or let a frontier agent such as Claude Code
or Codex hand it a long goal through Tenaz's
[companion skill](#delegate-from-another-agent) and check the result when
it is done.

### Real runs

Unattended runs on one Apple M5 Pro, each with its run sheet (what was
built, the checks, the engine numbers and what went wrong):

- [Wild West Showdown 3D](https://aartiles.github.io/local-model-benchmarks-site/runs/wild-west-showdown-3d-sushi-v124-tenaz/):
  a playable 3D game, Qwen 3.8 Flash Next on Sushi 1.2.4. Complete after
  104 hours, 575 sessions, 86 tasks.
- [Tenaz explainer](https://aartiles.github.io/local-model-benchmarks-site/runs/tenaz-explainer-sushi-v130/): a 45-second video
  with ComfyUI images, Qwen 3.8 Flash Next on Sushi 1.3.0. Complete after 12 hours, 54
  sessions, 15 tasks; one image slot missing.
- [Image landing, TensorFold](https://aartiles.github.io/local-model-benchmarks-site/runs/image-landing-tensorfold-native/): a
  product page with FLUX images, Qwen 3.8 27B. Complete in 3 h 49 min.
- [Image landing, mlx-serve](https://aartiles.github.io/local-model-benchmarks-site/runs/image-landing-mlx-serve/): the same goal
  with Qwen-Image 2.1 on the LLM's own server. Complete with one gap in
  3 h 22 min.

## How it works

```text
 goal ─▶ bootstrap ─▶ ┌─ build task ─▶ verify ─▶ review ─▶ commit ─┐ ─▶ final review ─▶ done
                      └──────────── retry, split, block or replan ◀──┘
```

You write the goal as a short brief. The **bootstrap** turns it into
numbered requirements, a `verify` script and a list of small tasks, in three
sessions, one step each, so no session has to plan everything at once. Each
task then gets its own session: the agent builds it, Tenaz runs `verify`,
and a fresh session of the same model, or of the one `--model-for` gives
the reviewer, reviews the diff. A review asks for changes only for a
problem that matters for the requirements. A task that
passes is committed; one that fails is undone and retried, split into
smaller tasks or marked blocked. Its notes are kept, so a split knows what
the failed attempts tried. A builder that finds its task cannot pass
without changing a check of a done task says so in
`.tenaz/conflicts/<TASK>.md`; the task is blocked at once, and a replan
may add a card that amends those checks, judged against the spec (ADR
0009). Work too heavy for a session, such as a long render, is declared
by the builder in `.tenaz/jobs/<TASK>.json`; Tenaz runs it between
sessions, with the engine's memory freed where it can, for at most
`jobMinutes` (60), and the next session of the attempt reads how it went
(ADR 0010). When every task is done, a final review checks the whole goal
against the brief.

**`verify` decides when the work is done.** It is an executable at the
project root, in any language, that runs every check and ends its output
with a one-line JSON result. The bootstrap writes it as one small file with
a check that passes from the start, and each task adds the checks for what
it builds, as files in `checks/`. The model saying it is done counts for nothing; only `verify`
passing does. In an existing project it is a thin wrapper around the
project's own tests.

- **Fresh sessions, state on disk.** Every unit of work is a new agent
  session with a short context. The spec, task cards, notes and progress live
  in the project under `.tenaz/`, and every step is a git commit.
- **The runner decides, not the model.** Tenaz runs `verify` itself, commits
  or reverts, and decides when to retry, split a task, block or replan. An
  attempt that leaves fewer passing checks of other tasks than when the last
  task was done fails, so a check cannot be dropped to make a task pass.
- **Engine failures are not task failures.** A crash, a refused connection or
  a smaller context than expected is retried with backoff without charging
  the task. A reply cut at the output limit is the session's: it counts as
  an attempt, and the next session reasons at a lower effort.
- **Watchdogs.** Loops, repeated calls, idle turns and huge outputs get a
  steering message first, then the session is stopped. A file written
  outside the project is logged and the session told so.
- **Resident models.** When the agent asks for an image, Tenaz unloads
  the LLM, runs the image model, frees it and reloads the LLM. With
  `--model-for`, every model named must be loaded at once: Tenaz checks
  before the first session (loading what it can, as for `--model`), does
  not swap between them, and frees and reloads them all around an image
  or a heavy job. The agent's requests wait meanwhile instead of failing.
  See [Images](#images) for what that needs from your engine.
- **Pause and resume.** Ctrl-C pauses after the current session; run again
  to continue.

### Terms

- **Engine**: the server that runs the model and answers OpenAI-compatible
  requests (llama.cpp, LM Studio, Ollama, ...). Tenaz needs only that API
  to run; freeing memory for images needs more (see [Images](#images)).
- **Harness**: the coding agent Tenaz drives in each session, fx or Pi.
  Tenaz talks to fx over ACP (the Agent Client Protocol) and to Pi over its
  RPC mode.
- **Brief**: the goal in your words, in `.tenaz/brief.md`. It does not
  change after `init`.
- **Task card**: one small task in `.tenaz/tasks/`, with the requirements it
  covers and the `verify` checks that prove it. Check IDs start with the
  task's ID (`T007.import-quoted-csv`); the bootstrap's start with `T000.`.
- **Blocked**: the run stopped on something it cannot get past by itself,
  such as a task that failed its retries and splits. `tenaz status` says
  why; fix the cause and run again with `--resume`.

## What you need

- macOS on Apple silicon or Linux x86_64, and `git`.
- **A coding agent** Tenaz can drive: [fx](https://github.com/vercel-labs/fx)
  or [Pi](https://github.com/earendil-works/pi).
- **An OpenAI-compatible engine** serving a model with tool calling:
  llama.cpp's `llama-server`, LM Studio, Ollama, vLLM, MLX's
  `mlx_lm.server`, TensorFold, Sushi or similar. A context window of 32k
  tokens is the practical minimum. Reasoning and prefix caching are checked
  before the first session.
- Optional: [ComfyUI](https://github.com/comfyanonymous/ComfyUI) for images.

How much memory depends on the model you choose, not on Tenaz.
[`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) lists what has been
tested and what has not.

Once Tenaz is installed, `tenaz doctor` checks all of this (git, the agent,
the engine and the model's context) and says how to install or fix what is
missing. It also lists the image engines it finds, which are optional.

## Install

With the install script, which puts `tenaz` in `~/.local/bin` and adds
that directory to your `PATH`. It needs no Rust:

```bash
curl -LsSf https://github.com/aartiles/tenaz/releases/latest/download/tenaz-installer.sh | sh
```

Open a new terminal afterwards, or run `source ~/.local/bin/env`. To leave
your shell profile alone, run the script with `TENAZ_NO_MODIFY_PATH=1`.

With Homebrew, on macOS:

```bash
brew install aartiles/tap/tenaz
```

The [releases page](https://github.com/aartiles/tenaz/releases) also has
the archive for each platform with its SHA-256 checksum. To check that
an archive was built from this repository by its release workflow
(optional; needs the GitHub CLI, logged in with `gh auth login`):

```bash
gh attestation verify tenaz-aarch64-apple-darwin.tar.gz --repo aartiles/tenaz
```

Or build from source with Rust 1.91 or later:

```bash
cargo install --locked --git https://github.com/aartiles/tenaz tenaz
```

Then try `tenaz demo` (below), and run `tenaz doctor` to see what else a
real run needs.

## Try it without a model

Right after installing, this shows a whole run in seconds. It needs
no agent and no engine:

```bash
tenaz demo
```

A scripted harness plays a fixed session script, with engine failures,
watchdog stops and a review rejection, against a real git repository in a
new `tenaz-demo` folder (`--project` picks another). The runner, the
`verify` script and the commits are the real ones. Then look at
`tenaz-demo/.tenaz/` and `git -C tenaz-demo log --oneline`.

`tenaz demo` is `tenaz init` with `--harness fake --model fake --provider
fake --set engineRetryBaseSec=0 --set capacityRetryWaitSec=0 --set
notify=false`, then `tenaz run`.

## Before a real run

> **By default Tenaz does not sandbox the agent.** The agent runs the shell
> commands and writes the files the model asks for, as your user, and Tenaz
> approves every permission request it makes; the project's `verify` script
> runs the same way. A model that misreads the goal can touch anything your
> user can. `init --set sandbox=true` limits that: see [Sandbox](#sandbox).
> Either way, run Tenaz in a container, a VM or a user account that holds
> nothing you would miss, and do not point it at a brief or a repository you
> do not trust. While a run works, the engine proxy and the media MCP server
> listen on `127.0.0.1` without authentication, so any local process can
> reach them. See [`SECURITY.md`](SECURITY.md).

> **Tenaz commits to the project's repository.** Every step of a run is a
> commit, and a failed attempt is undone with `git reset --hard` and
> `git clean -fd`, which deletes untracked files that are not ignored. So
> in a repository that already has commits, the run never touches your
> checkout: it works on a branch `tenaz/<goal>` in a worktree next to it.
> See [Where the run works](#where-the-run-works).

## Run it on a real goal

Write the goal in a brief of one page or less. Say what a person can run and
see when it is done, what must not change, how the project is built and
tested today, and any constraint the code does not show. Leave out the plan:
Tenaz makes its own. Goals that can be checked by running the program suit
it best; a run takes hours.

1. Start your engine with a model loaded, then create the project:

   ```bash
   tenaz init
   ```

   It asks what to build, finds the coding agents on your `PATH`, the
   engines on their usual ports with the models they serve and their
   context windows, and ComfyUI when it runs. It shows what it found,
   prints the same setup as a `tenaz init` command line for next time, and
   creates the project.

2. From the project directory, start the run:

   ```bash
   tenaz run
   ```

   It probes the engine, then works until the goal is met or a task blocks.

3. From another terminal, check on it:

   ```bash
   tenaz status
   ```

   `tenaz watch` follows it session by session instead. Each session's
   prompt, events (Pi's own records with Pi) and, with fx, the model's
   reasoning are kept in `.tenaz/private/` (`<label>.prompt.md`,
   `.rpc.jsonl`, `.thinking.md`).

4. When the run is done (or stopped), turn it into something to review:

   ```bash
   tenaz finish
   ```

   This creates a branch in your repository, named after the goal, with the
   project's changes as one commit and without `.tenaz/`. Nothing is pushed.

## Delegate from another agent

A frontier agent (Claude Code, Codex and the like) can hand a long goal to
Tenaz, keep working on other things, and check the result when the run is
done. The local model does the hours of work; the frontier agent writes the
brief and reviews what comes back.

Tenaz ships a companion [Agent Skill](skills/tenaz/SKILL.md) for that.
It tells the agent how to install and set up Tenaz with `tenaz doctor`,
when to delegate and when not to, how to write the brief, how to start and
follow the run, and how to check the result before reporting it. Install
it with [skills](https://skills.sh), which asks which agents to add it to:

```bash
npx skills add aartiles/tenaz
```

Or take the copy that matches your `tenaz` binary; for Claude Code:

```bash
mkdir -p ~/.claude/skills/tenaz && tenaz guide > ~/.claude/skills/tenaz/SKILL.md
```

Then ask the agent to hand the goal to Tenaz. Scripts can use the same
interface the skill relies on:

- `tenaz --help` gives the workflow and a non-interactive example.
- `tenaz run` exits with 0 when the goal is met, 3 when it completes with
  gaps, 4 when it is blocked and 5 when it pauses; 1 is an error.
- `tenaz status --json` prints the phase, the state (`running`, `paused`,
  `blocked`, `interrupted`, `complete`, `complete_with_gaps`), the block
  reason, the tasks and the prompt cache share (`null` when the engine does
  not report it) as one JSON object.

## Reference

### The commands

- `init` creates the project (or uses the directory you point it at),
  writes the goal to `.tenaz/brief.md`, adds the working protocol
  (`.tenaz/protocol.md`) and role prompts (`.tenaz/prompts/`), pins the
  model (and any `--model-for` role models), the agent, how to start the
  engine and the run parameters in `.tenaz/run.json`, and makes the first commit. Everything it writes is
  under `.tenaz/`, with its own `.tenaz/.gitignore`; the project's
  `AGENTS.md`, `CLAUDE.md` and `.gitignore` are left as they are. The
  wizard looks for engines on Sushi's port 12345, mlx-serve's 11234,
  llama.cpp's 8080, LM Studio's 1234, Ollama's 11434 and vLLM's 8000. It
  offers an image tool when one of those engines, or a server on 1337
  (Atomic Chat), lists an image model, the LLM's own engine first, and
  otherwise when ComfyUI answers on 8188.
- `run` reads all of that from `run.json`. A launch flag given to `run`
  (`--comfyui`, `--engine-start`, ...) replaces the pinned value and is
  saved. `--resume` continues a blocked run after you have fixed what
  blocked it. A second `run` on a project that is already running is
  refused.
- `status` reads files only and is safe while a run works.
- `watch` follows the run as it works, session by session: with
  `--level summary` the run log only, with `actions` (the default) also each
  tool call and model reply on one line with the context it used and the
  decode speed, with `full` whole replies and the start of tool results, and
  with `debug` the harness's own log too. It reads files only; Ctrl-C stops
  watching, not the run.
- `finish` creates a branch in your repository (named after the goal, or
  `--branch`) with one commit on top of the commit the run started from:
  the project's changes, without `.tenaz/`. One commit, because the run's
  own history interleaves bookkeeping with steps that later steps rework;
  that history stays on `tenaz/<goal>`. `--remove-worktree` then removes
  the worktree (its uncommitted `run.json` and journal go with it).
- `params` lists the run parameters that `--set key=value` overrides.

### Where the run works

In a repository that already has commits, `init` creates a branch
`tenaz/<goal>` from your HEAD and checks it out in a worktree next to the
repository (`../<repo>-tenaz-<goal>`; `--worktree` and `--name` change
them). It records the starting branch and commit in `.tenaz/run.json`, and
the run works there. `run` and `status` say where, and point to the
worktree when started from your checkout. `init` refuses a tree with
uncommitted changes rather than leave them behind; an untracked
`.tenaz/brief.md` is taken as the brief.

`--in-place` makes the run commit on your current branch instead, and its
first commit takes everything uncommitted (`git add -A`). A new or empty
directory becomes a new repository.

### Without questions

Every answer has a flag, and `init` with `--model` asks nothing, for
scripts and CI. With llama.cpp's server on its default port:

```bash
tenaz init --project my-goal --brief brief.md --model my-model --base-url http://127.0.0.1:8080/v1 --context-window 65536
```

`--model-for ROLE=ID` gives one role another model, for example
`--model-for reviewer=Qwen3.8-27B` to have the diffs reviewed by another
model family; the other roles keep `--model`. Repeat it for more roles
(bootstrap, builder, reviewer, split, replan, final-reviewer), and add
`--context-window-for ROLE=N` when that model's window is not
`--context-window`; roles that name the same model get the same window.
The engine serves every model named, all loaded at once: Tenaz does not
swap between them. Not with `--harness pi` yet. See
[ADR 0011](docs/adr/0011-a-model-per-role.md).

`--vision` says the model reads images. `--set key=value` overrides a
parameter; `tenaz params` lists them. Whatever its model, the diff a
reviewer is shown takes at most `diffShare` (0.25) of the reviewer's
context budget, under the engine's capacity cap when there is one,
counted at 3 bytes per token, and never more than `diffLimitBytes`
(40000 bytes). Before each session Tenaz estimates its
first request, the prompt at 3 bytes per token plus
`harnessReserveTokens` (8192) for the harness, and blocks the run when it
is over the budget of the role's model; see
[ADR 0012](docs/adr/0012-prompts-that-fit-their-model.md). Tenaz
does not set the sampling temperature, so each engine uses its own
default; `--set temperature=0` pins it for the run (through the proxy),
which makes runs on different engines comparable.

### Pi instead of fx

`--harness pi` uses Pi instead of fx. Pi needs no setup of its own: like
fx, it runs with a configuration Tenaz writes for the run, so the engine
and model are the same answers or flags. When your `~/.pi/agent/models.json`
lists the model, the wizard offers it and the run keeps its settings there
(API key, thinking map); the file is only read.

### Images

The agent gets an image tool from one of two kinds of image engine:

- **A server with the OpenAI Images API** (`POST /v1/images/generations`),
  as several MLX servers, LocalAI and Atomic Chat serve it:
  `--images http://127.0.0.1:1337/v1 --image-model <model>`. No workflow
  is needed.
- **ComfyUI** with an API-format workflow:
  `--comfyui http://127.0.0.1:8188 --workflow default=txt2img.json`.

When the image API is on the same server as the LLM, that server holds
both models and Tenaz leaves the LLM loaded. Otherwise Tenaz does not
assume the LLM and the image model fit in memory together.
Before each image it frees the LLM, and after it loads the LLM again. The
OpenAI API has no standard way to unload a model, so how Tenaz does that
depends on the engine:

- **An engine with a load and unload API** (`/v1/load-model` and
  `/v1/unload-model`, as Sushi and mlx-serve have). Tenaz uses it
  (`--engine load-api`; `sushi`, its earlier name, still works); `init`
  detects it from the engine's answers.
- **Any other engine** (`--engine generic`). Give Tenaz two
  commands at `init`, or let the wizard ask for them:
  - `--engine-stop` frees the memory, for example by stopping the engine's
    process. Tenaz runs it, then waits until the model is no longer listed
    at `/v1/models` (or the engine stops answering).
  - `--engine-start` brings the engine back with the same model (with
    `--model-for`, every model of the run). It must return once the engine
    is launched, so start a server process in the background. Tenaz runs
    it once when the engine does not answer or has none of the models
    loaded, then waits up to 20 minutes for them to be listed again. An
    engine that has only some of them loaded is running, so it is not
    started a second time: `tenaz run` names the missing ones.

  Both are whole command lines, run as your user. Without both, the LLM
  stays loaded and serving while the image renders, so the memory has to
  hold the two models together.

### Protocol, prompts and skills

The protocol in `.tenaz/protocol.md` and the prompts in `.tenaz/prompts/`
are plain Markdown in your project; edit them to change how the agent works.
Every session's prompt starts with the protocol, so it does not depend on
the agent finding a context file. The project's own `AGENTS.md` or
`CLAUDE.md`, which agents such as Pi load by themselves, still apply to how
the code is written; where they disagree with the protocol about how the
work is done (commits, branches, scope), the protocol wins. Sessions may not
edit either file. `bootstrap.md` has a `## Step N` section for each
bootstrap step; a session gets the common part and its own step.

A run keeps the protocol and prompts it was started with. After updating
Tenaz, `tenaz run --resume --refresh-templates` gives a started run the new
ones.

Skill directories passed with `--skill` at `init` are pinned read-only into
`.tenaz/skills/`, and are the only skills the agent is offered; `--set
agentSkills=true` also lets it load the ones it finds by itself. fx 0.0.14
can still find and load the skills in the folders above the project; see
[Known gaps](docs/COMPATIBILITY.md#known-gaps).

### Sandbox

With `--set sandbox=true` at `init` (or `"sandbox": true` in the params of
`.tenaz/run.json`), the agent and `verify` run in an OS sandbox: Seatbelt on
macOS, Landlock on Linux (5.13 or later). Nothing to install. Everything
they start inherits it.

- They can write only to the project, its git directory (for a worktree,
  the repository's shared `.git` too), the temporary directories, on macOS
  the per-user cache (`getconf DARWIN_USER_CACHE_DIR`, where Metal keeps
  its shaders), `/dev` and the existing directories in `sandboxAllowWrite`
  (by default the package caches `~/.cargo/registry`, `~/.cargo/git`,
  `~/.npm`, `~/.cache` and `~/Library/Caches`). Writing elsewhere fails
  with `Operation not permitted`.
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

Without the sandbox (the default), Tenaz still looks at the file each
write tool names. When it is outside the places above, the write goes
ahead, and Tenaz names the file in `orchestrator.log` and in the session's
journal record (`outside_writes`), and tells the session once that its
files belong in the project. A bootstrap step whose files are then missing
says where they went. Writes from shell commands are not seen (ADR 0008).

### In an existing project

This mode is experimental: it has been exercised with the scripted harness
only, not yet with a real model.

Pointed at a repository that already has commits, `init` sets
`existingProject=true` (override it with `--set existingProject=false`).
The run then treats the brief as a change to that project:

- `verify` is `.tenaz/verify`, a thin wrapper that runs the project's own
  tests and reports them in the `verify` format, so nothing is added at the
  root and the wrapper is not part of the change.
- Checks for new behaviour go into the project's own test suite, written
  like its existing tests, so they ship with the change; its existing tests
  are the bootstrap's `T000.` checks and must keep passing.
- Bootstrap maps only what the change touches and plans as few tasks as the
  change needs, instead of a skeleton and 6 to 40 tasks.
- The run works on its own branch in a worktree (see
  [Where the run works](#where-the-run-works)), and `tenaz finish` puts the
  change on a branch of your repository as one commit, without `.tenaz/`.

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
