# ADR 0027: ACP agents behind one trait

Status: accepted, 2026-11-07.

## Context

Tenaz drives three agents over ACP: fx, OpenCode and Qwen Code (ADR
0019). Each has a module in `crates/acp` that repeats the same steps: it
writes its configuration into an isolated `HOME` for each session's
model, builds a command line that puts it in ACP mode, goes through the
sandbox launcher, and reports `--version`. Only a few things differ per
agent: the configuration format, the arguments (Qwen Code names the
model, fx names a log file), the environment, and fx's shim that puts
its `refused` stop reason right.

DeepSeek Harness (`dsh`) speaks ACP too. A probe on 2026-11-07 ran it
through a script passed as `--fx-bin`, which read fx's profile and wrote
dsh's own. That works, but it is the same hack ADR 0019 replaced, and
the maintainer asked for the agents to be generic, with fx and dsh as
options rather than dsh built on fx.

## Options

### A. Add dsh as a fourth module, like the others

- Smallest change today.
- A fourth copy of the launch and configuration steps, and the
  wiring grows one more near-identical function. Each new agent repeats
  it again.

### B. One trait for an ACP agent, each agent an implementation

- `AgentProfile` says what differs: its kind, its configuration and how to
  write it for a model, its program, arguments, environment and wrapper.
  `agent::harness` and `agent::configured` do the rest the same way for
  every agent, and the wiring has one path for all of them.
- fx, OpenCode, Qwen Code and dsh each implement it. `run.json` keeps
  the same `harness` names; nothing a script reads changes.
- Moving the existing agents costs one refactor, with their tests
  kept.

### C. Only a generic `--harness acp` with the user's command line

- Rejected in ADR 0019 for the same reasons: the user would write each
  agent's configuration and keep it in step with the run's models, and
  nothing would keep the agent's own memory or plugins out.

## Decision

**B**, plus a generic agent for what has no implementation yet:
`--harness acp` runs a command the user names. Before each session
Tenaz writes the engine's URL, the session's model and its limits to
`.tenaz/private/acp-session.json` and names that file in
`TENAZ_AGENT_SESSION`, so a small script can configure an agent Tenaz
does not know. It replaces
pointing `--fx-bin` at such a script.

## Consequences

- A new ACP agent is one implementation of `AgentProfile` with its
  configuration's tests, plus its `HarnessKind`, its `--<agent>-bin`
  flag and a wiring function; the session code and the launch do not
  change.
- The generic `acp` kind gives none of the isolation the named agents
  have: what the user's script does is up to it. The docs say so.
- dsh did not ask for permission before its tool calls in the probe, so
  under it the runner does not see a call before it runs (see Known gaps
  in `docs/COMPATIBILITY.md`).
