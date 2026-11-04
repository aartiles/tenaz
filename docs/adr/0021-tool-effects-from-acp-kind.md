# ADR 0021: Tool effects from ACP's `kind`

Status: accepted, 2026-11-04.

## Context

The watchdog's idle rule counts a session as idle when a turn neither
changes the worktree nor reads something new, and it counts writes for
the journal. Until now it knew reads and writes only by tool name: `read`,
`write`, `edit` (Pi), `read_file`, `write_file`, `edit_file` (fx).

Qwen Code 0.26.6 names its tools `ReadFile`, `WriteFile` and `Edit`. In a
real run (2026-11-03, the articulated cabinet) session `043-builder-T007`
read 20 different files with `ReadFile` and was stopped as "20 turns
without changing or reading anything new", and every session journaled
`writes: 0`. Qwen Code also titles a call it cannot describe yet
`ToolCall`, so even a full list of names misses some calls.

ACP tool calls carry a `kind`: `read`, `edit`, `delete`, `move`,
`search`, `execute`, `think`, `fetch`, `switch_mode` or `other`. Qwen
Code 0.26.6 sets it from the tool itself (seen in its sources); OpenCode
is expected to, but that has not been checked. Pi does not speak ACP.

## Options

### A. More names

- Small: add `ReadFile` and `WriteFile` to the lists.
- Each new agent, or a renamed tool, breaks the rule again until someone
  notices; Qwen Code's `ToolCall` placeholder is still missed.

### B. Only `kind`

- One rule for every ACP agent, whatever it names its tools.
- Pi has no `kind`, and an agent may send `other` for a tool it does not
  classify (Qwen Code does for a tool it cannot find).

### C. `kind` first, then the name

- The ACP translator turns `kind` into an effect on the tool call event:
  `read` reads, `edit` writes. Any other kind says
  nothing, and the watchdog falls back to the name lists, which now also
  hold Qwen Code's names. A later update whose kind says nothing does not
  undo an effect already declared.
- One more optional field on the event.

## Decision

C. The event's `effect` is optional and left out of `rpc.jsonl` when
empty, so old logs still read.

## Consequences

- `delete` and `move` are left out: the outside-write warning tells the
  model to write the file again, which is wrong for a delete, and a
  move's paths have other names. No agent has sent them to Tenaz.
- A tool an agent declares as `edit` counts as a write and its path is
  checked against the project, like `write` (ADR 0008).
- An agent that labels a tool's kind wrongly is believed. A shell call is
  `execute`, so writes made by shell commands are still seen only by the
  worktree fingerprint, as before.
