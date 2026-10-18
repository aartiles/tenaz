# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

### Added

- `smr` binary with `init`, `run`, `status` and `params`.
- Orchestrator ported from the task loop: bootstrap, build, verify, review,
  split or block, replan and final review, with pause and resume.
- Lease manager keeping one engine resident at a time.
- OpenAI-compatible engine proxy that holds requests while engines swap.
- Harness adapters for fx over ACP and Pi over its RPC mode, plus a scripted
  fake harness.
- Engine adapters for Sushi and generic OpenAI-compatible engines; ComfyUI
  media adapter; MCP server exposing `generate_image`.
- CI on macOS and Linux and a tagged release workflow.
