#!/usr/bin/env bash
# Idle memory of a tern app bundle with no tab, for the release row in docs/perf.md.
#
#   scripts/measure-idle.sh [path/to/tern.app] [runs]   (default target/release/bundle/tern.app, 3)
#
# Each run copies the bundle, opens it over a new empty TERN_CONFIG_DIR, waits for start-up to
# settle, samples RSS for 20 s (scripts/measure-rss.sh) and reads phys_footprint, then quits it.
# One run can be far off (a just-closed app, first launch of a new signature); the median of
# several is the number to record, with the range beside it.
set -euo pipefail
cd "$(dirname "$0")/.."

bundle=${1:-target/release/bundle/tern.app}
runs=${2:-3}
[ -d "$bundle" ] || { echo "no app at $bundle (scripts/bundle-app.sh builds one)" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

rss=(); foot=()
for ((i = 1; i <= runs; i++)); do
  ditto "$bundle" "$work/$i/tern.app"
  mkdir -p "$work/$i/config"
  open -n --env TERN_CONFIG_DIR="$work/$i/config" "$work/$i/tern.app"
  sleep 8
  pid=$(pgrep -f "$work/$i/tern.app/Contents/MacOS/tern" | head -n 1)
  [ -n "$pid" ] || { echo "run $i: tern did not start" >&2; exit 1; }
  r=$(scripts/measure-rss.sh "$pid" 20 | sed -n 's/^final_rss_mb=\([0-9.]*\).*/\1/p')
  f=$(footprint -p "$pid" 2>/dev/null | sed -n 's/.*phys_footprint: *\([0-9.]*\) MB.*/\1/p' | head -n 1)
  kill "$pid"
  sleep 2
  echo "run $i: ${r} MB RSS, ${f:-?} MB footprint"
  rss+=("$r"); foot+=("${f:-0}")
done

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
range() { printf '%s\n' "$@" | sort -n | awk 'NR==1{lo=$1} {hi=$1} END {print lo "-" hi}'; }
echo "median: $(median "${rss[@]}") MB RSS ($(range "${rss[@]}")), $(median "${foot[@]}") MB footprint ($(range "${foot[@]}"))"
