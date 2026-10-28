# ADR 0007: An engine that does not report its prompt cache

Status: accepted, 2026-10-28.

## Context

The preflight asks two questions that share a long prefix, and passes the
cache check when the second answer's usage shows cached tokens. On
2026-10-28 LM Studio 0.4.26 (llama.cpp runtime 2.65.1) with LFM3-1.7B
failed it with `cache=false`, and the run needed `--allow-weak-engine`.
LM Studio's own log shows that the second turn reused the prefix
(`f_sim_best` 0.990) and processed only 661 new tokens out of 2,796. The
`usage` it returns has no `cached_tokens` at all.

Pi fills a missing `cached_tokens` with 0, and so did the proxy, so "not
reported" and "nothing cached" looked the same. `tenaz status` then showed
"prompt cache 0.0%" for every session.

The engine proxy sees each raw response, with Pi and with fx, so it can
tell the two apart. Without the proxy (`--no-proxy`) nothing can.

## Options

### A. Fail, as before

- Blocks an engine that caches, and teaches users to pass
  `--allow-weak-engine`, which also hides real failures.

### B. Pass and warn

- The run starts; the orchestrator log says the cache could not be
  checked, and `run.json` records `cache_reported: false`.
- An engine with no cache at all, and no report, also passes. It costs
  time on every turn, but the run works.

### C. Infer the cache from timings

- With a cache the second prompt's prefill is much shorter than the
  first's. The engine's own `prompt_ms` reaches Tenaz only with fx and
  only from llama.cpp-style engines; LM Studio's response has no
  `timings`. Time to first token is measured for every harness, but the
  first request also pays for warming the model, and a 1.7B model fills
  3,000 tokens in a fraction of a second, so the difference is noise.
- A guess that blocks a run is worse than no check.

## Decision

**B.** The proxy notes, for each response with usage, whether the usage
named its cached tokens (`cached_tokens` in `usage` or its `*_details`).
An engine that named them in no response is "not reported":

- The preflight passes the cache check, keeps `cache: false`, and records
  `cache_reported: false`; the orchestrator log has a warning.
- `tenaz status` shows `cache=not reported` and "prompt cache not
  reported"; `tenaz status --json` has `prompt_cache_reported` and
  `prompt_cache_percent` (`null` when not reported).
- An engine that names them, even as 0, is judged as before. So is every
  engine without the proxy (`cache_reported` is absent).

## Consequences

- LM Studio 0.4 runs without `--allow-weak-engine`.
- An engine without a cache that does not report one is not caught. Its
  turns are slow, and its own log is where to look.
- Only the preflight learns it, so a run whose preflight passed before
  this change (with `--allow-weak-engine`) keeps "prompt cache 0.0%" in
  `tenaz status`.
- Only the usage counts: an engine that names cached tokens only in
  llama.cpp's `timings` (`cached_n`) is judged by its usage, as the
  harness is.
- Not yet run against LM Studio: tested with recorded usage shapes and
  the fake harness.
