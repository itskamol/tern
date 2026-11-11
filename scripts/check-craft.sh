#!/usr/bin/env bash
# Mechanical craft gates that clippy cannot express.
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0

# 1. No god-files: production .rs files stay under 800 lines.
while read -r n f; do
  [[ "$f" == "total" ]] && continue
  if (( n > 800 )); then echo "file too long ($n > 800): $f"; fail=1; fi
done < <(find crates -name '*.rs' -not -path '*/tests/*' -not -name '*_tests.rs' -print0 | xargs -0 wc -l)

# 2. Every file with copied code names its origin.
for f in $(grep -rl 'Adapted from' crates --include='*.rs' || true); do
  # The header may wrap: the license must appear within the first 5 lines.
  head -5 "$f" | tr '\n' ' ' | grep -q 'Adapted from .*(\(GPL\|MIT\|Apache\)' || { echo "bad attribution header: $f"; fail=1; }
done

# 3. Every crate opts into the workspace lints.
for c in crates/*/Cargo.toml; do
  grep -q '^\[lints\]' "$c" && grep -A1 '^\[lints\]' "$c" | grep -q 'workspace = true' \
    || { echo "missing [lints] workspace = true: $c"; fail=1; }
done

# 4. The workspace's own crates are pinned at the workspace version: a release bump that misses
#    one fails in cargo far from the cause.
ws=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
while read -r name pin; do
  [ "$pin" = "$ws" ] || { echo "$name is pinned at $pin, the workspace is at $ws (Cargo.toml)"; fail=1; }
done < <(sed -n 's/^\(tern-[a-z]*\) = { path = "[^"]*", version = "\([^"]*\)" }/\1 \2/p' Cargo.toml)

exit $fail
