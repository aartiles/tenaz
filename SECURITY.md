# Security

## What Tenaz does and does not protect

Tenaz drives a coding agent that runs shell commands and writes files as
your user. By default it is not a sandbox:

- Every permission request the agent makes is approved. The commands come
  from a model, so treat them as untrusted code.
- The project's `verify` script runs with your user's rights and a time
  limit only; processes it leaves behind are not stopped.
- A failed attempt is undone with `git reset --hard` and `git clean -fd` in
  the run's worktree.
- While a run works, the engine proxy and the media MCP server listen on
  `127.0.0.1` on ports the OS picks, without authentication.
- fx and Pi run with their own configuration under `.tenaz/private/`, which
  keeps your own agent settings out of the run. It is not isolation: the
  agent can still read and write anything your user can.

`sandbox=true` (see the README's Sandbox section) runs the agent and
`verify` in an OS sandbox, Seatbelt on macOS and Landlock on Linux: they
write only to the project, its git directory, the temporary directories and
the configured caches; they cannot change the git hooks and config or
`.tenaz/run.json`, which Tenaz uses outside the sandbox (on Linux Tenaz
puts them back after each session and `verify` run instead of the kernel
refusing the write); and they cannot read the configured secrets
directories. It reads everything else, and the network is open. It limits
what a mistaken model can damage; it is not a boundary against a hostile
brief or repository.

Run Tenaz in a container, a VM or a dedicated user account, and only on
briefs and repositories you trust.

## Reporting a vulnerability

Please report it privately through GitHub's
[private vulnerability reporting](https://github.com/aartiles/tenaz/security/advisories/new)
rather than in a public issue. Include the Tenaz version (`tenaz --version`),
the platform, the harness and engine, and the steps to reproduce. You will
get an answer within a week.

Only the latest release is supported.
