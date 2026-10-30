# ADR 0012: Prompts that fit their role's model

Status: accepted (1A, 2A, 3A, 4A), 2026-10-30.

## Context

The first real run with a model per role (ADR 0011) was Digger 3D, on
2026-10-29: fx 0.0.15 and mlx-serve 26.10.3 on a 64 GB Mac, Ternary
Bonsai 2 27B as the builder with a 131072 window, and Qwen3.8-27B for the
other roles with 65536. At `contextBudget` 0.65 the budgets are 85196 and
42598 tokens.

Twice the run sent a prompt that could not be taken, unchanged, seven
times, and then blocked as "engine unavailable". Each time took about 45
minutes (backoff 60 s to 900 s, `engineRetries` 6).

1. **fx refused the prompt.** T001, sessions 007 to 013: a reviewer prompt
   of 281465 characters, about 200 KB of it a file stat (clipped since,
   `statClipBytes`). fx estimated 75528 tokens against 49152 usable
   (three quarters of 65536) and exited with `ContextCapacityExceeded`.
   The text is in the session's `harness_exit`; nothing reads it, so the
   journal says `cause: harness`. The refusal is deterministic: the same
   prompt is refused again.
2. **mlx-serve refused the prompt for memory.** T003, sessions 025 to 031
   and 033: "This prompt (30245 tokens) does not fit in GPU memory even
   after freeing the prefix cache … It was refused before any work
   started." fx reports it as a `ProviderError`. `OUT_OF_MEMORY` does not
   match it, so the journal says `cause: engine` and the log says "engine
   error". It depends on free memory: one reload of Qwen let 032 through,
   and 016 and 020 had the same refusal mid-session. The run had ten of
   them, all on Qwen reviewers.

Behind both: nothing compares a prompt with its role's window before the
session opens, and the review diff is cut at one `diffLimitBytes` (40000)
for every role. The reviewer's first request was 30245 of its 42598
tokens before it read a file; the diff was 40.6k of the prompt's 79.7k
characters.

What Tenaz does today, for reference:

- `Recovery::next` (`orchestrator/recovery.rs`) lowers the run's
  `context_cap` only for Sushi's "Maximum context … N tokens". An engine
  out of memory is a `Retry` with `FailureCause::OutOfMemory`: the cap is
  not touched, and a lower cap would not shrink the prompt anyway.
- A budget is a share of the role model's window, under `context_cap`
  (`ledger/budget.rs`).
- `util::approx_tokens` counts 4 characters per token.

### The harness reserve, measured

For every session of the run, the first request's `input + cacheRead`
(from `private/NNN-*.rpc.jsonl`) against the session's `prompt_chars`:

| Sessions | Model | Prompt | First request − chars/4 |
|---|---|---|---|
| 001–004 bootstrap | Qwen3.8 | 8.6k–35.0k chars, prose | 6743–6786 tokens |
| 005, 006, 015–024 builder | Bonsai | 13.1k–24.1k chars | 6936–7453 tokens |
| 016–033 reviewer | Qwen3.8 | 63.3k–86.6k chars, 24.7k–48.1k of diff | 9143–11990 tokens |

- fx adds about 6750 tokens to every session: its system prompt and the
  tool schemas. It is the same at 8.6k and at 35k characters of prose.
- Prose is close to 4 characters per token. A diff is not: between the
  reviewer sessions, each 1000 more characters of diff cost about 330
  more tokens, 3 characters per token. That is the rest of the reviewers'
  gap.
- So 4 characters per token plus a fixed reserve underestimates a review
  prompt by about a quarter of its diff: session 025 is 19918 + 6750 =
  26668 by that count, and 30245 in fact.

## Decisions

### 1. fx's `ContextCapacityExceeded`

- **A. Block the run at once** (recommended). The reason names the
  session, its role, its model, the model's window and the prompt's size,
  and what to change. Rerunning the same prompt cannot help.
- B. Rebuild the prompt smaller and rerun once, then block. Only the
  reviewer's diff can be made smaller after the fact, and decision 4 makes
  it fit before the session instead.
- C. Lower the budget, as for Sushi's capacity limit. It does not help:
  the prompt stays the same.

A, because after decision 4 this refusal means the estimate was wrong,
and a person should look. It costs one session instead of seven.

### 2. mlx-serve's "does not fit in GPU memory"

- **A. Treat it as the engine running out of memory** (recommended): add
  it to `OUT_OF_MEMORY`. The retries stay, the log and the session's
  `provider-error` event name the memory, and a builder rerun gets the
  memory note when jobs are on. The journal's `cause` stays `engine`.
- B. As A, with fewer or shorter retries for it.
- C. Block at once.

A, because the same prompt passed after a reload, and the message itself
says it depends on memory. It does not save the 45 minutes: the waits
did not help in the run, the reload did. What saves them is a smaller
prompt (decisions 3 and 4) and an operator who knows what to do, so the
rerun's log line, the notice at the first rerun and the "engine
unavailable" reason after the last retry say to free memory or reload
the model, for any engine out of memory. They do not say to free the prefix
cache: mlx-serve has already done that before it refuses.

The out-of-memory path does not lower `context_cap`, and it stays so: a
lower budget steers a session sooner, it does not shrink the prompt that
was refused.

### 3. The size of the review diff

- **A. A share of the reviewer's budget** (recommended): a new parameter,
  `diffShare`, default 0.25, counted at 3 bytes per token, with
  `diffLimitBytes` kept as the ceiling. For the Qwen reviewer above,
  0.25 × 42598 × 3 = 31948 bytes; for a reviewer on the 131072 window the
  ceiling, 40000, holds.
- B. A fixed number of tokens. It ignores the window, which is what went
  wrong.
- C. Leave it alone and let decision 4 catch it. Every large review would
  then block instead of being clipped.

A. A share keeps room for the files the reviewer reads, on any window,
and old `run.json` files load with the default.

### 4. A check before a session opens

- **A. Clip, then block** (recommended). Each prompt that has a part
  sized by the budget sizes it before the session: today the reviewer's
  diff (decision 3); the lists of changed files are already clipped
  (`statClipBytes`). Then, before the harness starts, Tenaz estimates the
  first request and blocks the run when it is over the session's budget,
  with the same reason as 1A. No session runs.
- B. Block at once, without clipping.
- C. Warn only.

The estimate: the prompt at 3 characters per token, plus a reserve for
the harness of 8192 tokens (a new parameter, `harnessReserveTokens`). The
reserve is the measured 6750 rounded up; 3 characters per token is the
diff's rate, so prose is overcounted by about a quarter, which errs on the
side of fitting. For session 025: 79670 / 3 + 8192 = 34749, against 30245
in fact. Over the 20 sessions of the run that got a reply, the estimate is 2.2k
to 4.5k tokens high, and never low.

A, because a prompt that cannot fit costs a session or more, and the
operator needs the reason more than the attempt.

## Consequences

- Neither refusal is resent seven times. fx's refusal and a prompt over
  its budget block the run with a reason that names the role, model,
  window and prompt size.
- A blocked run keeps its pinned parameters. To go on after such a block,
  the user lowers `diffShare` or `diffLimitBytes`, or gives the role a
  model with a larger window, in `run.json`, and runs with `--resume`.
- mlx-serve's memory refusal is told apart from other engine errors in
  the log, the event and the notice. The journal's field names and values
  stay as they are: `cause` is still `engine` or `harness`.
- Two new parameters, both with defaults, so old `run.json` files load.
- The exit codes of `tenaz run` do not change: a block is 4.

## Out of scope

- Shrinking a prompt after the engine refused it for memory.
- Counting tokens with the model's tokenizer.
- `tenaz status` showing each role's budget.
- Probing each role's model in the preflight (ADR 0011).
