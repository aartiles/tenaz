# ADR 0038: Note a removed private folder, and write Tenaz's files again

Status: accepted, 2026-11-19.

## Context

`.tenaz/private/` holds what a run keeps out of git: `orchestrator.log`,
each session's prompt, transcript and stderr, the run lock, Pi's session
files and each harness's own configuration (Pi's agent directory, the
fx, OpenCode, Qwen Code and dsh homes). It is inside the project, so the
agent's shell commands can remove it, with the sandbox on or off.

In the Raycaster Shooter run of 2026-11-19 (Tenaz 0.8.0, Pi 1.4.0) it was
removed during T009's builder session 021, most likely by a shell
command; the transcript that would show it went with it. The journal had
only a provider error (Pi could not open its session file) and then six
sessions that failed at once. Nothing said the folder had gone.

`run.json` is protected by the sandbox and put back after each session
(ADR 0008, README "Sandbox").

## Options

### A. Deny writes to it in the sandbox, like `run.json`

- The harness runs in the same sandbox as the shell commands it starts,
  and writes its own state, logs and sessions there. Denying the folder
  stops the harness.

### B. Snapshot it and put it back after each session

- Its files change during every session (logs, transcripts, Pi's
  sessions); a snapshot is out of date at once, and putting one back
  would undo the session's own logs.

### C. Move it outside the project

- Out of the agent's reach (in the git directory, say), but scripts and
  people read `.tenaz/private/orchestrator.log`, and the sandbox would
  have to let the harness write there. A larger change, for later.

### D. Notice it, say so, and write Tenaz's own files again

- After each session Tenaz checks the prompt file it wrote there when the
  session started. When it is gone, the folder (or that much of it) was
  removed during the session.

## Decision

**D.** The journal records `private-removed` with the session's label,
`orchestrator.log` says what may be lost, and Tenaz writes the prompt
file again. Separately, after every session Tenaz checks that the file at
`.tenaz/private/run.lock` is the one its lock holds (same device and
inode). When it is not, it takes the lock again, so another `tenaz run`
cannot start beside it from then on; when another run took it meanwhile, this run
stops with the error that names it, without saving its ledger over the
other run's. Each harness writes its own files
there before every session anyway, since the same day's fix.

## Consequences

- The run goes on; the session's own logs and the harness's state from
  before are lost, and the log says so.
- A removal is noticed when the session ends, not when it happens: the
  session itself may fail on its harness's missing files, and is rerun as
  any other.
- Until the session ends, another `tenaz run` can take the lock whose
  file was removed and work beside this one; this run then stops. A
  harness that cannot be opened or sent its prompt ends the session
  before these checks, and the run stops with that error, saving its
  ledger as before.
- Removing the prompt files alone is noted the same way, and removing
  the run lock alone has the lock taken again. Removing only other files
  there (a log, a transcript) is not noticed.
- Tested with the scripted harness (a builder whose shell call removes
  the folder); not yet seen in a real run since.
