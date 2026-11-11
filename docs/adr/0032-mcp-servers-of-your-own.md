# ADR 0032: MCP servers of your own

Status: accepted, 2026-11-10.

## Context

fx, OpenCode, Qwen Code, Pi and dsh run with a `HOME` or agent
directory of its own (ADR 0019), so the MCP servers in the user's own
agent settings never reach a run; a generic ACP agent sees only what its
own settings name.
The only server an agent is offered is Tenaz's media server, and only
when the run has an image or 3D engine.

Some goals are worked through a running program rather than files. The
example that asked for this is Blender: Blender Lab's MCP server (v1.0.3,
Blender 5.1 and later) runs Python inside a running Blender through an
add-on, and serves MCP over stdio or HTTP (`blender-mcp --transport http
--port 9191`). The add-on also runs in a background Blender
(`blender -b --online-mode -c blender_mcp`).

## Options

### A. `--mcp name=url` for HTTP servers

- `init` pins the servers in `run.json` (`launch.mcpServers`); `run
  --mcp` replaces them, as `run --workflow` does, and `run --no-mcp`
  clears them.
- They join the media server in the list Tenaz already gives each agent:
  fx, OpenCode and Qwen Code in their own configuration, the generic ACP
  agent in `session/new` and `acp-session.json`. Nothing new is written
  per agent.
- The user starts and stops the server; Tenaz does not watch it.
- Stdio servers need an HTTP bridge.

### B. Also stdio servers, `--mcp-command name="cmd args"`

- More servers work without a bridge.
- Each agent's configuration writes stdio servers in its own format, and
  the sandbox must allow the command. Four writers to change and test.

### C. Read the user's own agent settings

- No new flag.
- Brings back what ADR 0019 keeps out: every server, plugin and memory
  the user has configured, different on each machine.

## Decision

Option A. It reuses the path the media server already takes and is
enough for Blender's server, which speaks HTTP. A name may hold letters,
digits, `-` and `_`; `media` is Tenaz's. Pi runs with `--no-mcp` and dsh
is given no MCP server, so `init` and `run` refuse servers with either,
with the reason, before anything is created or started, as they refuse
swapping a run cannot do.

## Consequences

- Each server's calls get the media time limit (`--media-timeout`,
  1800 s by default) in fx's, OpenCode's and Qwen Code's configuration;
  only fx's has been seen to hold (OpenCode's may bound only listing the
  tools). A server passed in `session/new` gets the agent's own limit.
- A call to such a server does not free the LLM as a media job does: the
  server shares the machine's memory with the engine.
- `verify` cannot see the state a server keeps, such as Blender's scene.
  A brief should ask for that state to be saved to a file `verify`
  checks.
- `run` checks the pinned servers again, so one edited into `run.json`
  by hand meets the same rules.
- Stdio servers, Pi and dsh are left for later.
