# Task-loop orchestrator: v2 design (draft)

Status: v0 implemented in `scripts/task-loop.mjs`. It completed its first
real run (Pocket Ledger, Qwen 3.8 27B on TensorFold, 2026-10-12) without
intervention; see
`benchmarks/reports/2026-10-12-qwen38-27b-tensorfold-native-107-pi-task-loop-pocket-ledger.md`.
It replaces the
challenge-specific milestone protocol in `CHALLENGE-RUNNER.md` (v1) with a
generic one.

## Usage

```sh
# Check the engine (reasoning + prefix cache) before any run
node scripts/task-loop.mjs preflight --model tensorfold/qwen3.8-27b-tensorfold

# New project from a brief; parameters are pinned in .agent/run.json
node scripts/task-loop.mjs init --project local-runs/<date>-<model>-<challenge>/project \
  --brief scripts/task-loop/briefs/pocket-ledger.md \
  --model tensorfold/qwen3.8-27b-tensorfold --thinking medium [--set replanEvery=5 ...]

# Run (resumable; --resume continues a BLOCKED run, --refresh-templates copies changed
# AGENTS.md and role prompts into a started run, --allow-weak-engine records a failed preflight)
node scripts/task-loop.mjs run --project <dir> > <run-dir>/run.log 2>&1
# Pause: Ctrl-C, or kill -INT <orchestrator pid>. Run the same command again to continue.
node scripts/task-loop.mjs status --project <dir>
scripts/task-loop/watch.sh <dir> [--thinking]   # follow Pi live (see "Monitoring a run")

# Orchestrator self-test with a scripted fake Pi (no model needed)
scripts/task-loop/test/run-fake.sh
```

Role prompts and the shared `AGENTS.md` are templates in
`scripts/task-loop/templates/`, and `init` copies them into each project.
Each session's prompt and raw RPC stream are saved under
`.agent/private/`, and the per-session metrics are in `.agent/journal.jsonl`.

### Monitoring a run

`run` writes its progress to stdout, so start it with the output redirected
to a log next to the project (for example `> <run-dir>/run.log 2>&1`). With
`P=<run-dir>/project`:

| Question | Command |
|---|---|
| What is Pi doing right now? | `scripts/task-loop/watch.sh $P` |
| …including its reasoning? | `scripts/task-loop/watch.sh $P --thinking` |
| Where is the run? | `node scripts/task-loop.mjs status --project $P` |
| Session starts, ends, steers, aborts | `tail -f <run-dir>/run.log` |
| What is the plan? | `$P/.agent/tasks/T007.md` (one card per task) |
| Why was a task accepted or rejected? | `$P/.agent/reviews/T007.md` |
| What did the builder hand over? | `$P/.agent/notes/T007.md` |
| What changed, attempt by attempt? | `git -C $P log --oneline` |
| Does the project pass now? | `(cd $P && ./verify --all)` |
| Turns, tokens, cache, watchdogs per session | `tail -3 $P/.agent/journal.jsonl \| jq .` |
| Engine TTFT and decode per request | `tail -5 $P/.agent/private/requests.jsonl \| jq -c .` |
| Memory pressure, swap, server footprint | `tail -5 $P/.agent/private/memory.jsonl \| jq -c .` |
| Full prompt and event stream of a session | `$P/.agent/private/<NNN-role-task>.prompt.md` and `.rpc.jsonl` |

`watch.sh` replays the current session from its start and then follows it.
It switches to each new session (builder, reviewer, planner) as the
orchestrator starts it, and stops with Ctrl-C. Each turn shows the tool calls
(`→`), failed tool calls (`✗`), text replies (`💬`), reasoning excerpts
(`💭`, with `--thinking`), compactions (`⚠`), and the prompt size with its
cached share (`· ctx 30720 (cached 26936), out 986`). A turn whose `cached`
value is far below `ctx` re-prefilled its context. Look for that when turns
are slow.

### Notifications

On macOS, `run` posts a desktop notification at these points, so whoever
shares the machine knows the engine and most of its memory are taken:

- when the run starts;
- at the first engine failure of a streak (usually memory taken by another
  process);
- when the run blocks, finishes, or stops with an error.

Turn notifications off with `--set notify=false`.

### Engine timings

After each model response the orchestrator reads the engine's Prometheus
`/metrics` endpoint, next to the OpenAI-compatible base URL (TensorFold,
vLLM and similar). The counters are cumulative and the engine serves one
request at a time, so the difference across one response is that request's own
figures. Each response is appended to `.agent/private/requests.jsonl` with:

- the session and sequence number;
- Pi's prompt, cached and output token counts;
- the engine's deltas (`ttft_s`, `decode_s`, `latency_s`, tokens, and drafted
  and accepted draft tokens);
- client-side timings measured from Pi events.

`engine_matched` is true only when exactly one engine request finished and its
prompt and output tokens equal Pi's. Only matched requests feed the
summaries: the session record's `engine` field in the journal, the `Engine:`
line of `status`, and the preflight's `engine` field. Those summaries give the
median TTFT, the median per-request decode rate, and the output-weighted decode
rate (total output tokens / total decode seconds).

Treat the client-side timings as orchestration latency, not engine
performance. When the engine has no `/metrics`, they are the only timings, and
reports must label them as client-observed.

### Memory

On macOS the orchestrator samples memory every `memorySampleSec` seconds
(default 30; 0 disables it), and also at the start and end of every session.
Each sample goes to `.agent/private/memory.jsonl` with:

- the kernel's pressure level (`1` normal, `2` warning, `4` critical);
- swap in use, and pages swapped in and out since the previous sample;
- compressed, wired and free memory;
- the footprint of the process listening on the engine's port, from `top`.
  Its `CMPRS` column is not recorded: for Sushi it reported about 46 GiB while
  the whole system compressor held under 9 GiB. On Apple silicon the model's
  GPU memory shows up as wired memory instead.

Each session's journal record gets a `memory` summary: worst pressure, GiB
swapped in and out, and peak server footprint. `status` prints a `Memory:`
line for the whole run. Swap-ins during a session are the signal that the
model or its cache no longer fits in RAM, and they slow every request. A high
compressed share alone does not show that: macOS also compresses idle pages.

`status` prints the phase (`BOOTSTRAP`, `LOOP`, `FINAL`, `COMPLETE`,
`COMPLETE_WITH_GAPS`, `BLOCKED`), the task counts, the task and session in
flight, the number of failed sessions and compactions, and the engine and memory summaries. It only reads
files, so it is safe to run while the orchestrator works.

The engine side is in the server log. For the TensorFold launchd jobs on this
workstation, that is
`~/qwen38-ds4-validation/validation-logs/tensorfold-zig-<context>/server.log`.
Each request has a `done … prompt=N cached=M … tokens=K` line, and
`server.stderr.log` holds the prompt-cache statistics.

### Engine setup on this workstation

The preflight needs reasoning and a working prefix cache. TensorFold
`serve` 1.0.7 disables the cache with `--prompt-cache-gib 0`. It also refuses
any cache that would take the process past 70% of RAM. On a 64 GiB Mac that
limit is reached by the model alone (about 48 GiB at 131072 tokens, about
44 GiB at 65536). Both launchd jobs therefore use `--prompt-cache-over-cap`
and run with `--thinking`. The 131k job keeps a 2 GiB cache and the 64k job a
6 GiB cache, so both use about 50 GiB in total.

The cache size matters more than it looks. Qwen 3.8 27B prompt states take
about 80 KB per token in TensorFold's cache (about 1.2 GiB at 15k tokens), and
a state larger than the cache is refused. In the Pocket Ledger run the 2 GiB
cache evicted 1153 states and refused 326. Only 74% of prompt tokens were
served from cache, and no prompt above about 25k tokens was cached at all.
Task-loop sessions peaked at 43k tokens there, so the 64k job with 6 GiB of
cache is the default for task-loop runs. Use the 131k job only when a session
needs more than about 40k tokens.

`~/qwen38-ds4-validation/tensorfold-switch.sh 131k|64k|stop|status` switches
between the jobs `com.alfredo.tensorfold-zig` and
`com.alfredo.tensorfold-zig-64k`, which share port 18080. It waits for the
old process to release its memory and for the new model to be served. Pass
the matching Pi model to `preflight` and `init`:

- 131k: `tensorfold/qwen3.8-27b-tensorfold`
- 64k: `tensorfold-64k/qwen3.8-27b-tensorfold-64k`

The provider prefix selects the Pi entry that holds the context window. The
orchestrator refuses a model that its provider does not list.

## Goal

Give one local model a complex brief and have it plan, split the work into
small tasks, implement them one at a time, check and review its own work, and
finish without human intervention. It must work for any challenge (game, API,
CLI, refactor) and for real tasks outside the benchmark.

Constraints:

- One inference process at a time. Every role is the same model, run
  sequentially.
- The context window is finite and compaction is lossy. A task should fit in
  one session without compaction.
- Quality is held by the handover between fresh sessions. Long sessions are
  not used to carry it.

## Lessons from v1 (Wild West Showdown 3D, 2026-10-10)

Evidence: `local-runs/2026/10/2026-10-11-qwen38-27b-tensorfold-wild-west-showdown-3d/project/.benchmark/`.

- Both Builder sessions failed on M03 without editing any code. Session 1 ran
  the same headless-Chrome screenshot command 47 times in a row (about 3 h).
  Session 2 ran the same `--dump-dom | grep SMOKE` command 25 times. The
  runner has no loop detection, only an 8 h timeout.
- The model's self-improvised verification was ambiguous. Running `grep` on
  the DOM matched both `SMOKE-FAIL` and `SMOKE-OK` literals in the harness
  source. Screenshots were blank or rendered at random.
- Sessions emitted 0 thinking and 0 text characters, only tool calls of about
  250 tokens each. `cacheRead` was 0, so every turn re-prefilled the whole
  context (35k–70k tokens, 2–5 min per turn). Session 1 ended with four
  `terminated` provider errors at about 5-minute intervals.
- Each session used about 35k tokens reading files before it acted.
- Two failures exhausted `maxRecovery = 2`, and the run stalled.

Design consequences: checks must have a fixed contract that the orchestrator
runs itself. Stalls must be detected and handled by a recovery ladder.
Session context must be packed by the orchestrator. The engine must provide
reasoning and a prefix cache before any orchestration result can be trusted.

## Division of labour

| Orchestrator (deterministic code) | Model (judgement) |
|---|---|
| Choose the next ready task | Write the spec and the task backlog |
| Build each session's briefing | Build the project's `verify` command |
| Run `verify`; commit or revert | Implement one task |
| Detect loops and no-progress, steer, abort | Review a diff without the builder's context |
| Write the factual journal and metrics | Split or replan tasks; write short notes |
| Enforce file ownership and budgets | |

The orchestrator never contains challenge-specific logic.

## Project layout (generic)

All orchestration state lives under `.agent/` in the project repository:

```text
BRIEF.md                  immutable original brief (hash pinned)
.agent/
  spec.md                 numbered requirements R1..Rn derived from BRIEF.md
  map.md                  repository map: file -> purpose / key exports
  tasks/T001.md ...       task cards (one file each)
  notes/T001.md ...       the model's handover notes per task (about 15 lines)
  journal.jsonl           orchestrator-only, append-only facts per session
  run.json                orchestrator ledger (status, budgets, config pins)
  private/                raw session logs, screenshots (git-ignored)
verify                    project-owned check entry point (any language)
```

### `verify` contract

The model builds it in the bootstrap phase. The orchestrator enforces only
the interface:

- `./verify [--task T007] [--all]` runs from the project root without
  interaction and finishes within a configured time limit (default 120 s).
- Exit code 0 means every selected check passed. Any other code means failure.
- The last line of stdout is one JSON object:
  `{"ok":bool,"passed":n,"failed":[{"id":"...","msg":"..."}],"artifacts":[...]}`.
  The output is cut to 4 KB before it is shown to a model.
- Checks are deterministic. A check that cannot be automated is listed in
  `spec.md` as `manual:` with a reason. It does not count as passed.
- Each check carries the task ID and requirement IDs it covers, so coverage
  can be computed from `spec.md`.

The orchestrator runs `verify --all` twice at bootstrap acceptance and
rejects the run if the results differ. A flaky check is a bootstrap defect.

### Task card

```markdown
---
id: T007
status: todo            # todo | active | done | blocked | split
requires: [R4, R9]
depends: [T003]
files: [src/duel.js, src/hud.js]
budget: {turns: 40, tokens: 60000}
attempts: 0
---
## Goal
One or two sentences.
## Acceptance
- check `duel.false-start` added to verify and passing
- verify --all still passes
## Context
Pointers only: functions, decisions, earlier tasks. No pasted code.
```

The orchestrator owns `status` and `attempts`. The model owns everything
else, and only in planner sessions.

## Phases

### 1. Bootstrap (planner role, may take several sessions)

The bootstrap writes `spec.md`, `map.md`, a working `verify` with at least a
smoke check, and an initial backlog. Tasks are ordered to reach a minimal
end-to-end slice early. The bootstrap is accepted when the orchestrator
confirms all of the following:

- The files parse.
- Every requirement is covered by at least one task or marked `manual:`.
- The dependencies form a directed acyclic graph.
- `verify --all` passes and gives the same result twice.

A rejected bootstrap gets the problems in its next session's prompt. When the
orchestrator reverted files the session wrote where a bootstrap may not, the
problems start with which files and where code belongs: under `.agent/` a
bootstrap writes only `spec.md`, `map.md`, `tasks/` and `notes/`. In the
first promo run on Sushi, three bootstraps in a row put `verify`'s helper in
`.agent/checks/`; each saw only `verify` failing and repeated it.

### 2. Task loop

For each ready task (dependencies done, lowest ID first):

1. **Build session.** A fresh session receives the briefing (see below). The
   model implements the task, adds its acceptance checks to `verify`, updates
   `map.md` for any files it changed, and writes `notes/T007.md`.
2. **Verify.** The orchestrator runs `verify --all`. If it fails, the next
   build attempt receives the failure JSON. A regression in an earlier
   task's check counts as a failure.
3. **Review session.** A fresh session receives only the task card,
   `git diff` of the task, and the `verify` output. It answers with
   `ACCEPT` or a list of findings. Each finding becomes a new task card
   (`T007a`, depending on nothing) or forces a retry. The reviewer cannot
   edit code.
4. **Commit** on accept, with message `task T007: <title>`, and set
   `status: done`. Otherwise run `git reset --hard` to the pre-task commit
   once `attempts` exceeds its limit, then escalate.

### 3. Replan (every N tasks, or when tasks block)

A planner session reads `spec.md`, the backlog, the done list, and the
journal summary. It may add, split, reorder, or rewrite `todo` tasks. It may
not reopen `done` tasks. Any regression must become a new task.

### 4. Final acceptance

The orchestrator runs `verify --all` and computes requirement coverage. A
final review session then checks `spec.md` against reality and lists the
gaps as new tasks. The run is complete when the backlog is empty, `verify`
passes, every requirement is covered or marked `manual:` with a reason, and
the final reviewer returns `ACCEPT`.

## Session briefing (context packing)

The orchestrator writes the first user message. Stable content comes first so
that the engine's prefix cache can reuse it across sessions:

1. Role prompt (fixed per role).
2. `spec.md` (stable after bootstrap).
3. `map.md`.
4. Task card.
5. Notes from the dependency tasks and the previous attempt.
6. Latest `verify` JSON, cut to 4 KB.

The role prompt tells the model to read only the files listed on the card or
named in the map, unless a check fails elsewhere. Target: the briefing is
≤ 15% of the context window, and a task finishes in ≤ 65% of it
(`contextBudget`; the session is steered to wrap up past it).

## Watchdogs and recovery ladder

Signals are taken from the Pi RPC stream:

- **Loop:** the same `(tool, args)` 3 times in a row (steer) or 5 times
  (abort). Or the same call with an identical result 4 times (steer) or 6
  times (abort) among the last 12 calls. The second rule catches alternating
  loops such as `write notes` / `wc -l notes`. These look like progress to
  the no-progress check because a file keeps changing.
- **No progress:** N consecutive turns (default 12) with no write or edit
  inside the project.
- **Budget:** the task's turn or token budget is exceeded, or a compaction
  begins.
- **Context stop:** past `contextAbort` (default 1.3) times its context
  budget, a session is stopped. The steer alone was not enough: in the
  microwave anatomy run, T015 builders kept going for 15 minutes after the
  steer and grew from the 55k budget to 95–110k tokens, past what the engine
  could hold. A stopped session keeps its changes. A fresh session continues
  the same work, with a note telling it to start from `git status`, the diff
  and the task notes. This repeats up to `contextContinuations` (default 3)
  times without counting as an attempt (journal record
  `context-continuation`). After that, the stopped session is handled like
  any other stopped session.
- **Refused image:** a vision engine can refuse an image the session has
  read: too many images or image tokens in one request, or a format it does
  not decode. The image stays in the conversation, so every later request
  fails the same way, and under greedy decoding a retried session reads the
  same image again. In the promo video run, TensorFold refused a WebP banner
  until the run blocked. Now the session is stopped and continued in a fresh
  one, as with the context stop and sharing its `contextContinuations` limit,
  without counting as an attempt (journal record `image-continuation`). The
  new session's note quotes the refusal and asks it to read only PNG or JPEG,
  convert other formats first, and combine stills into one contact sheet.
- **Large output:** a single tool result over `bigOutputTokens` (default
  8000, counted as text length / 4) gets a steer to grep and read ranges, up to
  three times per session. In the anatomy run, whole-file reads of the skill's
  50–90 KB references took sessions past 90k tokens in under 25 turns.
- **Provider:** consecutive provider errors or a per-turn latency ceiling.

The ladder, applied per task:

1. Steer the live session with a message that names the signal (for example,
   "you have run X 3 times with the same result").
2. Abort it and start a fresh build attempt, with the signal recorded in the
   briefing.
3. Run a planner session that splits the task into 2–4 smaller cards.
   Mark the original `split`.
4. Mark the task `blocked` and continue with other ready tasks.

The run stops only when no task is ready and every remaining task is
blocked. Blocking is the orchestrator's decision, never the model's.

A stopped builder session whose work still passes `verify` goes on to review
rather than being retried, because the work is often finished and the session
was circling afterwards. The reviewer is told the session was stopped, and the
task's journal record lists it under `stopped_sessions`, so such a task is not
counted as a clean first-attempt pass.

A reviewer that ends without writing a verdict gets one fresh retry. If the
retry also has no verdict, the task is accepted on `verify` alone and its
journal record has `reviewed: false`. In the Pocket Ledger run, one reviewer
fell into a repetition loop inside its reasoning (14k output tokens repeating
one line) under greedy decoding (`--temperature 0`). It ended without a
verdict.

### Engine failures

A session that ends with a provider error says nothing about the task, so it
does not use up an attempt. The orchestrator reruns the same session:

- **The prompt is larger than the engine can hold.** Sushi rejects such a
  prompt and names its current maximum ("Maximum context at KV8 … N tokens"),
  which falls when other processes take memory. If N is at least
  `contextFloor` (default 32768), the run's context budget is lowered to
  `capacityHeadroom` × N (default 0.8), and the session reruns at once. The
  journal gets a `context-cap` record, `status` shows the lowered budget, and
  it applies to every later session. After `capacityRetries` (default 4) such
  reruns in a row, the run blocks.
- **Anything else, or N below the floor.** Something outside the run holds the
  memory, or the server is down. The session reruns after a backoff of
  `engineRetryBaseSec` (default 60 s), doubling up to 15 minutes. These
  attempts are recorded as `engine-retry` journal records. After
  `engineRetries` (default 6) in a row, the run blocks with "engine
  unavailable", and `--resume` continues it.

A builder attempt cut short by an engine block or a pause is given back: the
task card's `attempts` is not charged for it.

**Pausing.** Ctrl-C or SIGTERM stops the current Pi session and pauses the
run, so the machine can be used for something else. The run stays in its
phase, the session's file changes stay in the working tree, and the journal
records `paused`. Running the same command again journals `resumed`, and the
next session gets the continuation note. Run wall time is the sum of the
segments, so the pause itself is not counted.

Before the capacity rule existed, the anatomy run blocked after six backoffs
(58 minutes of waiting). Its T006 sessions grew to 80–110k tokens, while
Sushi could hold 60–97k at that moment.

## Role prompts (generic)

There are four short role prompts in `.agent/prompts/`: `planner`, `builder`,
`reviewer`, and `final-reviewer`. They describe the contracts above and refer
to no specific challenge. The project's `AGENTS.md` holds only the rules
every role shares: `BRIEF.md` is immutable, `.agent/journal.jsonl` and
`run.json` belong to the orchestrator, and a session ends after its single
unit of work.

## Metrics (per session, to `journal.jsonl`)

Each record holds: task, role, attempt, turns, input/output/cache tokens,
reasoning tokens, compactions, watchdog events, verify result, diff stat,
wall time, outcome. These records answer the benchmark question: which task
size, briefing, and review settings let a given local model finish real work?

## Tunable parameters (for experiments)

The following are pinned in `run.json` per run:

- task budget,
- review on or off,
- briefing on or off (when off, the model explores on its own),
- replan cadence,
- watchdog thresholds,
- maximum attempts per task.

## Engine prerequisites (check before the first session)

The orchestrator runs a preflight probe and refuses to start if any check
fails:

- The model returns reasoning content at the configured thinking level.
- A repeated identical prefix reports cached tokens on the second request.
- A request of about 60k tokens completes within the provider timeout.

## Alternatives considered

- **Pi extension or a `localagent` CLI** with the same loop inside the
  agent. This is good for interactive real work, but harder to instrument
  and reproduce. It can come later and reuse the same `.agent/` contracts.
- **Stateless single-call steps** (Aider- or SWE-agent-style), which
  rebuild the context every call. This variant removes compaction entirely
  but also free exploration. It is kept as a comparison variant through the
  "briefing on, tools minimal" settings.
- **Fixed milestone gates (v1).** These are replaced by per-task
  verification and review, which keep diffs small enough for a local model to
  review.

## Rollout

1. Implement `scripts/task-loop.mjs` with the preflight, ledger, briefing,
   verify runner, watchdogs, and journal. Reuse the Pi RPC handling from
   `challenge-runner.mjs`.
2. Run it end to end on a small challenge (target: about 10 tasks).
   Adjust the thresholds.
3. Rerun Wild West Showdown 3D from a fresh copy at commit `2523584`,
   with `CHALLENGE.md` as `BRIEF.md`.

## Open questions

- Should the reviewer use the same thinking level, or a higher one?
- Should `verify` checks be locked after their task is done, so that later
  builders cannot weaken them? The orchestrator could hash check files per
  task and require planner approval for edits.
- How should a brief with no automatable surface (pure visual design)
  be handled? Probably a screenshot artifact plus `manual:` coverage.
