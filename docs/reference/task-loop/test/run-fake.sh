#!/bin/sh
# End-to-end test of scripts/task-loop.mjs against the scripted fake Pi.
# Usage: scripts/task-loop/test/run-fake.sh [work-dir]
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
loop="$here/../../task-loop.mjs"
work=${1:-$(mktemp -d)}
mkdir -p "$work"
project="$work/project"
rm -rf "$project"
printf '# Fake brief\n\nBuild four trivial files.\n' > "$work/brief.md"
chmod +x "$here/fake-pi.mjs"

node "$loop" init --project "$project" --brief "$work/brief.md" --model fake/fake --pi "$here/fake-pi.mjs" \
  --set maxAttempts=2 --set replanEvery=2 --set engineRetryBaseSec=0 --set notify=false >/dev/null
# Pause during a T002 builder session (Ctrl-C), then run again to the end.
FAKE_SLOW_TASK=T002 node "$loop" run --project "$project" --pi "$here/fake-pi.mjs" > "$work/run.log" 2>&1 &
pid=$!
until grep -q 'builder-T002 started' "$work/run.log"; do sleep 0.2; done
kill -INT $pid; wait $pid || { cat "$work/run.log"; exit 1; }
grep -q '^PAUSED\|\] PAUSED' "$work/run.log" || { echo "FAIL: run did not pause"; cat "$work/run.log"; exit 1; }
grep -q '^attempts: 0' "$project/.agent/tasks/T002.md" || { echo "FAIL: paused attempt was counted"; exit 1; }
node "$loop" run --project "$project" --pi "$here/fake-pi.mjs" >> "$work/run.log" 2>&1 || { cat "$work/run.log"; exit 1; }
node "$loop" status --project "$project"

fail() { echo "FAIL: $1"; cat "$work/run.log"; exit 1; }
j="$project/.agent/journal.jsonl"
grep -q '"phase": "COMPLETE"' "$project/.agent/run.json" || fail "run did not complete"
grep -q 'steer:loop' "$j" || fail "loop was not steered"
grep -q '"type":"engine-retry"' "$j" || fail "engine error was not retried"
grep -q '"type":"bootstrap-validation","attempt":1,' "$j" || fail "engine error used up a bootstrap attempt"
grep -q '"type":"context-cap".*"engine_max_context":61488,"context_cap":49190' "$j" || fail "engine capacity error did not lower the context budget"
grep -q '"context_budget":49190' "$j" || fail "lowered context budget not applied to the next session"
grep -q 'context budget exceeded' "$j" || fail "session past 1.3x its context budget was not stopped"
grep -q '"type":"context-continuation".*"continuation":1' "$j" || fail "context-stopped session was not continued"
grep -l '^## Continuation' "$project/.agent/private/"*-builder-T001.prompt.md >/dev/null || fail "continuation note missing from the next prompt"
grep '"type":"verify","task":"T001","attempt":1,' "$j" | wc -l | grep -q '^ *1$' || fail "context stop counted as an attempt"
grep -q '"type":"paused","label":"[0-9]*-builder-T002"' "$j" || fail "pause not journaled"
grep -q '"type":"resumed"' "$j" || fail "resume after pause not journaled"
grep -q 'steer:big-output-1' "$j" || fail "large tool output was not steered"
grep -q 'same bash call 5 times' "$j" || fail "loop was not aborted"
grep -q 'steer:repeat' "$j" || fail "alternating loop was not steered"
grep -q 'same result 6 times' "$j" || fail "alternating loop was not aborted"
grep '"type":"task","task":"T004","outcome":"done"' "$j" | grep -q '"attempt":1' || fail "stopped T004 session not recorded on the task"
grep -q '"task":"T004","attempt":1,"review":2,"verdict":"ACCEPT"' "$j" || fail "T004 review without a verdict was not retried"
grep -q '"verdict":"CHANGES"' "$j" || fail "review rejection not recorded"
grep -q '"outcome":"split"' "$j" || fail "T003 was not split"
grep -q '"type":"replan-accepted"' "$j" || fail "no replan"
grep -q 'BRIEF.md' "$j" || fail "BRIEF.md tampering not reverted"
[ "$(cat "$project/BRIEF.md")" != "tampered" ] || fail "BRIEF.md changed"
grep -q '^status: done' "$project/.agent/tasks/T001.md" || fail "T001 card was tampered"
for t in T001 T002 T004 T003a T003b; do grep -q '^status: done' "$project/.agent/tasks/$t.md" || fail "$t not done"; done
grep -q '^status: split' "$project/.agent/tasks/T003.md" || fail "T003 not marked split"
[ ! -e "$project/checks/T003.exists.sh" ] || fail "failed T003 work was not reverted"
echo "FAKE-E2E-OK ($work)"
