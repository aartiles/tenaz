# ADR 0030: Turn off dsh's automatic compaction

Status: accepted, 2026-11-09.

## Context

Tenaz ends a session that reaches its context budget and starts a fresh
one from the task's notes (ADR 0022). DeepSeek Harness (`dsh`)
0.2.2-rc.1 also summarizes its own conversation: its `compaction-basic`
plugin sends one more request whose last user message opens with "You
are now acting as a compaction engine", and puts the summary in place of
the earlier history.

It does so when its own count of the history reaches
`min(W × 0.8, W − O − headroom)`, with a default headroom of 65536.
For a 131072-token window and 16384 output tokens that is 49152 tokens,
well below Tenaz's budget of 85196. dsh counts the reasoning it keeps in
its history, which the proxy removes before sending (ADRs 0023 and
0028), so it reached that count with 19k to 48k tokens actually sent.

In a raycaster run with the 0.6.0 release (started 2026-11-08 at 22:04
UTC, Qwen3.8-Flash-Next on Sushi 1.5.1, no changes during the run; not
the 19-hour run in docs/COMPATIBILITY.md) dsh summarized 18 times. The
summaries and the requests after them, which read the history again
without a cache, took about 51 minutes of a 6 h 57 min run. Two
summaries failed (the reply was all reasoning, or ended in a tool call);
dsh repeated each with the same prompt and got the same result. After a
summary the model read the same files again: one session read the same
two files 21 times.

## Options

### A. Turn off automatic compaction

- The patch Tenaz writes sets `auto: false` on the `compaction-basic`
  entry. dsh then summarizes only on `/compact`, which Tenaz never sends.
  Tenaz's budget ends the session, as for every other harness.
- `auto: false` turns off every automatic trigger: the check between
  steps, compaction on a model switch (Tenaz never switches models
  inside a session), and recovery from a context-overflow error. Tenaz
  stops a session well before the window is full, so only a single very
  large tool result could overflow it; the session then ends on the
  engine's error.
- Depends on the entry's id, `compaction-basic`, in dsh's `dsh-base`
  bundle. If a later dsh renames the entry or its package, dsh skips the
  patch with a warning in its own log and compacts again; Tenaz does not
  see this.

### B. Raise the threshold

- Set a small `headroomTokens` (for example 8192), so the threshold sits
  above Tenaz's budget.
- dsh still counts reasoning the proxy removes, so a session that
  reasons a lot can still reach the threshold before the budget. Some
  summaries would remain, and when they happen depends on how much the
  model reasons.

### C. Refuse the request in the proxy

- As for Qwen Code (ADR 0022): recognise the opening and answer with an
  error, which ends the session as one out of context.
- With dsh's threshold where it is, sessions would end at 19k to 48k
  tokens instead of 85k: more sessions, each starting again from the
  notes.

## Decision

A: Tenaz's patch turns off dsh's automatic compaction.

## Consequences

- dsh sessions end at Tenaz's context budget, steered by the context
  rules, with no summaries. The raycaster run spent about 51 minutes on
  them; some of that returns as context continuations instead.
- A new dsh version must be checked for the `compaction-basic` id, its
  package name, and that the entry still has no config of its own,
  which the patch's config would replace.
- The patch was checked with `dsh --profile acp --dump-config`; no real
  run has used it yet.