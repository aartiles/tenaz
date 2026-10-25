# ADR 0002: Image engines

Status: accepted, 2026-10-25.

## Context

The agent's only image tool renders through ComfyUI, and only with an
API-format workflow the user exports and passes with `--workflow`. The
wizard finds ComfyUI on port 8188 but cannot find a workflow, so a user
who has ComfyUI running still gets no image tool until they build one.
Nothing else is recognised.

Since then, local servers have converged on one interface: OpenAI's
`POST /v1/images/generations`, which takes a prompt and a size and returns
the image as base64. As of October 2026 it is served by MLX servers on
Apple Silicon (Rapid-MLX, mlxserve, mlx-omni-server, vmlx; an open pull
request adds it to oMLX), by LocalAI and Atomic Chat (stable-diffusion.cpp
or diffusers), and by vLLM-Omni. Several of them serve the image model from
the same process as the LLM. LM Studio and TensorFold do not generate
images as far as their documentation shows.

The lease frees the LLM before each image and loads it again after,
because the two models may not fit in memory together. That is right for a
separate image engine. It is wrong when the LLM's own server renders the
image: unloading the LLM, or stopping the engine with `--engine-stop`,
takes the image model down with it, and the server manages its own
memory anyway.

## Options

### A. Keep ComfyUI only, and ship default workflows

- Removes the need to export a workflow.
- Still needs ComfyUI, the heaviest of the options, and a workflow per
  model family that has to match the checkpoints the user installed.

### B. An adapter per engine

- Each engine's native API, with what it adds (negative prompts, steps).
- Many adapters to write and keep up with, for APIs that change often.

### C. One adapter for the OpenAI Images API, next to ComfyUI

- One adapter covers every server above, and the ones that follow the same
  path. No workflow: the request is the prompt, size and seed.
- Discovery is cheap: the server lists its models at `/v1/models`, the
  same call Tenaz already makes for the LLM.
- Cost: the API has no standard negative prompt, seed, steps or unload.
  Tenaz sends a seed and a negative prompt as extra fields, which servers
  that do not know them ignore, and relies on the server to manage memory.

## Decision

**C.** Tenaz gets an `openai-images` media adapter, configured with
`--images <base URL>` and `--image-model <id>`, as an alternative to
`--comfyui` (a run uses one image engine).

- When the images base URL is the LLM engine's own origin, the lease keeps
  the LLM loaded: the server renders the image and manages its memory. The
  adapter says so (`MediaAdapter::shares_engine`). Otherwise the lease
  frees the LLM as it does for ComfyUI.
- The adapter does not unload anything after a job; the API has no way to.
- Discovery, in `init` and `tenaz doctor`: the LLM engine's own
  `/v1/models` is checked for image models by name (FLUX, SDXL, Stable
  Diffusion 3, Z-Image, Qwen-Image and similar), and known image servers
  are probed on their default ports. What is found is offered, not
  enabled silently.
- ComfyUI stays as it is. Default ComfyUI workflows (option A) can come
  later without changing this decision.

## Consequences

- `run.json` gains `imagesUrl` and `imageModel` under `launch`.
- `generate_image` takes the same arguments for both engines; `workflow`
  only applies to ComfyUI.
- Recognising image models by name is a heuristic: a model with an unusual
  name is not offered, and `--image-model` names it.
- The adapter is tested against a mock server. No real server has been
  run with it yet.
