# ADR 0043: A peak the engine held is the cap, not 0.8 of it

Status: accepted, 2026-11-26. Amends ADR 0026.

## Context

ADR 0026 lifts the run's context cap when a session runs past it with no
engine error and no critical memory pressure. It lifts it to the headroom
share (`capacityHeadroom`, 0.8) of the session's `context_peak`, as a
lowering takes the headroom share of what the engine said. The watchdog
stops a session at `contextAbort` (1.3) times the cap. A session that runs
to that stop lifts the cap to 0.8 × 1.3 = 1.04 times what it was.

In the Last Delivery r2 run (2026-11-24, Qwen3.8-Flash-Next 2.6bpw on
Sushi, fx) the engine said 35832 tokens under memory pressure, and the cap
became 28665. Builders needed up to 79086 tokens, which the engine held
with no error. Sixteen builder sessions were stopped at 1.3 times the cap,
peaks running a little past the stop, each lifting it by 1 to 7 %: from
28665 to 59276 over about two days. Each overrun also starts the count
toward an ADR 0014 raise again, so that raise did not fire while builders
kept overrunning.

## Options

### A. Keep the headroom on a held peak

ADR 0026 as it is. A few per cent per stopped session when the cap is
far under what the engine holds.

### B. Lift the cap to the peak itself

The headroom covers the error in the engine's figure: the engine says
what it can hold when a prompt overflows it, under the memory pressure of
then. A peak is what the engine did hold, with no error. The next stop is
then 1.3 times that peak, a 30 % step. That is more than the engine has
shown it holds, so the lift is a raise tried: a ceiling under the peak is
dropped, as when a raise is tried past it, and if the engine overflows,
the cap is lowered, the ceiling set again from that overflow, and the
wait before the next raise doubles, as for a raise that overflows (ADR
0014).

### C. Count an overrun session toward the ADR 0014 raise

Instead of starting the count again. A raise then fires after four
sessions, overrun or held, and goes halfway to the largest budget. It
still waits four sessions, and with B the overruns are few.

### D. Steer at the cap, stop at `contextAbort` times the configured budget

Let the engine's own overflow stop a session. Every session past the cap
then risks an overflow, and the steer alone would hold the session back.

## Decision

B, with the trigger of ADR 0026 kept: the cap is lifted only when the
headroom share of the peak is over it, that is a peak past 1.25 times the
cap. The watchdog steers a session only once it passes its budget, so
many end a little past it; those count toward the ADR 0014 raise as
before, rather than lifting the cap a few per cent and starting the count
again. C and D are not taken: with B, Last Delivery r2 would have reached
62975 after three stops instead of 16.

## Consequences

- `capacityHeadroom` decides when a peak lifts the cap, not where to: the
  cap becomes the peak. The journal's `context-cap-overrun` record keeps
  its fields; its `context_cap` is now the session's `context_peak`, or
  null past the largest budget.
- A lift is now a raise tried, which ADR 0026 said it was not: it drops a
  ceiling under the peak instead of raising it to the headroom share of
  the peak, and an engine overflow after it sets the ceiling again and
  doubles the wait. A run whose engine held a peak once and then cannot
  hold 1.3 times it pays one uncounted rerun, then waits longer before the
  cap climbs past that ceiling.
- Not yet seen in a run (`docs/COMPATIBILITY.md`).
