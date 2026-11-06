# ADR 0024: A note in place of a video file

Status: accepted, 2026-11-06.

## Context

Qwen Code 0.26.6 sends a video the model read with `ReadFile` as a
`video_url` content part holding the whole file as a `data:video/mp4`
URL, and keeps it in every later request of the session. Sushi takes
video only as a list of image frames, and refuses such a request with
400 ("video could not be decoded: its frames must be base64 image data
URLs of one size").

In the articulated-cabinet run (2026-11-05, Qwen3.8 Flash Next on Sushi)
every reviewer of T014 read `outputs/motion.mp4`. Each session failed
after a few turns with provider errors, Tenaz retried the review six
times over an hour, then blocked the run as "engine unavailable".

## Options

### A. A note in the proxy, for every engine (chosen)

- The proxy replaces each `video_url` part whose URL is a `data:video/`
  file with a text note: this engine does not take video files; extract
  frames as images and read those.
- The model learns what to do in the same turn, and the session goes on.
- The replacement is the same in every request, so the engine's cached
  prefix stays valid.
- An engine that could decode the file never gets it. None of the local
  engines Tenaz drives is known to.

### B. Only for engines that refuse it

- Probe the engine at preflight with a short video and replace only when
  it refuses.
- One more preflight request on every run, and a probe file to keep, for
  no engine that needs it yet.

### C. In the harness configuration

- Stop Qwen Code from reading video files.
- Qwen Code has no such setting, and every other agent would need its
  own.

## Decision

A: the proxy replaces every `data:video/` part, on every engine. A list
of frames (`{"type": "video", ...}`), a remote URL and images are left
alone.

## Consequences

- The model reads the note where the video was and can extract frames
  itself. The reviewers of T014 were already extracting
  frames with ffmpeg.
- If an engine Tenaz drives learns to decode video files, option B is
  the way to let it have them.
- Tested with unit tests on request bodies. Not yet seen in a real run.
