# ADR 0019: OpenCode and Qwen Code as harnesses

Status: accepted, 2026-11-03.

## Context

Tenaz drove two coding agents: fx over ACP (the Agent Client Protocol)
and Pi in RPC mode. OpenCode and Qwen Code speak ACP too
(`opencode acp`, `qwen --acp`). On 2026-11-03 each one finished a run
with Tenaz, through a script passed as `--fx-bin` that read the fx
profile Tenaz wrote and started the other agent (see
`docs/COMPATIBILITY.md`). That run found a bug in how Tenaz reads their
tool calls, since fixed. It also showed what a first-class harness has
to do that the script did by hand:

- Point the agent at the engine proxy and name each session's model and
  its context window.
- Ask the client before every edit and command, so the watchdog and
  `toolMinutes` see each call.
- Keep the user's own configuration out: their providers, plugins, MCP
  servers, skills and memories.
- Qwen Code 0.26.6 keeps memories and records sessions in its home
  folder by default, and reads them back in the next session. Every
  Tenaz session must start from the brief, the plan and the repository
  alone.

## Options

### A. One generic `--harness acp` with the user's command line

- One kind covers every ACP agent, including ones not yet tried.
- The user writes each agent's configuration and keeps it in step with
  the run's models; nothing stops the agent's own memory or the user's
  plugins from reaching a session. Each role's model and window would
  need the user's script too.

### B. A kind per agent, each writing its own configuration

- `--harness opencode` and `--harness qwen`, like `fx`: Tenaz writes the
  agent's configuration into an isolated `HOME` under `.tenaz/private/`
  before each session, for that session's model.
- Each agent needs a little code and a version pinned at `init`, and a
  new agent needs a new kind.

### C. The agents' own configuration, left as it is

- Nothing to write. But the run would depend on whatever the user has
  configured, and Qwen Code's memories would carry from one session to
  the next.

## Decision

**B.** The two agents share the ACP adapter fx uses; only the
configuration each one is given differs.

- **OpenCode**: `opencode acp` with `HOME` and the XDG folders in
  `.tenaz/private/opencode-home`, and `OPENCODE_CONFIG` pointing at a
  config Tenaz writes there: one OpenAI-compatible provider at the proxy,
  the session's model with its window and output limit, `ask` for edits
  and commands, web fetches denied, sharing and updates off. OpenCode
  installs its plugin package into its config folder when it starts
  (about 60 MB on 1.19.5); npm's cache stays `~/.npm`, so after the
  first run that install needs no network.
- **Qwen Code**: `qwen --acp` with `HOME` in
  `.tenaz/private/qwen-home`, the proxy, the session's model and the
  `default` approval mode (ask before edits and commands) on the command
  line, and a `settings.json` that sets the window and turns off managed
  memory, its consolidation ("dream"), chat recording, updates and usage
  statistics. Tenaz also removes the context files Qwen Code reads from
  its `HOME` into every session (`QWEN.md`, `AGENTS.md`) before each
  session.
- Both get the media MCP server in their configuration with the media time
  limit, as fx does (for OpenCode, whether its `timeout` bounds a call and
  not only listing the tools is not confirmed), and the pinned skills
  linked into their own skill folder.

## Consequences

- A run on these agents is reproducible from `run.json` and does not see
  the user's configuration. Neither does it see the user's credentials,
  which a local engine does not need.
- When either agent changes its configuration format, Tenaz has to
  follow. The version is pinned at `init` and listed in
  `docs/COMPATIBILITY.md` with what was checked.
- Neither agent takes the steering messages fx and Pi do, so a watchdog
  stops their sessions without a warning first.
- Qwen Code's system prompt is about 21k tokens, against about 7k for
  OpenCode: a small context window fills sooner.
