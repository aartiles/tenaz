# ADR 0014: A capacity ceiling is tried again

Status: accepted, 2026-10-31.

## Context

When the engine runs out of memory, Tenaz lowers the run's context budget
to the headroom share (`capacityHeadroom`, 0.8) of what the engine said it
could hold, and raises it halfway back after `capacityRecoverSessions`
sessions in a row the engine held. Since the Wild West run, a raise the
engine overflows also sets a ceiling: later raises stop there. In that run
the engine held 86016 tokens every time, and raising past 68812 only
overflowed again, six times.

The ceiling never went away. In the Digger below the Mine run (2026-10-30,
Qwen3.8-Flash-Next 2.6bpw, fx, 85196 configured) a raise overflowed at
12:56 while memory was short, the engine said 73024, and the ceiling became
58419. From 19:09 the budget sat at 58419 for more than 20 sessions with
normal memory pressure and no capacity error. Sessions that ran past their
budget before the watchdog stopped them reached 77257 tokens without
the engine failing. The memory freed since was never used: the budget
could not go above 58419 for the rest of the run.

## Options

### A. Keep the ceiling for the whole run

What the Wild West run needed. A run whose memory pressure eases keeps the
budget it had at its worst moment, for hours.

### B. A session's peak raises the ceiling

A session the engine held without critical pressure proves the engine can
hold its `context_peak`. The ceiling could move to the headroom share of
it, as it is set from the engine's own figure. Only sessions that run past
their budget show more than the ceiling, and the headroom share of 77257
is 61805: little of the memory freed. It also depends on the watchdog
letting sessions overshoot.

### C. The ceiling is tried again after twice the wait

Held at the ceiling for twice the sessions a raise waits for, the next
raise aims at the largest configured budget again, as if there were no
ceiling. If the engine overflows it, the ceiling is set again from what
the engine says then, and the wait doubles, as for any failed raise.

## Decision

C. The engine only says what it can hold when a prompt overflows it, so
memory freed later is found by trying, as the raises already do. The cost
is bounded: each try that fails costs one rerun of a session, not an
attempt, and failed tries make the next one wait longer, up to 32 sessions
at the ceiling with the default `capacityRecoverSessions` of 4: 8 to 32
by how many raises failed in a row, since, as for any raise, the raise
after one the engine held starts that escalation again. A run like Wild
West, where no raise holds between tries, pays one rerun in 32 sessions;
a run like Digger gets its budget back.

## Consequences

- The ceiling is dropped in the same step that raises past it; a raise the
  engine overflows sets it again from that overflow alone.
- The counting moves into the ledger (`Ledger::note_cap_held` and
  `Ledger::lower_cap`), where it is tested session by session.
- Sessions held at the ceiling keep counting instead of starting over at
  each due raise, so the try comes after twice the wait in a row. Critical
  memory pressure still resets the count.
- Not yet observed in a run with an engine whose memory comes back
  (`docs/COMPATIBILITY.md`).
