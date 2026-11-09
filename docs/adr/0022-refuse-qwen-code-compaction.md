# ADR 0022: Refuse Qwen Code's compaction in the proxy

Status: accepted, 2026-11-04. Amended by ADR 0030, which refuses dsh's
summary request the same way.

## Context

Tenaz ends a session that runs out of context and starts a fresh one from
the task's notes (a context continuation). Each harness also has its own
way to make room: Pi and fx report a compaction over their protocol, and
the watchdog steers at the first and stops the session at the second.

Qwen Code 0.26.6 compacts on its own, near 75% of the window it is told
(`contextWindowSize`): it sends the engine one more request, whose prompt
opens with "You are the component that summarizes a conversation when
its context window is about to overflow". It says nothing about it over
ACP, and it has no setting to turn it off: `context.autoCompactThreshold`
only caps the trigger, which is held lower by the room it keeps for the
summary.

In the articulated-cabinet run (2026-11-03 and 04, Qwen3.8 Flash Next on
Sushi, a 131072 window and an 85196-token budget) Qwen Code compacted 25
times at 99k to 105k tokens. Each summary was 7k to 18k tokens and took 3
to 8 minutes, 270k tokens in all. Tenaz saw each as an empty reply,
counted no compaction, and stopped the session at its context budget right
after it, so no summary was ever used.

## Options

### A. Refuse the request in the proxy

- Every request already goes through Tenaz's proxy. It recognises the
  summarizing prompt, answers it with an error without sending it to the
  engine, and the session ends there as one out of context; a fresh one
  continues.
- Depends on the opening of Qwen Code's prompt, which a later version may
  change. If it does, compactions are wasted again but nothing breaks.

### B. Tell Qwen Code a larger window

- Set `contextWindowSize` above the real window, so that its trigger is
  past the point where Tenaz stops a session.
- Configuration only, but untrue: if Tenaz did not stop a session in time,
  Qwen Code would send the engine more than it can hold.

### C. Let it compact

- Count the compaction and let the session go on with the summary.
- The watchdog sees context use from the engine's usage; the summary
  request alone is past the budget, and a summary from a 2.6-bit model
  under a reasoning budget is not a better handoff than the task's notes.

## Decision

A. The proxy takes the prompt's opening as `compaction_prompt_opening`,
set only for `--harness qwen`, and refuses a request whose last message
is from the user and opens with it (Qwen Code sends the summarizing
prompt there; its system prompt is the agent's own). A refused request
becomes a `CompactionRefused` session event; the watchdog counts a
compaction and stops the session with "the harness began to compact its
context", which the orchestrator treats as a context stop.

## Consequences

- A Qwen Code session ends at about 99k tokens on a 131072 window instead
  of growing past 110k first, and no time goes to a summary nobody reads.
- Qwen Code takes the error as a failed compaction and goes on with its
  next turn, which may reach the engine before the abort does; that turn
  is mostly cached and is cancelled with the session.
- A session the watchdog does not watch (the preflight) is not stopped:
  the refusal is only counted.
- The new abort reason, "the harness began to compact its context",
  appears in `run.json` and the journal.
- A new Qwen Code version must be checked for the prompt's opening
  (`COMPACTION_PROMPT` in `crates/acp/src/qwen.rs`).
- Not yet seen in a real run.
