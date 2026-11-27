# ADR 0047: A session kept going at a lowered output limit is not held

Status: accepted, 2026-11-26. Amends ADRs 0043 and 0046.

## Context

ADR 0043 lifts the run's context cap to the peak of a session the engine
held well past it, with no engine error. ADR 0046 has the engine proxy
send a request again, once, with the output limit Sushi says fits, and
journals an `output-limit-lowered` event for the session. Together, a
session that went on only because the proxy lowered its output limit
counts as held: its peak lifts the cap, and the session counts toward an
ADR 0014 raise.

That peak is a prompt size where the engine said the full output limit
does not fit. Every request asks the full limit (ADR 0046), so the next
session at that cap is refused again at that size. When the room left is
under a quarter of the limit, the refusal is passed on: the cap is
lowered, the ceiling set, and the wait doubled. The run goes round lift,
failed raise, ceiling, doubled wait.

## Options

### A. Lift the cap only to where the full output limit fits

For such a session, lift the cap to `peak - (asked - fits)`, the largest
`asked - fits` of its events.

- Keeps part of the lift.
- The event names `asked` and `fits`, not the prompt size of the refused
  request. That prompt is at most the peak, and grows after the refusal,
  so `peak - (asked - fits)` can still be past where the full limit fits.
  The lift stays a guess with a known way to fail.

### B. Do not count such a session as held

No lift, no count toward a raise, and the count starts again, as after
critical memory pressure.

- The engine has said it was short of memory in that session; a row of
  sessions without it running out is what a raise waits for (ADR 0014).
- The cap climbs only by the ADR 0014 raise, which a failed raise already
  slows. A run whose long sessions are all refused at the output limit
  keeps its cap.

## Decision

B. A refusal at the output limit is the engine running out of room,
caught in time. A, the other way, needs the prompt size at the refusal,
which the proxy's event does not carry.

## Consequences

- A session with an `output-limit-lowered` event gives no
  `context-cap-overrun` and no step toward a `context-cap-raised`, and
  starts the count of held sessions toward a raise again.
- Unlike an overflow, the event neither lowers the cap nor fails a raise
  still pending: it names no figure for what the engine can hold, and the
  session's budget is the cap the engine served.
- Only harnesses that take the proxy's events (fx and the other ACP
  agents) record the event. With Pi the resend is only logged, and such a
  session still lifts the cap as in ADR 0043.
- Not yet run against Sushi: tested with the scripted harness.
