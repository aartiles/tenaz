# Reference material (read-only)

Verbatim copies taken from the `local-model-benchmarks` repository at commit
`9a52455` on 2026-10-17. They are the source the runner is ported from and
are kept here so this repository is self-contained. Do not edit them; fix
the port instead.

| Path | What it is |
|---|---|
| `task-loop/task-loop.mjs` | The v0 orchestrator (Pi RPC, one model, sequential roles). The behaviour to carry over is listed in `../SINGLE-MODEL-RUNNER-HANDOFF.md`. |
| `task-loop/TASK-LOOP-ORCHESTRATOR.md` | Its design: phases, `verify` contract, task cards, watchdogs, recovery ladder, engine failures, metrics. |
| `task-loop/templates/AGENTS.md` | Rules shared by every role session; copied into each project by `init`. |
| `task-loop/templates/prompts/*.md` | The six generic role prompts: bootstrap, builder, reviewer, split, replan, final-reviewer. |
| `task-loop/test/fake-pi.mjs`, `run-fake.sh` | The scripted fake Pi and the end-to-end self-test that exercises every orchestrator path without a model. The runner needs an equivalent against a fake harness. |
