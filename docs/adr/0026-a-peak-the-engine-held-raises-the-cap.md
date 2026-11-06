# ADR 0026: A peak the engine held raises the cap

Status: accepted, 2026-11-06.

## Context

When the engine runs out of memory, Tenaz lowers the run's context budget
to the headroom share (0.8) of what the engine said it could hold. It
raises the budget only after `capacityRecoverSessions` sessions in a row
held at the cap, and past a ceiling only after twice that wait (ADR 0014).

In the articulated-cabinet run (2026-11-06, Qwen3.8-Flash-Next 2.6bpw,
Qwen Code) other work on the machine pressed memory at 15:43. The engine
said about 56178 tokens, and the cap and the ceiling became 44942. Memory
was freed soon after. From then on seven sessions in a row, the splits of
T032 and three replans, ran to 58509-61334 tokens with no engine error and
low pressure. The watchdog stopped each at 1.3 times the cap. They needed
more than that just to read what their role reads, so none finished; T032
was blocked, the replans failed, and the run blocked at 16:33 with seven
of the eight sessions the raise was waiting for.

## Options

### A. Keep waiting for the raise

ADR 0014 as it is. Each wait is cheap when sessions still finish under the
cap. When the cap is under what a role needs, every session in the wait
is wasted, and a run can block before the raise.

### B. Count a session cut at the cap as held, with a shorter wait

Fewer sessions lost, but still some, and the wait is a guess.

### C. A peak the engine held raises the cap at once

A session that went past the cap with no engine error and no critical
memory pressure proves the engine holds its `context_peak` now. The cap
rises at once to the headroom share of that peak, as it is lowered to the
headroom share of the engine's own figure, and a ceiling under it rises
with it. ADR 0014 rejected this as the only way back, since only sessions
that overshoot show it; here it is added to the raises, not instead.

## Decision

C. It uses only what the engine has shown, with the same headroom as a
lowering, so it does not raise the risk of an overflow: if the engine
overflows anyway, the cap is lowered as before. The raises of ADR 0014
still climb above it.

## Consequences

- It works with `capacityRecoverSessions` at 0, when the raises of ADR
  0014 are off.
- In the cabinet run, the first replan at 61334 would have raised the
  cap to 49067 and its successor's stop to 63787.
- The journal gains a `context-cap-overrun` record with the session's
  `context_peak` and the new `context_cap`.
- A raise this way is not a raise tried: it never doubles a wait. The
  engine held more than any raise still pending, so that raise counts as
  held. The session that lifts the cap did not test the new one, and the
  count toward the next raise starts again.
