# ADR 0020: A 3D tool

Status: accepted, 2026-11-03.

## Context

A goal such as "turn this photo of a cabinet into a jointed model" needs a
3D mesh made from an image during the run. The agent has an image tool
(ADR 0002) and nothing for 3D. mlx-serve (26.11.1) serves
`POST /v1/3d/generations`: the body names a `model` and carries an `image`
(base64 PNG or JPEG) plus options of that model (`resolution`, `seed`,
`steps`, `mode` and others), and the answer is
`{"format": "glb", "data": "<base64 GLB>"}`. The models are Idol (it cuts
the object out of the photo itself), TRELLIS.2 and Pixal3D (they need a
cutout whose alpha is the object) and Hunyuan3D. mlx-serve lists them at
`/v1/models` with `"modality": "3d"`.

A call is slow and heavy: on an M5 Mac, Idol takes about 73 s, TRELLIS.2
about 80 s at 512 and 6.7 minutes at 1024, where it needs about 23 GB of
free memory. That is a media job like an image: it must run under the
lease, which frees the LLM first where the engine can, and within the
media time limit.

No other local server offers this API yet, and it is not a standard.

## Options

### A. A ComfyUI workflow for 3D

- Reuses `--comfyui` and `--workflow`.
- ComfyUI's 3D nodes take an image from its own input folder, not from
  the request, and write the mesh to its output folder under a name the
  workflow picks: a different job shape from images. And mlx-serve,
  which serves the models above on this Mac, is not ComfyUI.

### B. Pass 3D through the `--images` server

- One flag fewer.
- The image and 3D models are often on different servers, or only one of
  them is there. One URL for both would make a run take both or neither.

### C. A mesh API with flags of its own, next to the image engine

- `--meshes <base URL>` and `--mesh-model <id>`, pinned in `run.json` as
  `launch.meshesUrl` and `launch.meshModel`. A run may have an image
  engine, a mesh engine, both or neither. Both may be the same server.
- One adapter for the mlx-serve API. Another server with a different API
  gets its own adapter later.

## Decision

**C.**

- The tool is `generate_3d` on the media MCP server. Its arguments:
  - `image`: a PNG or JPEG inside the project (relative to its root, or
    absolute under it). Paths outside the project (also through a
    symbolic link), under the top-level `.git` or `.tenaz`, and files that do not start as a PNG or JPEG are refused
    before the lease is taken: the tool reads the file and sends it to
    the server, and a wrong path should not cost an LLM reload.
  - `output`: where to write the GLB, inside the project, not under
    `.git` or `.tenaz`, and ending in `.glb`. Optional: without it the GLB goes to
    `.tenaz/private/media/<time>-<n>/mesh.glb`, as images go there. The
    agent usually wants the mesh in the project, so the description
    says to give `output`. An existing file is replaced.
  - `resolution`, `seed`, `steps` and `mode`, sent as they are when
    given. Each model takes the ones it knows (`mode` is Idol's
    `synthetic` or `real`; `resolution` is TRELLIS.2's 512, 1024 or
    1536) and the server answers 400 to a value it refuses; the tool
    returns that answer to the agent.
  - Other model options are not exposed. They can be added when a goal
    needs them.
- The GLB is written next to its target and moved in place, so a failed
  write leaves an earlier mesh as it was.
- The run names one model (`--mesh-model`); the agent cannot pick another.
- The answer must be a GLB (`format: "glb"` and data that starts with the
  `glTF` magic), or the call fails and nothing is written.
- Each call takes the lease like an image: the LLM is freed first unless
  the mesh API has the LLM engine's origin (then that server manages its
  memory, as for `--images`) or the engine cannot unload. The call is
  limited by `--media-timeout` (1800 s by default), which also sets the
  agent's MCP call time limit. TRELLIS.2 at 1024 fits in it.
- No limit on the number of calls per run. The lease runs one media job
  at a time, and each call is in the journal (`lease` records for
  `media:mesh-api`) for anyone who wants to count them. The server's
  instructions tell the agent to call once per object.
- `tenaz doctor` recognises 3D models by name, as it does image models:
  mlx-serve also marks them `"modality": "3d"`, but other servers may not,
  and the doctor reads only model ids today. It looks in the engines'
  `/v1/models` (TRELLIS, Idol, Hunyuan3D, Pixal3D) and offers the flags,
  as for images. The setup wizard does not ask for a mesh engine yet:
  `--meshes` and `--mesh-model` are given to `init` or `run`.

## Consequences

- `run.json` gains `launch.meshesUrl` and `launch.meshModel`.
- The media MCP server starts when the run has an image engine or a mesh
  engine, and lists only the tools it can serve.
- Pi gets no media tools (it runs with `--no-mcp`), so it has no 3D tool
  either.
- The adapter is tested against a mock server, and by hand against
  mlx-serve 26.11.1 (see `docs/COMPATIBILITY.md`).
