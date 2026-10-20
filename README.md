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
  llama.cpp's `llama-server`, LM Studio, Ollama, vLLM, Sushi or similar.
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

Or build from source with a stable Rust toolchain:

```bash
cargo install --locked --git https://github.com/OWNER/tenaz tenaz
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

- `init` creates the project, writes the goal to `BRIEF.md`, adds the
  agent rules (`AGENTS.md`) and role prompts (`.tenaz/prompts/`), pins the
  model, the agent, how to start the engine and the run parameters in
  `.tenaz/run.json`, and makes the first commit.
- `run` reads all of that from `run.json`, probes the engine, then works
  until the goal is met or a task blocks. A launch flag given to `run`
  (`--comfyui`, `--engine-start`, ...) replaces the pinned value and is
  saved. `--resume` continues a blocked run after you have fixed what
  blocked it.
- `status` reads files only and is safe while a run works.

> **Tenaz commits to the project's repository.** `init` uses an existing
> `.git` if there is one, and its first commit stages everything
> (`git add -A`), including changes you had not committed. During `run`,
> every step is a commit on the current branch, and a failed attempt is
> undone with `git reset --hard` and `git clean -fd`, which deletes
> untracked files that are not ignored. To point Tenaz at an existing
> project, start from a clean tree on its own branch or worktree:
>
> ```bash
> git worktree add ../my-project-tenaz -b tenaz/my-goal
> ```

### Without questions

Every answer has a flag, and `init` with `--model` asks nothing, for
scripts and CI. With llama.cpp's server on its default port:

```bash
tenaz init --project my-goal --brief brief.md --model my-model --base-url http://127.0.0.1:8080/v1 --context-window 65536
```

`--harness pi` uses Pi instead of fx and reads the model's URL and context
window from `~/.pi/agent/models.json`. `--vision` says the model reads
images. `--set key=value` overrides a parameter; `tenaz params` lists them.
To give the agent an image tool, point it at ComfyUI and an API-format
workflow (`--comfyui http://127.0.0.1:8188 --workflow default=txt2img.json`);
with an engine other than Sushi, `--engine-stop` and `--engine-start` let
Tenaz free the memory while images render.

The prompts in `.tenaz/prompts/` and the rules in `AGENTS.md` are plain
Markdown in your project; edit them to change how the agent works. Skill
directories passed with `--skill` at `init` are pinned read-only into
`.tenaz/skills/`.

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
