# ADR 0008: Writes outside the project

Status: accepted, 2026-10-28.

## Context

On 2026-10-28 LFM3-VL-2B on LM Studio, with Pi, wrote `spec.md` and
`map.md` outside the project three times during bootstrap step 1: twice to
the parent repository's `.tenaz/`, once to `~/.tenaz/`. Tenaz validated the
project, found the files missing, and the run blocked. Nothing said where
the files had gone; they were found and moved by hand.

The sandbox (`sandbox=true`) makes such writes fail, but it is opt-in, and
the benchmark runs leave it off. Without it nothing looked at where the
agent writes.

The write tools name their file in their arguments (`path` in Pi and fx,
`file_path` elsewhere), and the watchdog sees every tool call start. A
shell command can write anywhere, and its text cannot be read reliably
(`cd ..`, variables, scripts it runs).

## Options

### A. Only note it

- The journal and the log name the file; the session goes on as before.
- The model is not told until the next session's briefing, if there is one.

### B. Note it and steer the session once

- As A, and the session is told once that the file is outside the project
  and where its files belong, while it can still write them again.
- A steer costs a few dozen tokens of context.

### C. Abort the session

- Stops a model that writes elsewhere at once, but also ends a session that
  would have written the right file next. The validation already fails a
  step whose files are missing.

### D. Turn the sandbox on by default

- The writes fail where they happen, and the model sees the error.
- The sandbox has not run on a Linux kernel yet, cannot start when Tenaz
  itself runs in a sandbox on macOS, and refuses the package caches it was
  not told about. A default that stops runs on some machines is worse than
  a note.

## Decision

**B**, for runs without the sandbox. A write tool's target is resolved
against the project root, where sessions run (`~` expanded, `.` and `..`
taken out as written), and checked against the places the sandbox allows:
the project, its git directories, the temporary directories, on macOS the
per-user cache, `/dev` and `sandboxAllowWrite`. The sandbox adapter builds
that list once per run, from the same policy the sandbox uses, so the two
cannot drift apart.
Each file outside them is:

- a watchdog event `outside-write` with the path, and in the session's
  journal record `outside_writes`;
- a line in `orchestrator.log`: `<label>: wrote outside the project: <path>`;
- once per session, a steer that names the file and the project root.

A bootstrap step that is not accepted lists the files before the errors in
its problems, so the next session's briefing and the block message say where
they went.

## Consequences

- The write is not stopped and the file is not moved: Tenaz does not touch
  paths outside the project.
- Writes from shell commands (`echo > ../x`, `cp`, a script) are not seen.
  Only the sandbox catches those.
- Symbolic links are not followed: a link inside the project to a
  directory outside it counts as inside, and a path that reaches the
  project through a link outside it counts as outside. The project, the
  temporary directories and the per-user cache are allowed both as given
  and as resolved.
- fx runs with `HOME` set to its profile; a `~` in an fx write is read as
  the user's home. Either way the file is outside the project.
- Tested with the scripted harness; not yet seen against a real model.
