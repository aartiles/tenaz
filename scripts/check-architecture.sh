#!/usr/bin/env bash
# Structural rules that clippy cannot express. Run by the pre-commit hook and CI.
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0
err() { echo "check-architecture: $*" >&2; fail=1; }

# Dependencies point inwards: tenaz-core depends on no other tenaz crate and
# holds no process, network or HTTP code (that belongs in an adapter behind a port).
if grep -nE '^tenaz-' crates/core/Cargo.toml; then
  err "tenaz-core must not depend on other tenaz crates"
fi
if grep -nE '^(reqwest|hyper|hyper-util|axum|rmcp|agent-client-protocol|landlock)\b' crates/core/Cargo.toml; then
  err "tenaz-core must not depend on network, protocol or OS crates"
fi
if grep -rnE 'Command::new|tokio::process|std::net::|tokio::net::' crates/core/src; then
  err "tenaz-core must not spawn processes or open sockets; add a port and an adapter"
fi

# Tests on the virtual clock (every orchestrator scenario uses it) run under
# paused time (ADR 0006): without it the clock runs in real time and timers
# fire in whatever order the machine allows.
while IFS= read -r f; do
  if grep -nE '^[[:space:]]*#\[tokio::test' "$f" | grep -v 'start_paused = true'; then
    err "$f: use #[tokio::test(start_paused = true)] (ADR 0006)"
  fi
done < <(grep -rlE 'VirtualClock' crates/core/src)

# Only the binary may use anyhow; libraries return thiserror enums.
for toml in crates/*/Cargo.toml; do
  [ "$toml" = crates/cli/Cargo.toml ] && continue
  if sed -n '/^\[dependencies\]/,/^\[/p' "$toml" | grep -qE '^anyhow\b'; then
    err "$toml: anyhow is only for the binary (crates/cli)"
  fi
done

# Every crate inherits the workspace lints and takes its versions from the workspace.
for toml in crates/*/Cargo.toml; do
  if ! grep -qE '^\[lints\]' "$toml" || ! sed -n '/^\[lints\]/,/^\[/p' "$toml" | grep -q 'workspace = true'; then
    err "$toml: needs [lints] workspace = true"
  fi
  if sed -n '/dependencies\]/,/^\[[^d]/p' "$toml" | grep -E '^[a-z0-9_-]+ *= *("|\{ *version)' ; then
    err "$toml: declare dependency versions in the root Cargo.toml and use workspace = true"
  fi
done

exit $fail
