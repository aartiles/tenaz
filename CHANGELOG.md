# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `tenaz init --model-for ROLE=ID` gives a role another model than
  `--model`, for example a reviewer of another model family, served by the
  same engine. It is repeatable, one role each, and
  `--context-window-for ROLE=N` sets that model's window (default: the
  run's). The map is kept in `run.json` under `config.roleModels`;
  `tenaz status` names the roles it covers and gives the builder's context
  budget, and `status --json` has `role_models`. An unknown or repeated
  role, the run's own model, and a window for a role with no model of its
  own are refused, and so are one model given different windows for two
  roles and a map with `--harness pi`.

  fx is launched with each session's model, and the session's context
  budget is the `contextBudget` share of that model's window; the
  session's journal record has `model`. Planners size cards for the
  builder's model. After the engine runs out of memory, the lowered budget
  is raised toward the largest model's budget. See ADR 0011.
- With `--model-for`, every model of the run must be loaded before the
  first session. Tenaz loads those that are not, as it does the run's
  model, and `tenaz run` stops and names any it could not load. Around an
  image job or a heavy job the lease frees every model and loads them all
  back; a generic engine's stop and start commands run once for all of
  them. A generic engine that serves only some of the run's models is not
  started a second time: `tenaz run` stops and names the others.
- A builder whose task needs work too heavy for a session (a render, a
  long build) writes `.tenaz/jobs/<TASK>.json` with the command, its time
  limit and why, and stops. Tenaz runs the command between sessions of the
  same attempt, with the engine's request gate closed and the LLM unloaded
  where the engine can, for at most `jobMinutes` (60); a fresh session
  then gets its exit code, duration and the end of its log, in
  `.tenaz/private/jobs/`. The job does not cost an attempt. An attempt
  may have `jobsPerAttempt` (3) job files; one past them is removed unread
  and the attempt is judged. The journal gets `job-started` and
  `job-finished` records. A file that is not valid runs nothing and the
  next session is told why. A stop during the job pauses the run, and on
  resume the job runs again before any session; a report no session has
  read yet is kept in `run.json` across a pause. A crash costs the
  attempt: the job file it left is removed unrun. After a job the LLM
  serves again before the next session; one that does not come back
  blocks the run as "engine unavailable". `jobMinutes=0` turns jobs off:
  a job file is removed unread and the attempt is judged. In the Lotion
  Pump run a 25-minute Blender render next to the model's engine failed
  for memory, and so did the engine; the operator had to run it with the
  run blocked. See ADR 0010.
- A builder session rerun after the engine ran out of memory is told why
  and asked not to leave heavy work running in the background, but to
  declare it as a job. Other roles, other engine errors and runs with
  `jobMinutes=0` get no such note. See ADR 0010.
- A builder whose task cannot pass without changing a check of a done task
  writes `.tenaz/conflicts/<TASK>.md` naming those checks and stops. Tenaz
  blocks the task at once, without more attempts or a split, journals a
  `conflict` record, and starts a replan that is shown the conflict. The
  replan may add a card with `amends: [<check IDs>]`, naming only checks a
  conflict names; its builder may change those checks and no others, and
  its reviewer judges each change against the spec. A conflict with the
  task's own checks, or with a task that is not done, does not stand: the
  attempt fails as before and the next builder is told why. A builder that
  declared a conflict is not asked for a passing check, and a conflict
  belongs to the attempt that declares it: the file is removed when an
  attempt starts. In the Lotion
  Pump run the builders of T016b measured that their card and a check of
  T016a could not both pass, and no session was allowed to change the
  check. The builder prompt no longer promises a replan for any other
  task that cannot be done. See ADR 0009.
- Without the sandbox, a write tool that names a file outside the project,
  the temporary directories, on macOS the per-user cache, and
  `sandboxAllowWrite` is noted: a line in
  `orchestrator.log`, `outside_writes` and an `outside-write` watchdog
  event in the session's journal record, and one steer telling the session
  its files belong in the project. A bootstrap step that is not accepted
  says where the files went. The write is not stopped, and shell commands
  are not checked. See ADR 0008.

### Changed

- These apply to every run, with or without `--model-for`:
  - A session whose context budget is already under the cap a memory
    failure set (a card that asks for a small budget, for example) no
    longer counts toward raising the cap: it did not test it.
  - After an image job or a heavy job, a generic engine's start command
    is skipped when the engine still serves the model.
  - When the run's model is not served before the first session,
    `tenaz run` says `Model <id> is not being served at <url>`, with why
    loading it failed, instead of `the LLM could not be loaded: ...`
    (`Model <id> is not being served (<error>)` when the engine does not
    answer, as before).
  - When freeing the LLM for an image or a heavy job fails and the engine
    still answers without it, Tenaz loads it back at once (a generic
    engine's start command runs) instead of leaving it to the engine
    retry.
- On macOS the sandbox lets the agent and `verify` write the per-user cache
  (`getconf DARWIN_USER_CACHE_DIR`), as they could already write the
  temporary directory beside it. Metal keeps its compiled shaders there:
  without it every program that draws with Metal compiles them again on
  each start, and in a Blender benchmark each `./blender` call took about
  10 s longer (a small render took 11 s instead of 0.9 s).
- In the failure history a split is shown, an attempt whose `verify`
  failure repeats the one before word for word says so and names the
  attempt that has the text, instead of repeating it. In the Lotion Pump
  run the split of T016b read the same 1.5 KB of `verify` output three
  times. `run.json` keeps that failure as `current.verify_failure`
  (`attempt`, `text`).
- A builder whose previous attempt failed `verify` word for word as an
  earlier one is told so. If `verify` reports a measurement, nothing done
  since moved it: change the approach; or, when the card cannot pass
  without changing a check of a done task, declare the conflict (see
  "Added"); or say in the notes why the card cannot be done, and stop. In
  the Lotion Pump run the three attempts of T016b each left the scene at
  its starting numbers. The task is not escalated any sooner: a check
  that only says `exit 1` reads the same while real work goes on.

### Fixed

- A list of changed files put in a prompt is clipped to `statClipBytes`
  (8000 bytes, a new parameter), at a whole line, with the number of lines
  left out. Such lists went in whole: the reviewer's (ahead of a diff
  clipped to `diffLimitBytes`) and its list of changed images, the files
  an earlier bootstrap session left, and a retried builder's changes so
  far. In the Digger 3D run of 2026-10-29 a task had committed 3,265
  `node_modules` files: their list alone was about 200 KB, and fx refused
  the review before sending it, as over the reviewer model's 65,536-token
  window. Every rerun sent the same prompt.
- With fx, an engine that runs out of memory part way through a reply is
  reported as such. Sushi sends the error inside a stream that started
  with status 200, which the proxy ignored, so the session read as a
  harness exit: `failed (toolUse)` in the log and `cause: harness` in the
  journal. The proxy now reads an `error` sent in a `data:` chunk of the
  stream and reports it as it arrives; the session's log line says
  `failed (engine out of memory)` and the rerun's log line says another
  program may be using the GPU. In the Lotion Pump run seven sessions
  ended this way while Blender rendered on the same GPU. Pi reports
  engine errors itself, without the proxy; an out-of-memory error it
  reports is named the same way.
- Any engine error stated before the harness exits now journals the
  rerun as `cause: engine` rather than `harness`: the exit follows the
  error. The journal's `cause` keeps its two values; out of memory is
  `engine`.
- A harness that dies in a later prompt of its session, such as the one
  that asks a builder for its task's checks or the note that resumes an
  interrupted turn, is rerun without charging an attempt, as one that
  dies in the first prompt is. The session counted as settled from its
  first answer, so the crash failed the attempt: in the Lotion Pump run
  fx lost its engine connection while T016b's builder was adding its
  check, and the task lost attempt 2.
- The preflight no longer fails an engine that does not report cached
  prompt tokens. LM Studio 0.4 reuses the prompt prefix but leaves
  `cached_tokens` out of its usage, which looked like no cache and needed
  `--allow-weak-engine`. Such an engine now passes with a warning, and
  `tenaz status` says its prompt cache is "not reported" instead of 0 %;
  `run.json` has `preflight.cache_reported` and `tenaz status --json` has
  `prompt_cache_reported` and `prompt_cache_percent`. An engine that
  reports 0 cached tokens still fails. Needs the engine proxy. See ADR 0007.
- A task out of attempts keeps its notes. Its work was reverted with the
  notes in it, so the split never saw what the builders had measured and
  ruled out, and wrote cards that retried it: in the Lotion Pump run the
  split of T016b blamed a cause its builder had already disproved. The
  notes now survive the revert, the split's prompt shows them under
  "Builder notes from the failed attempts", and a blocked task keeps them.

## [0.2.0] - 2026-10-28

### Added

- `capacityRetryWaitSec` (10 by default): the pause before a session is
  rerun after the engine ran out of memory and Tenaz lowered the context
  budget. It was a fixed 10 seconds. A stop (Ctrl-C) during the pause now
  pauses the run at once; before, the pause ran out and the next session
  started first. `tenaz demo` sets it to 0, so the demo ends about 10
  seconds sooner.

- A time limit per tool call, `toolMinutes` (10 by default, 0 turns it
  off). A call that runs over it, such as a script in an endless loop, is
  stopped and the same session is told which call and what to do; a second
  one ends the session. Pi's `bash` has no default timeout, and a hung call
  held a session for 21 minutes. See ADR 0004.

- Release archives come with a GitHub build provenance attestation, which
  shows that an archive was built from this repository by the release
  workflow. Check one with
  `gh attestation verify <archive> --repo aartiles/tenaz`.

### Changed

- The sessions that write task cards (the bootstrap's task-cards step,
  split and replan) are told the builders' context budget in tokens, with
  a rule: what a builder reads and writes for a card fits in about two
  thirds of it, at roughly 4 characters per token, and a card over that is
  split before it is written. The prompts only said "context-limited
  sessions": in the Digger 3D run of 2026-10-27 a card that needed eight
  modules read was split twice, and builders ended past the budget.

### Fixed

- A run paused during the review of an attempt that passed `verify` (or
  stopped there because the engine gave out) runs `verify` on that
  attempt again when it resumes and, if it still passes, reviews it
  again. It counted the attempt and built the task again, and a pause in
  the review of the last allowed attempt split the task. The Digger 3D run
  of 2026-10-27 was paused this way during the review of T012ba's attempt
  2.

- A session stopped at its second context compaction is continued by a
  fresh session, like one stopped at its context budget, and no longer
  counts as a failed attempt. Both mean the context ran out; in the Digger
  3D run of 2026-10-27 the compaction stop charged T012ba an attempt.
  An attempt that uses up its continuations this way is split, as one
  stopped at its budget is (`contextSplit`).

- A session stopped at its context budget keeps the work of the reply that
  crossed it. The stop came at the reply's end, before its tool calls ran,
  so a model that wrote a whole file in that reply lost it: three files in
  the Digger 3D run of 2026-10-27, and each continuation wrote it again
  into the same stop. The reply's tool calls now run first, and the session
  stops before its next reply.

- The engine proxy no longer garbles a character that the network splits
  across two chunks of a streamed reply. It decoded each chunk on its own,
  so an accented letter or emoji cut in half reached the harness as `�`
  in every repaired stream. It now decodes only whole events.

- A request the harness cancels because Tenaz stopped the turn no longer
  counts as an engine error. Pi reports it as one ("This operation was
  aborted"), so a session aborted at its second compaction was rerun as an
  engine failure: the attempt did not count and an engine retry was spent.

- `verify` runs in a process group of its own, and Tenaz kills the group
  when it ends or times out. A check that started `npm run dev` and failed
  left Vite running on its fixed port, so the next check failed on the port
  (with `--strictPort`), and a leftover process that kept verify's output
  open made Tenaz wait for it.

- `verify` runs with `NO_COLOR=1` as well as `CI=1`. `CI` turns colours
  on in some tools (Vite), so a check that read their output failed under
  Tenaz and passed in the builder's shell. Escape codes are also left out of
  failure messages and output tails. The protocol tells builders the
  environment `verify` runs in.

- `tenaz watch` shows Pi sessions. Pi's stream keeps Pi's own records,
  which `watch` dropped (or showed raw, marked `?`, at `debug`); it now
  translates them as the Pi adapter does.

- The setup wizard names the provider as `tenaz init` with flags does. For
  an engine address that does not answer yet, it pinned `local`; the flags
  pinned the provider usual on that port (`llamacpp` on 8080, `ollama` on
  11434, ...). Both now take that one, and `local` only for another port.
  The provider is part of `model` in `run.json` (`provider/id`), and the
  wizard's advice on the context window now names that engine (Ollama's
  context setting for an address on 11434).

## [0.1.1] - 2026-10-27

### Changed

- `tenaz --version` and `runner_version` in `run.json` name the commit a
  binary was built from a git checkout of (`tenaz 0.1.0 (816e1d8)`). Three
  benchmark runs of different code all recorded `tenaz 0.1.0`. A build
  from a source archive, without git, shows the version alone.
- The final reviewer first judges the product as a whole against what the
  brief says it is for and what it says to avoid, under `## Overall` in
  `FINAL.md`, and asks for changes when it misses that even if every
  requirement passes. In the explainer run each motion requirement passed
  and the review accepted a video that read like the slides the brief
  ruled out.
- A builder or bootstrap session stopped at its turn budget is continued
  by a fresh session, like one stopped at its context budget, instead of
  failing the attempt: its changes are kept and the new session is told to
  finish from them. In the explainer run three builders ran out of turns
  while working, and each cost its task an attempt.
- The package description (shown by `brew info` and on the release) says
  what Tenaz does now: it drives a local model through a coding agent
  until the goal passes its checks.

## [0.1.0] - 2026-10-27

### Added

- The README opens with a GIF of a scripted `tenaz demo` run, recorded
  with VHS from `docs/demo/demo.tape`.
- `tenaz` binary with `init`, `run`, `status`, `finish` and `params`.
- `--help` describes the workflow with a non-interactive example; `run --help`
  lists the exit codes and `status --help` what each phase means.
- `tenaz run` exits with 0 when the goal is met, 3 when it completes with
  gaps, 4 when it is blocked and 5 when it pauses; 1 is an error and 2 a
  usage error.
- `tenaz status --json` prints the status as one JSON object (`phase`,
  `state`, `pid`, `blocked_reason`, `last_error`, task counts, current task,
  session in progress, `origin`); the text status gains a `State:` line.
- `tenaz run` holds a lock on the project while it works: a second run on
  the same project is refused, and `status` tells a working run (`running`)
  from one that was killed or failed (`interrupted`).
- `tenaz guide` prints an Agent Skill for delegating goals to Tenaz from
  another agent.
- The agent skill moved to `skills/tenaz/SKILL.md`, so `npx skills add
  aartiles/tenaz` installs it. It now also covers installing and setting
  up Tenaz: `tenaz doctor` and its fixes, what to ask the user to do
  rather than doing it, and `tenaz demo`.
- `tenaz demo` shows a whole run in about a minute with no model and no
  agent: the scripted harness builds a small goal in a new `tenaz-demo`
  folder. It replaces the three commands the README gave for that, and
  `tenaz doctor` offers it while something is still missing.
- `tenaz doctor` checks git, a coding agent, an engine and a model with
  enough context, and says how to install or fix what is missing (the
  install commands for Pi and fx, LM Studio or Ollama, the context setting
  of each engine). It exits with 0 when a real run can start and 1
  otherwise. The setup wizard gives the same advice when it finds no git,
  no agent, no engine or too small a context.
- Setup wizard: `tenaz init` without `--model`, in a terminal, finds the
  coding agents on `PATH`, the engines on their usual ports with their
  models, context windows and vision, and ComfyUI, then prints the
  equivalent command line. `--interactive` forces it with a piped stdin.
- `run.json` pins how to launch the run (`launch`: engine API and
  start/stop commands, proxy, harness executable, ComfyUI and workflows,
  media timeout), so `tenaz run` needs no flags; a flag given to `run`
  replaces the pinned value and is saved. `run` and `status` default to
  the current directory.
- On a repository that already has commits, `init` works on a new branch
  `tenaz/<goal>` in a worktree next to the repository instead of on the
  current branch, refuses a tree with uncommitted changes, and records the
  starting branch and commit in `run.json` (`origin`). `--in-place` keeps
  the old behaviour; `--worktree` and `--name` choose the directory and
  branch. The setup wizard asks which, `run` and `status` say where the
  run works, and both point to the worktree from the original checkout.
  A directory inside a repository that tracks nothing in it, such as an
  ignored directory with seed files, is a new project rather than refused.
- `tenaz finish`: a branch in the original repository with the run's
  result as one commit on the starting commit, without `.tenaz/` (nor,
  for older layouts, the root files and `.gitignore` lines Tenaz added);
  the run branch keeps the full history, `--remove-worktree` removes the
  worktree. Nothing is pushed.
- The run's state lives in `.tenaz/` (it was `.agent/`). A project still
  in the old layout is not run: `init`, `run` and `status` say how to move
  it (`mv .agent .tenaz`, the `.gitignore` lines, `--refresh-templates`).
- Tenaz keeps all its files under `.tenaz/`: the brief is
  `.tenaz/brief.md` (it was `BRIEF.md`), the shared rules are
  `.tenaz/protocol.md` (they were the project's `AGENTS.md`) and the ignore
  rules are in `.tenaz/.gitignore` (they were appended to the project's
  `.gitignore`). The protocol now starts every session's prompt instead of
  relying on the agent to load `AGENTS.md`, so `init` works in a project
  that has its own `AGENTS.md`, and sessions may not edit the project's
  `AGENTS.md` or `CLAUDE.md`. A run started with `BRIEF.md` at the root is
  not run: `run` and `status` say how to move it (`git mv`,
  `--refresh-templates`).
- `existingProject` parameter. `init` sets it when the repository already
  has commits: `verify` is then `.tenaz/verify`, a wrapper around the
  project's own tests, new checks go into the project's test suite, and
  bootstrap plans only the change instead of a skeleton.
- Without `--engine`, `init` asks the engine at `--base-url` which API it
  speaks (Sushi or generic) and names the provider after it.
- Orchestrator: bootstrap, build, verify, review,
  split or block, replan and final review, with pause and resume.
- Lease manager keeping one engine resident at a time.
- OpenAI-compatible engine proxy that holds requests while engines swap.
- Harness adapters for fx over ACP and Pi over its RPC mode, plus a scripted
  fake harness.
- Engine adapters for Sushi and generic OpenAI-compatible engines; ComfyUI
  media adapter; MCP server exposing `generate_image`.
- The README and `docs/COMPATIBILITY.md` name MLX's `mlx_lm.server` and
  TensorFold as engines to use with `--engine generic`, untested.
- CI on macOS and Linux and a tagged release workflow.
- Releases are built with dist and come with an install script
  (`curl ... | sh`) that puts `tenaz` in `~/.local/bin` and on `PATH`, and
  a Homebrew formula in `aartiles/homebrew-tap`.
- `requests.jsonl` rows carry the engine's own `timings` per request
  (`server`: prefill and decode milliseconds, cached tokens, and draft
  counts when the engine reports them), and `status` prints the draft
  acceptance.
- `tenaz watch` follows a run from its files as it works, at four levels:
  `summary` (the run log), `actions` (each tool call and reply on one line,
  with context, output, thinking and decode speed), `full` (whole replies,
  the start of tool results) and `debug` (the harness's log too). `--once`
  prints the current session and exits.
- A context budget lowered because the engine ran out of memory is raised
  halfway back after `capacityRecoverSessions` sessions in a row (4 by
  default; 0 keeps it lowered) that the engine held without critical memory
  pressure. A raise the engine overflows again lowers it as before and
  doubles the sessions the next raise waits for, up to four times as many;
  a raise the engine held starts that count again.
- `--images <base URL> --image-model <id>` gives the agent its image tool
  through an OpenAI Images API (`POST /v1/images/generations`), as MLX
  servers, LocalAI and Atomic Chat serve it, with no workflow to export.
  When that API is on the LLM's own server, the LLM stays loaded while the
  image renders (ADR 0002).
- `tenaz init`'s wizard offers an image tool when an engine on the usual
  ports, Atomic Chat on 1337 or mlx-serve on 11234, lists an image model
  (the LLM's own engine first), before it looks for ComfyUI. `tenaz
  doctor` lists the image engines it finds with the flags that use them.
  `TENAZ_NO_PROBE` keeps both from looking at the usual ports, for tests
  and scripted setups.
- A builder session that answers its prompt while `./verify --task` runs no
  passing check of its task gets one more prompt asking it to add them,
  before the attempt is judged. In the Wild West run 32 of 197 attempts
  passed `--all` but failed only for lack of the task's own check.
- A raised context budget never goes above the headroom share of what the
  engine said it could hold when an earlier raise overflowed it. Raising
  past it only overflowed again: in the Wild West run the engine held
  86016 tokens, yet the budget was raised above 68812 six times and lowered
  back each time, and a low reading under passing memory pressure then
  kept the budget at 38215 for seven hours while raises waited 64 sessions.
- A session is steered to wrap up when, at the pace its context grew over
  the last requests, it would reach the context stop within
  `contextSteerTurns` turns (10 by default; 0 steers only at the budget), so
  a slow-to-react model can still write its handoff before it is stopped.
- An attempt that uses up its context continuations and still fails
  `verify` is split at once instead of retried against the same budget
  (`contextSplit`, on by default), when the split depth allows.
- A builder's briefing shows only the spec requirements its card requires,
  with a pointer to the whole spec (`citedSpec`, on by default; a card that
  names none the spec has gets the whole spec). A session that continues
  stopped work no longer gets the repository map in its prompt.
- A session steered for going idle is asked to write what it already knows
  (the files and facts the change depends on, and its plan) in its task's
  notes before making the change, and the next attempt's briefing includes
  those notes.
- Reading something not read before counts as progress for the idle rule,
  like a file change; repeated reads of the same thing still count as idle.
- With fx, each session's reasoning text is kept in
  `.tenaz/private/<label>.thinking.md`, with a line after each model
  request. Before, only its length was kept.
- A bootstrap session after a failed one is told which files the earlier
  sessions left (`git diff --stat` since `init`) and asked to continue from
  them instead of starting over. Before, it got only the list of problems,
  and explored and wrote the spec again.
- The bootstrap runs in three steps, one session each: the spec and map,
  then a minimal `verify`, then the task cards (ADR 0003). Each
  session gets only its step's part of `bootstrap.md`, steps 2 and 3 get
  the spec and map in their prompt, and `maxBootstrapAttempts` applies to
  each step. A step whose files are already right is passed without a
  session. `run.json` gains `bootstrap_step`, `BootstrapValidation`
  records gain `step`, and `tenaz status` shows the step. A local model
  drafted the whole bootstrap in its reasoning and hit the output limit
  before writing any file, even at low effort.

- The engine API Tenaz loads and unloads models with is called `load-api`
  (`--engine load-api`, `"engine": "load-api"` in `run.json`) instead of
  `sushi`, since mlx-serve has the same API; `sushi` is still read. mlx-serve
  is detected on its port 11234 and named as itself, not as Sushi.

### Fixed

- In an existing project, a change to `.tenaz/verify` (the project's
  `verify`) is shown in the reviewer's diff and in the builder's list of
  changes kept from earlier attempts. Both left out all of `.tenaz/`, so a
  reviewer could not see an edit to the checks' runner.
- The media MCP server keeps a session open for a day without requests
  instead of 5 minutes (rmcp's default). fx opens its session when an agent
  session starts; a first image call more than 5 minutes later failed with
  `McpSessionExpired` and cost a call of the image budget, in both
  image-landing runs.
- `generate_image` without a width or height fills a ComfyUI workflow's
  size placeholders with 1024 instead of leaving `{{width}}` as text,
  which ComfyUI rejected.
- Ctrl-C during the engine preflight pauses the run instead of ending it
  with a failed preflight.
- A harness that ends before its turn settles is retried like an engine
  failure instead of being charged as a task attempt, and its exit code and
  stderr tail are journaled (`harness_exit`).
- A harness that ends while one of the session's tool calls runs counts as
  a failed attempt instead of an engine failure, and the next briefing
  names the call: a rerun made the same call and the harness died again
  until the run blocked. When the output was not valid UTF-8 (fx dies on
  it), the briefing also says not to cut output by bytes.
- A session whose reply stopped at the output limit while the model was
  still reasoning (fx exits on it with `OutputTruncated`) counts as a
  failed attempt instead of an engine failure, the next briefing says why,
  and the next session asks the engine for one step less reasoning effort
  (through the proxy). Rerun unchanged at temperature 0, such sessions
  reasoned the same way again until the run blocked.
- A reply that reaches the output limit counts as cut even when the engine
  ends it with `tool_calls` (TensorFold does, for a reply cut inside a
  tool call's arguments). Tenaz retried such sessions as engine failures,
  up to six times with waits, at the same effort.
- A steer for context, turns or compaction asks each role for what it may
  write before stopping: a reviewer its verdict file, the final reviewer
  `FINAL.md`, a planner its cards. All were told to write handoff notes,
  and reviewers did, in notes files that were then reverted.
- A review asks for changes only with at least one MAJOR finding: the
  reviewer prompt says so, and a `CHANGES` whose findings are all MINOR is
  accepted. One such review cost a whole builder and review cycle for a
  missing line in a decisions file.
- A reviewer's briefing lists the images the change adds or replaces, and
  the reviewer prompt asks to look at them first, in one contact sheet when
  there are more than two, and to check they match the project's other
  images. The final reviewer looks at all the product's images first. In
  the image-landing run on mlx-serve the reviewers opened page screenshots
  only, and the final reviewer marked the visual requirements manual
  without seeing the images, so a product that changed from image to image
  went unnoticed.
- Each line of `.tenaz/private/requests.jsonl` records, under `sent`, the
  sampling and limits the request asked the engine for, as the proxy
  forwarded it: `temperature`, `top_p`, `top_k`, `reasoning_effort` (the
  lowered one after a cut) and `max_tokens`. A field the request did not
  carry is left out: the engine used its own default. Only for requests
  through the proxy.
- `--set temperature=<t>` pins the run's sampling temperature: the proxy
  puts it in requests that carry none (fx sends none). Unset, as before,
  each engine uses its own default, which made the TensorFold and mlx-serve
  runs of the same goal sample at 0 and 0.8.
- An attempt fails when `./verify --all` passes fewer checks of other tasks
  than when the last task was done (`passed_checks` in `run.json`): a check
  was removed or `verify` no longer runs it. Before, only the reviewer could
  notice. A check weakened without being removed still passes this rule.
- fx writes one ACP log per session instead of overwriting a shared one.
- An ACP session handles a proxy observation before the prompt's end, so
  per-request usage is no longer occasionally lost or attributed to the
  wrong turn.
- The engine proxy adds the missing `action` to a tool call that sends a
  `command` without it, when every variant of the argument that takes a
  `command` fixes the same `action` (models drop it from fx's `shell` call,
  which fx then rejects).
- The engine proxy moves properties a model sends beside fx's
  `shell.request` (such as `timeout_ms` or `cwd`) into it, when the variant
  its `action` selects takes them all and the request does not already hold
  them; fx rejected those calls.
- The engine proxy clamps an integer argument outside the range its schema
  declares (such as fx's `yield_time_ms` above 30000) to that range; after
  fx's rejection models retried with the limit anyway.
- The protocol tells every session that commands run at the project root
  and to use relative paths: models guessed the project's absolute path for
  `cwd` or `cd`, and the guess was often wrong.
- With a generic engine and no `--engine-stop` and `--engine-start`, an
  image now renders with the LLM still loaded, as the wizard says; before,
  the image request failed. A stop command that fails no longer leaves the
  agent's requests waiting until the next session.

### Security

- `SECURITY.md` and the README say what Tenaz does not isolate (the agent's
  commands, `verify`, the unauthenticated local proxy and MCP server) and
  how to report a vulnerability privately.
