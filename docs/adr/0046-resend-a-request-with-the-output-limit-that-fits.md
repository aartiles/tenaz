# ADR 0046: Send a request again with the output limit the engine has room for

Status: accepted, 2026-11-26.

## Context

Sushi refuses a request whose prompt and output limit together do not
fit the memory it has now, with a 400 that names the limit that would
fit: "Context 103038 does not fit in GPU memory: the request needs
3.6 GB and 3.4 GB is available. max_tokens 13186 fits (asked 16384);
maximum context: 99840 tokens." The harness ends the session on the
error. In the Last Delivery run of 2026-11-18 (Sushi with fx 0.0.17), 7
such refusals ended 6 builder sessions, three of them after 46 to 57
turns of work, and each was rerun from the start after a wait: about
80 minutes of sessions and waits in all.

The output limit asked was 16,384 tokens, but replies there seldom need
it: of 5,521 replies, p50 331 tokens, p99 5,678, p99.9 9,378, max
16,009. Six of the seven refusals named a limit of 8,598 tokens or more;
one named 3,696.

## Options

### A. Leave it to the runner

- The runner already lowers the context budget when the engine names how
  much it can hold (ADR 0014), and now reads this message too.
- The session is still lost: the budget applies to the next session, and
  the refused request was mid-session, with its work not yet committed.

### B. The engine proxy sends the request again with the limit that fits

- The harness never sees the refusal; the session goes on. Only the
  proxy sees the request and the engine's answer together, so only it can
  do this without the harness's help.
- A reply longer than the lowered limit is cut there, as at any output
  limit, and the next session of that role reasons at a lower effort
  (ADR 0042). With the replies above, that is rare.
- The journal shows the lowered limit in the request's `sent` settings,
  which differs from the run's.

### C. Lower the output limit of every request after the first refusal

- Saves the refused request each time, but a limit lowered for one large
  prompt would stay for later small ones, and the run would need to raise
  it again by trying, as for the context cap.

## Decision

B. On a 400 that names `max_tokens N fits (asked M)`, the proxy sends the
same request once more with every output-limit field it names (or the
API's own, when it names none) set to N, when N is at least a quarter of
M: 4,096 tokens at the default 16,384, room for about 98 % of the
replies above, while less would cut too many to be worth the request. A
second refusal, or less room, is passed on as it came, and the runner
decides (A). The resend waits on the lease like any request, keeps the
request's id, is logged, and is journaled as an `output-limit-lowered`
watchdog event of the session for the harnesses that take the proxy's
events (fx and the other ACP agents; not Pi).

## Consequences

- A refusal of this kind no longer costs a session in the common case.
- Every later request of the session still asks the full limit, and its
  prompt only grows: once refused, a session is likely refused, and sent
  again, at each request after, one more round trip and journal event
  each. Once the room left is under a quarter, the refusal is passed on
  as before. Lowering every request's limit (C) would save the round
  trips at the cost of cutting replies that would have fit.
- The time a resent request took, and its time to first byte, include
  the refused round trip: they are the harness's wait.
- Only Sushi's wording is read; another engine's refusal of an output
  limit is passed on as before.
- Not yet run against Sushi itself: tested with the recorded message, a
  fake engine behind the proxy and the scripted harness.
