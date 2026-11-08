# ADR 0028: Remove the earlier reasoning in steps

Status: accepted, 2026-11-08. Amends ADR 0023.

## Context

ADR 0023 made the proxy remove the reasoning of every assistant message but
the last. Its Consequences said this cuts the engine's cached prefix, but
did not measure it. Each request removes the reasoning of the message that
was last in the request before, so the prompt changes a turn before its end
and the engine reads that turn, its tool result and the new reply again.

The raycaster run (2026-11-08, Qwen3.8-Flash-Next on Sushi 1.5.1, dsh)
measured it over its first 679 requests:

- 83.7% of prompt tokens came from the cache, against about 95.7% in
  three Sushi runs from before ADR 0023;
- the median time to first token was 3.48 s, against about 2.2 s; the
  prefill speed was the same, about 670 tokens/s;
- 21% of the uncached tokens came from requests whose prompt got shorter
  than the one before, and 40% from requests whose cache stopped a median
  of about 2.2k tokens before the end of the previous prompt.

To weigh the options, the run's `requests.jsonl` was replayed under each
one. The model: a request is the previous one plus the reply and the tool
result, minus the reasoning the option removes; the engine caches the
previous prompt and its reply in 512-token blocks; a prompt that shrinks
more than the option explains is dsh pruning its own history, and starts
over with the cache the run logged. The replay used the run's first 713
requests (it was still going); under ADR 0023 the model gives 84.9%
cached where the run logged 83.8% for those requests.

| Option | Cached | Uncached tokens | Extra prefill | Peak prompt, median / max |
|---|---|---|---|---|
| All reasoning kept (before ADR 0023) | 93.2% | 1.49M | 0 | 29.7k / 66.7k |
| Only the last kept (ADR 0023) | 84.9% | 2.72M | 31 min | 27.4k / 46.6k |
| None kept, not even the last | 90.6% | 1.63M | 4 min | 26.4k / 40.6k |
| Last K kept, in steps of K=4 | 88.9% | 2.08M | 15 min | 28.7k / 54.4k |
| Last K kept, in steps of K=8 | 90.4% | 1.88M | 10 min | 28.7k / 54.5k |
| Batches over a budget of 4k tokens | 89.1% | 2.02M | 13 min | 28.7k / 49.2k |
| Batches over a budget of 8k tokens | 90.3% | 1.88M | 10 min | 29.6k / 50.7k |
| Batches over a budget of 16k tokens | 91.8% | 1.70M | 5 min | 29.7k / 60.2k |

Extra prefill is the uncached tokens beyond keeping it all, at 670
tokens/s, over the 8 hours of the run.

## Options

### A. Batches over a budget

- The proxy keeps the reasoning until what is kept before the newest
  assistant message passes a budget, then removes all of it but the
  newest one's in one step. Between steps each request extends the one
  before it.
- The proxy keeps no state: it replays the conversation's reasoning sizes
  from the start on each request. While the harness only appends
  messages, the next request reaches the same steps, so a removed
  reasoning does not come back. After the harness prunes or compacts its
  history, the steps start over from what it still sends, and some
  reasoning removed before may be sent again; the cache is broken there
  anyway.
- The window carries at most the budget of earlier reasoning, plus the
  newest. The budget follows the reasoning: a model that reasons little
  steps rarely.

### B. Keep the last K, remove older ones in steps of K

- Also stateless and stable between steps, but K counts messages, not
  tokens: one long reasoning or K short ones cost the same. For the same
  cache it held a larger peak than A (54.5k against 50.7k at K=8).

### C. Keep the current behaviour

- The smallest window that keeps the last reasoning, and the most
  prefill: about 21 minutes more than A in the raycaster run.

### D. Remove all of it, the last one's too

- The prefix only grows, and the window is the smallest. But the model no
  longer sees the reasoning behind the tool call whose result it is
  reading. ADR 0023 kept it on purpose, and no run tells whether the
  model can do without it.

## Decision

A, with a budget of 8192 tokens (`pastReasoningTokens`, estimated at 4
bytes per token like the rest of Tenaz). `--set pastReasoningTokens=0`
removes it every turn, as ADR 0023 did; `keepPastReasoning=true` still
sends it all, whatever the budget.

## Consequences

- In the raycaster run's replay the cache rises from 84.9% to 90.3% and
  the extra prefill falls from 31 to 10 minutes.
- A session carries up to about 8k tokens more of reasoning: the median
  peak prompt rose by 2.2k in the replay, the largest by 4.1k. A session
  near its context budget reaches it a little sooner.
- The model sees the reasoning of several earlier turns, not only the
  last; whether it repeats less work because of that is not measured.
- 8192 was read off one run of one model on one engine. A model that
  reasons more steps more often but keeps the same bound.
- Reasoning a harness sends as JSON rather than text counts as its JSON.
- The replay is a model of the engine's cache, not a run: not yet seen in
  a real run.
