#!/usr/bin/env bash
# The test gate: the whole suite with its output kept and a time limit, so a test that hangs is
# stopped and named, and a test that fails is named without a second run.
#
#   scripts/test-gate.sh                 the workspace's tests
#   TERN_TEST_LIMIT=300 scripts/...      stop the run after 300 seconds (default 600)
#
# The build is not timed, only the run. The full output is in target/test-gate.log.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p target
log=target/test-gate.log
limit=${TERN_TEST_LIMIT:-600}
late=target/test-gate.late
rm -f "$late"

cargo test -q --workspace --no-run 2>/dev/null

# Each job gets its own process group, so the limit stops cargo and every test binary under it.
set -m
cargo test --workspace >"$log" 2>&1 &
run=$!
( sleep "$limit" && : > "$late" && kill -TERM -- -"$run" ) 2>/dev/null &
watch=$!
rc=0
wait "$run" 2>/dev/null || rc=$?
kill -TERM -- -"$watch" 2>/dev/null || true
wait "$watch" 2>/dev/null || true

if [ -e "$late" ]; then
  rm -f "$late"
  echo "tests: stopped after ${limit}s ($log). Still running then:"
  sed -n 's/^test \(.*\) has been running for over .*/  \1/p' "$log" | sort -u
  exit 1
fi
if [ "$rc" != 0 ]; then
  echo "tests: failed ($log):"
  grep -E '^test .* \.\.\. FAILED$|^error' "$log" | sed 's/^/  /' || true
  exit 1
fi
